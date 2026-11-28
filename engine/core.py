"""Durable event and copy index. No camera or cloud credentials in this database."""
import contextlib
import hashlib
import json
import sqlite3
import time
from pathlib import Path


class Index:
    def __init__(self, path):
        self.path = str(path)
        with self.connect() as db:
            db.executescript('''
              PRAGMA journal_mode=WAL;
              CREATE TABLE IF NOT EXISTS events(camera TEXT, at REAL, kind TEXT, score REAL);
              CREATE INDEX IF NOT EXISTS event_time ON events(camera,at);
              CREATE TABLE IF NOT EXISTS clips(path TEXT PRIMARY KEY, camera TEXT, at REAL,
                 duration REAL, size INTEGER, sha256 TEXT, deleted_local INTEGER DEFAULT 0);
              CREATE TABLE IF NOT EXISTS copies(path TEXT, source TEXT, target TEXT,
                 state TEXT DEFAULT 'pending', attempts INTEGER DEFAULT 0,
                 retry REAL DEFAULT 0, confirmed REAL, error TEXT DEFAULT '',
                 PRIMARY KEY(path,source,target));
              CREATE INDEX IF NOT EXISTS copy_pending ON copies(source,target,state,retry);
              CREATE TABLE IF NOT EXISTS segments(path TEXT PRIMARY KEY, camera TEXT,
                 start REAL, end REAL, state TEXT DEFAULT 'buffer', clip TEXT);
            ''')
            db.execute("UPDATE copies SET state='pending' WHERE state='uploading'")

    @contextlib.contextmanager
    def connect(self):
        db = sqlite3.connect(self.path, timeout=20)
        db.row_factory = sqlite3.Row
        try:
            yield db
            db.commit()
        except BaseException:
            db.rollback()
            raise
        finally:
            db.close()

    def event(self, camera, at, kind, score):
        with self.connect() as db:
            db.execute('INSERT INTO events VALUES(?,?,?,?)', (camera, at, kind, score))

    def active(self, camera, start, end, pre=3, post=5, kinds=None):
        kinds=('motion','crying') if kinds is None else tuple(kinds)
        if not kinds:return False
        with self.connect() as db:
            return db.execute('SELECT 1 FROM events WHERE camera=? AND at>=? AND at<=? AND kind IN ('+
                              ','.join('?' for _ in kinds)+') LIMIT 1',
                              (camera,start-post,end+pre,*kinds)).fetchone() is not None

    def register(self, file, root, camera, at, duration):
        relative = file.relative_to(root).as_posix()
        digest = hashlib.sha256()
        with file.open('rb') as source:
            for block in iter(lambda: source.read(1024*1024), b''):
                digest.update(block)
        with self.connect() as db:
            db.execute('INSERT OR IGNORE INTO clips(path,camera,at,duration,size,sha256) VALUES(?,?,?,?,?,?)',
                       (relative, camera, at, duration, file.stat().st_size, digest.hexdigest()))
        return relative

    def enqueue(self, source, target, cutoff=0):
        with self.connect() as db:
            db.execute('''INSERT OR IGNORE INTO copies(path,source,target)
              SELECT path,?,? FROM clips WHERE at>=?''', (source, target, cutoff))

    def next_copy(self, source, target, r2_first=None):
        with self.connect() as db:
            db.execute("BEGIN IMMEDIATE")
            gate=" AND EXISTS (SELECT 1 FROM copies r WHERE r.path=q.path AND r.source='r2' AND r.target=? AND r.state='done')" if r2_first else ""
            params=[source,target,time.time()]+([r2_first] if r2_first else [])
            row = db.execute("""SELECT c.*,q.attempts FROM copies q JOIN clips c USING(path)
              WHERE q.source=? AND q.target=? AND q.state IN ('pending','error') AND q.retry<=?"""+gate+
              " ORDER BY c.at LIMIT 1",params).fetchone()
            if row:
                db.execute("UPDATE copies SET state='uploading' WHERE path=? AND source=? AND target=?",
                           (row['path'], source, target))
            return dict(row) if row else None

    def complete(self, path, source, target):
        with self.connect() as db:
            db.execute("UPDATE copies SET state='done',confirmed=?,error='' WHERE path=? AND source=? AND target=?",
                       (time.time(), path, source, target))

    def fail(self, path, source, target, attempts, message):
        delay = min(3600, 15 * 2**min(attempts, 8))
        with self.connect() as db:
            db.execute("UPDATE copies SET state='error',attempts=?,retry=?,error=? WHERE path=? AND source=? AND target=?",
                       (attempts+1, time.time()+delay, message, path, source, target))

    def confirmed(self, path, targets):
        if not targets:
            return False
        with self.connect() as db:
            return all(db.execute("SELECT 1 FROM copies WHERE path=? AND source=? AND target=? AND state='done'",
                                  (path, source, target)).fetchone() for source, target in targets)

    def catalog(self, source, target):
        with self.connect() as db:
            return {r['path']: r['size'] for r in db.execute('''SELECT c.path,c.size FROM clips c
              JOIN copies q USING(path) WHERE q.source=? AND q.target=? AND q.state='done' ''', (source, target))}


def destination_targets(data):
    """Return enabled destinations, never interpreting a missing config as ready."""
    p = read_json(data/'cloud-policy.json', {})
    if not p.get('backup_enabled', True):
        return p, {}
    targets = {}
    if p.get('drive_enabled', True) and (data/'drive.conf').is_file() and (data/'drive-folder').is_file():
        folder = (data/'drive-folder').read_text().strip()
        targets['drive'] = {'target': f'drive:{folder}/recordings', 'config': str(data/'drive.conf'),
                            'days': p.get('drive_days', 0), 'retention_seconds': int(p.get('drive_days',0))*86400, 'env': {}}
    r2 = read_json(data/'r2.json', {})
    if r2 and p.get('r2_enabled', r2.get('enabled', False)):
        targets['r2'] = {'target': f"r2:{r2['bucket']}/{r2['prefix']}/recordings", 'config': '/dev/null',
                         'days': r2['days'], 'retention_seconds': retention_seconds(p,'r2',r2['days']), 'env': {
            'RCLONE_CONFIG_R2_TYPE': 's3', 'RCLONE_CONFIG_R2_PROVIDER': 'Cloudflare',
            'RCLONE_CONFIG_R2_ACCESS_KEY_ID': r2['access_key'],
            'RCLONE_CONFIG_R2_SECRET_ACCESS_KEY': r2['secret_key'],
            'RCLONE_CONFIG_R2_ENDPOINT': f"https://{r2['account_id']}.r2.cloudflarestorage.com",
            'RCLONE_CONFIG_R2_REGION': 'auto', 'RCLONE_CONFIG_R2_NO_CHECK_BUCKET': 'true'}}
    return p, targets


def read_json(path, default):
    try:
        return json.loads(path.read_text())
    except FileNotFoundError:
        return default


def selected_mode(camera):
    motion = camera.get('record', {}).get('motion', {}).get('days', 0) > 0
    cry = camera.get('audio', {}).get('enabled', False)
    return motion, cry


def eligible_local_delete(row, policy, targets, index, modified):
    if not policy.get('backup_enabled', True):
        return False
    r2=[target for target in targets if target[0]=='r2']
    if not policy.get('local_enabled', True) and r2:
        return index.confirmed(row['path'],r2)
    if not index.confirmed(row['path'],targets):
        return False
    staged = (not policy.get('local_enabled', True) and policy.get('local_off_since') is not None
              and modified * 1000 >= policy['local_off_since'])
    seconds=retention_seconds(policy,'local',policy.get('local_days',0))
    return staged or (seconds>0 and row['at']<time.time()-seconds)


def remove_local_after_r2(index, row, root, policy, targets):
    r2=targets.get('r2')
    if (not policy.get('backup_enabled',True) or policy.get('local_enabled',True)
            or not r2 or not index.confirmed(row['path'],[('r2',r2['target'])])):
        return False
    file=root/row['path']
    if file.is_symlink() or not file.is_file() or file.stat().st_size!=row['size']:
        return False
    file.unlink()
    with index.connect() as db:
        db.execute('UPDATE clips SET deleted_local=1 WHERE path=?',(row['path'],))
    return True


def retention_seconds(policy,source,legacy_days):
    value=policy.get(source+'_retention_seconds')
    return int(legacy_days)*86400 if value is None else max(0,int(value))
