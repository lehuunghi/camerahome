"""CameraHome native capture. Private internal API, never publish port 5000/1984."""
import asyncio
import contextlib
import csv
import datetime as dt
import hashlib
import json
import os
import shutil
import signal
import time
import uuid
from pathlib import Path

import aiohttp
from aiohttp import web
import cv2
import numpy as np
import yaml
from vision import MotionDetector
from core import Index, destination_targets, read_json, selected_mode, eligible_local_delete, remove_local_after_r2

DATA = Path(os.getenv('DATA_DIR', '/app-data'))
ROOT = Path(os.getenv('RECORDINGS_DIR', '/recordings'))
SPOOL = Path(os.getenv('SPOOL_DIR', '/spool'))
CONFIG = Path(os.getenv('CAMERA_CONFIG', '/config/config.yml'))
GO = os.getenv('GO2RTC_URL', 'http://go2rtc:1984')
RTSP = os.getenv('GO2RTC_RTSP', 'rtsp://go2rtc:8554')
MODEL = Path(os.getenv('CRY_MODEL', '/models/yamnet.tflite'))
ID = __import__('re').compile(r'[A-Za-z0-9_-]{1,100}')
STATUS = {}
TASKS = {}
INDEX = None
MODEL_OK = False
EXPORT_LOCK = asyncio.Lock()
CONFIG_LOCK = asyncio.Lock()


def config():
    return yaml.safe_load(CONFIG.read_text())


def paused():
    return (DATA/'purge-request.json').exists() or read_json(DATA/'purge-status.json', {}).get('state') in ('pending', 'running')


def can_record():
    try:
        policy, targets = destination_targets(DATA)
        return not paused() and policy.get('backup_enabled', True) and (policy.get('local_enabled', True) or bool(targets))
    except (ValueError, OSError, KeyError):
        return False


def ffmpeg(source):
    return ['ffmpeg', '-hide_banner', '-loglevel', 'error', '-nostdin', '-threads', '1',
            '-rtsp_transport', 'tcp', '-timeout', '15000000', '-i', f'{RTSP}/{source}']


async def spawn(args, pipe=False):
    return await asyncio.create_subprocess_exec(*args, stdout=asyncio.subprocess.PIPE if pipe else asyncio.subprocess.DEVNULL,
                                                stderr=asyncio.subprocess.DEVNULL)


async def stop(proc, drain=False):
    # A stopped raw-video producer can still have a full stdout pipe. Drain it
    # while waiting, otherwise asyncio's process transport never finishes closing.
    if proc.returncode is None:
        with contextlib.suppress(ProcessLookupError): proc.terminate()
    try:
        await asyncio.wait_for(proc.communicate() if drain else proc.wait(),5)
    except asyncio.TimeoutError:
        if proc.returncode is None:
            with contextlib.suppress(ProcessLookupError): proc.kill()
        with contextlib.suppress(asyncio.TimeoutError):
            await asyncio.wait_for(proc.communicate() if drain else proc.wait(),3)


def hit(camera, kind, score):
    at = time.time()
    INDEX.event(camera, at, kind, float(score))
    STATUS[camera]['last_'+kind] = at


async def motion(camera):
    proc = await spawn(ffmpeg(camera+'__compact')+['-an', '-vf', 'fps=3,scale=320:180', '-pix_fmt', 'bgr24', '-f', 'rawvideo', 'pipe:1'], True)
    detector=MotionDetector()
    try:
        while True:
            raw=await asyncio.wait_for(proc.stdout.readexactly(320*180*3),20)
            frame=np.frombuffer(raw,np.uint8).reshape(180,320,3)
            STATUS[camera]['last_frame']=time.time()
            STATUS[camera]['frame']=frame.copy()
            triggered,score=detector.feed(frame)
            if triggered and selected_mode(config()['cameras'][camera])[0] and can_record():
                hit(camera,'motion',score)
    finally:
        await stop(proc,drain=True)


class CryModel:
    def __init__(self):
        from tflite_runtime.interpreter import Interpreter
        self.model = Interpreter(model_path=str(MODEL), num_threads=1)
        initial=self.model.get_input_details()[0]
        if initial.get('shape_signature',np.array([])).tolist()==[-1]:
            self.model.resize_tensor_input(initial['index'],[15600])
        self.model.allocate_tensors()
        self.input = self.model.get_input_details()[0]
        self.output = next(x for x in self.model.get_output_details() if x['shape'][-1] == 521)
        self.samples = int(np.prod(self.input['shape']))
        if self.input['dtype'] != np.float32 or self.samples < 15000:
            raise ValueError('unsupported cry model')
        labels = list(csv.DictReader((MODEL.parent/'yamnet_class_map.csv').open()))
        self.cry = next(int(x['index']) for x in labels if x['mid'] == '/t/dd00002')

    def score(self, audio):
        self.model.set_tensor(self.input['index'], audio.reshape(self.input['shape']))
        self.model.invoke()
        return float(np.max(self.model.get_tensor(self.output['index'])[..., self.cry]))


async def crying(camera):
    model = await asyncio.to_thread(CryModel)
    proc = await spawn(ffmpeg(camera)+['-vn', '-ac', '1', '-ar', '16000', '-f', 's16le', 'pipe:1'], True)
    consecutive = 0
    try:
        while True:
            raw = await asyncio.wait_for(proc.stdout.readexactly(model.samples*2), 20)
            audio = np.frombuffer(raw, '<i2').astype(np.float32)/32768
            STATUS[camera]['last_audio'] = time.time()
            # Feed original audio to the classifier; listening denoise is separate.
            score = await asyncio.to_thread(model.score, audio) if np.sqrt(np.mean(audio**2)) > .002 else 0
            STATUS[camera]['cry_score'] = score
            consecutive = consecutive+1 if score >= .35 else 0
            if consecutive >= 2 and can_record():
                hit(camera, 'crying', score)
                consecutive = 0
    finally:
        await stop(proc,drain=True)


async def export_segments(camera, rows):
    if not rows or paused():
        return
    async with EXPORT_LOCK:
        first = dt.datetime.fromtimestamp(rows[0]['start'], dt.timezone.utc)
        output = ROOT/first.strftime('%Y-%m-%d/%H')/camera/first.strftime('%M.%S.mp4')
        output.parent.mkdir(parents=True, exist_ok=True)
        playlist = SPOOL/(uuid.uuid4().hex+'.concat')
        temporary = output.with_suffix('.working')
        playlist.write_text(''.join(f"file '{r['path']}'\n" for r in rows))
        try:
            proc = await spawn(['ffmpeg', '-hide_banner', '-loglevel', 'error', '-nostdin', '-y',
                '-f', 'concat', '-safe', '0', '-i', str(playlist), '-map', '0:v:0', '-map', '0:a:0?',
                '-c:v', 'copy', '-c:a', 'copy', '-movflags', '+faststart',
                '-f', 'mp4', str(temporary)])
            try:
                code = await asyncio.wait_for(proc.wait(), 60)
            finally:
                await stop(proc)
            if code or not temporary.exists() or temporary.stat().st_size == 0:
                raise RuntimeError('clip export failed')
            # Do not overwrite an existing archive at a clock collision.
            if output.exists():
                temporary.unlink()
            else:
                temporary.replace(output)
            duration = sum(r['end']-r['start'] for r in rows)
            relative = await asyncio.to_thread(INDEX.register, output, ROOT, camera, rows[0]['start'], duration)
            with INDEX.connect() as db:
                db.executemany("UPDATE segments SET state='done',clip=? WHERE path=?", [(relative,r['path']) for r in rows])
        finally:
            playlist.unlink(missing_ok=True)
            temporary.unlink(missing_ok=True)


async def capture(camera):
    directory = SPOOL/camera/uuid.uuid4().hex
    directory.mkdir(parents=True)
    began = time.time()
    (directory/'session.json').write_text(json.dumps({'camera': camera, 'at': began}))
    proc = await spawn(ffmpeg(camera)+['-map','0:v:0','-map','0:a:0?','-c:v','copy','-c:a','aac','-b:a','48k','-ac','1',
        '-f','segment','-segment_time','5','-segment_format','mpegts','-reset_timestamps','1',
        '-segment_list',str(directory/'segments.csv'),str(directory/'%09d.ts')])
    try:
        while proc.returncode is None:
            if not can_record() or shutil.disk_usage(SPOOL).free < 2*1024**3:
                STATUS[camera]['record_status'] = 'paused' if not can_record() else 'disk_full'
                return
            STATUS[camera]['record_status'] = 'buffering'
            await asyncio.sleep(1)
    finally:
        await stop(proc)


async def supervise(camera, worker):
    delay = 2
    while True:
        try:
            if worker == capture and not can_record():
                STATUS[camera]['record_status'] = 'paused'
                await asyncio.sleep(2)
                continue
            await worker(camera)
        except asyncio.CancelledError:
            raise
        except Exception:
            STATUS[camera][worker.__name__+'_error'] = int(time.time())
        await asyncio.sleep(delay)
        delay = min(30, delay*2)


async def collect():
    """CSV contains only closed TS segments. Recover these sessions after a restart."""
    while True:
        try:
            now = time.time()
            for file in SPOOL.glob('*/*/segments.csv'):
                session = read_json(file.parent/'session.json', {})
                camera = session.get('camera')
                if not camera or not ID.fullmatch(camera):
                    continue
                for row in list(csv.reader(file.open())):
                    if len(row) != 3:
                        continue
                    path = Path(row[0])
                    if not path.is_absolute(): path = file.parent/path
                    if path.parent != file.parent or not path.exists(): continue
                    start, end = session['at']+float(row[1]), session['at']+float(row[2])
                    with INDEX.connect() as db:
                        db.execute('INSERT OR IGNORE INTO segments(path,camera,start,end) VALUES(?,?,?,?)', (str(path),camera,start,end))
            with INDEX.connect() as db:
                cameras = [r[0] for r in db.execute("SELECT DISTINCT camera FROM segments WHERE state IN ('buffer','selected')")]
            for camera in cameras:
                with INDEX.connect() as db:
                    rows = db.execute("SELECT * FROM segments WHERE camera=? AND state='buffer' AND end<? ORDER BY start", (camera,now-10)).fetchall()
                    for row in rows:
                        if INDEX.active(camera,row['start'],row['end']):
                            db.execute("UPDATE segments SET state='selected' WHERE path=?",(row['path'],))
                        else:
                            Path(row['path']).unlink(missing_ok=True)
                            db.execute("DELETE FROM segments WHERE path=?",(row['path'],))
                    selected = [dict(r) for r in db.execute("SELECT * FROM segments WHERE camera=? AND state='selected' ORDER BY start", (camera,))]
                batch=[]
                for row in selected:
                    if batch and (Path(row['path']).parent!=Path(batch[-1]['path']).parent or row['start']-batch[-1]['end']>2 or row['end']-batch[0]['start']>178):
                        await export_segments(camera,batch);batch=[]
                    batch.append(row)
                if batch and now-batch[-1]['end']>25:
                    await export_segments(camera,batch)
            with INDEX.connect() as db:
                for row in db.execute("SELECT path FROM segments WHERE state='done'").fetchall():
                    Path(row['path']).unlink(missing_ok=True)
                    db.execute('DELETE FROM segments WHERE path=?',(row['path'],))
                db.execute('DELETE FROM events WHERE at<?',(now-86400*7,))
            # Remove only empty completed session dirs, never erase pending segments.
            for f in SPOOL.glob('*/*/session.json'):
                if now-f.stat().st_mtime>120 and not list(f.parent.glob('*.ts')):
                    (f.parent/'segments.csv').unlink(missing_ok=True);f.unlink();f.parent.rmdir()
        except asyncio.CancelledError:
            raise
        except Exception:
            print('collector: retrying pending segments',flush=True)
        await asyncio.sleep(2)


def cloud_command(remote, args):
    cmd=['rclone','--config',remote['config'],'--contimeout','10s','--timeout','60s','--retries','1','--low-level-retries','3']
    if remote['target'].startswith('drive:'):
        cmd += ['--tpslimit','1','--tpslimit-burst','1','--drive-pacer-min-sleep','1s','--drive-pacer-burst','1']
    return cmd+args


async def cloud(remote,args,source):
    proc=await asyncio.create_subprocess_exec(*cloud_command(remote,args),env={**os.environ,**remote['env']},
                                             stdout=asyncio.subprocess.PIPE,stderr=asyncio.subprocess.DEVNULL)
    work=asyncio.create_task(proc.communicate())
    started=time.monotonic()
    try:
        while not work.done():
            await asyncio.sleep(.5)
            _,targets=destination_targets(DATA)
            if paused() or source not in targets or targets[source]['target']!=remote['target'] or time.monotonic()-started>300:
                raise RuntimeError('backup paused or timed out')
        result,_=await work
        if proc.returncode: raise RuntimeError('cloud request failed')
        return result
    finally:
        await stop(proc)
        if not work.done():
            work.cancel()
            with contextlib.suppress(asyncio.CancelledError): await work


async def transfer(source,row,remote,targets):
    file=ROOT/row['path']
    download=None
    try:
        if not file.exists():
            r2=targets.get('r2')
            if source!='drive' or not r2 or not INDEX.confirmed(row['path'], [('r2',r2['target'])]):
                raise RuntimeError('original unavailable; preserving queue')
            download=SPOOL/(uuid.uuid4().hex+'.mp4')
            await cloud(r2,['copyto',r2['target']+'/'+row['path'],str(download)],'r2')
            file=download
        def sha256():
            with file.open('rb') as f: return hashlib.file_digest(f,'sha256').hexdigest()
        digest=await asyncio.to_thread(sha256)
        if file.stat().st_size!=row['size'] or digest!=row['sha256']: raise RuntimeError('original verification failed')
        target=remote['target']+'/'+row['path']
        await cloud(remote,['copyto',str(file),target,'--checksum'],source)
        stat=json.loads(await cloud(remote,['lsjson',target,'--stat','--hash'],source))
        hashes={k.lower():v.lower() for k,v in stat.get('Hashes',{}).items() if isinstance(v,str)}
        if stat.get('Size')!=row['size']: raise RuntimeError('cloud size verification failed')
        # Verify remote MD5 when the backend exposes it (Drive and single-part S3).
        if hashes.get('md5'):
            def md5():
                with file.open('rb') as f: return hashlib.file_digest(f,'md5').hexdigest()
            if hashes['md5']!=await asyncio.to_thread(md5): raise RuntimeError('cloud checksum verification failed')
        INDEX.complete(row['path'],source,remote['target'])
        if source=='r2':
            policy,current=destination_targets(DATA)
            if remove_local_after_r2(INDEX,row,ROOT,policy,current):
                (DATA/'local-retention-status').write_text(f'R2 đã xác nhận; đã xóa video tạm trên máy chủ. Drive sao lưu từ R2.|{int(time.time())}')
    finally:
        if download: download.unlink(missing_ok=True)


async def uploader(source):
    while True:
        row=None
        try:
            p,targets=destination_targets(DATA)
            if source in targets and not paused():
                remote=targets[source]
                cutoff=time.time()-remote['retention_seconds'] if remote['retention_seconds'] else 0
                INDEX.enqueue(source,remote['target'],cutoff)
                row=INDEX.next_copy(source,remote['target'],targets['r2']['target'] if source=='drive' and 'r2' in targets else None)
                if row:
                    await transfer(source,row,remote,targets)
                    (DATA/(source+'-status')).write_text(f'Đã xác nhận sao lưu video mới|{int(time.time())}')
        except asyncio.CancelledError:
            raise
        except Exception:
            if row:
                INDEX.fail(row['path'],source,remote['target'],row['attempts'],'Chưa xác nhận sao lưu; sẽ thử lại.')
                (DATA/(source+'-status')).write_text(f'Chưa xác nhận sao lưu; giữ video và tự thử lại.|{int(time.time())}')
        await asyncio.sleep(1 if row else 3)


async def retention():
    while True:
        try:
            p,targets=destination_targets(DATA)
            if not paused() and p.get('backup_enabled',True):
                with INDEX.connect() as db: rows=[dict(r) for r in db.execute('SELECT * FROM clips')]
                for row in rows:
                    file=ROOT/row['path']
                    p,latest=destination_targets(DATA)
                    if paused() or not p.get('backup_enabled',True): break
                    if {s:r['target'] for s,r in latest.items()}!={s:r['target'] for s,r in targets.items()}: break
                    if file.is_file() and eligible_local_delete(row,p,[(s,r['target']) for s,r in targets.items()],INDEX,file.stat().st_mtime):
                        file.unlink()
                        with INDEX.connect() as db: db.execute('UPDATE clips SET deleted_local=1 WHERE path=?',(row['path'],))
                    for source,r in targets.items():
                        if not r['retention_seconds'] or row['at']>=time.time()-r['retention_seconds']: continue
                        # R2 is the fallback original: do not expire it before Drive confirms.
                        if source=='r2' and 'drive' in targets and not INDEX.confirmed(row['path'],[('drive',targets['drive']['target'])]): continue
                        if not INDEX.confirmed(row['path'],[(source,r['target'])]): continue
                        await cloud(r,['deletefile',r['target']+'/'+row['path']],source)
                        with INDEX.connect() as db:
                            db.execute("UPDATE copies SET state='expired' WHERE path=? AND source=? AND target=?",(row['path'],source,r['target']))
        except asyncio.CancelledError:
            raise
        except Exception:
            print('retention: keeping unconfirmed copies',flush=True)
        await asyncio.sleep(60)


async def apply_streams(cfg):
    streams=cfg.get('go2rtc',{}).get('streams',{})
    previous=config().get('go2rtc',{}).get('streams',{})
    async with aiohttp.ClientSession(timeout=aiohttp.ClientTimeout(total=5)) as client:
        existing=await client.get(GO+'/api/streams')
        names=await existing.json()
        for name in names:
            if name not in streams:
                async with client.delete(GO+'/api/streams',params={'src':name}) as r: await r.read()
        for name,urls in streams.items():
            if previous.get(name)==urls: continue
            async with client.put(GO+'/api/streams',params={'name':name,'src':urls if isinstance(urls,str) else urls}) as r:
                if r.status>=400: raise RuntimeError('stream configuration rejected')
                await r.read()


async def restart_cameras(cfg):
    for task in TASKS.values(): task.cancel()
    await asyncio.gather(*TASKS.values(),return_exceptions=True)
    TASKS.clear();STATUS.clear()
    for camera,c in cfg.get('cameras',{}).items():
        if not ID.fullmatch(camera): raise ValueError('invalid camera')
        STATUS[camera]={'last_frame':0,'last_audio':0,'record_status':'starting'}
        TASKS[(camera,'motion')]=asyncio.create_task(supervise(camera,motion))
        TASKS[(camera,'capture')]=asyncio.create_task(supervise(camera,capture))
        if selected_mode(c)[1]: TASKS[(camera,'crying')]=asyncio.create_task(supervise(camera,crying))


async def stats(request):
    with INDEX.connect() as db:
        queue=[dict(r) for r in db.execute('SELECT source,state,count(*) AS count FROM copies GROUP BY source,state')]
    return web.json_response({'engine':'camerahome-native','cry_model_ready':MODEL_OK,'queue':queue,
        'sensors':[{'camera':key[0],'kind':key[1],'running':not task.done()} for key,task in TASKS.items()],
        'cameras':{k:{'camera_fps':3 if time.time()-v.get('last_frame',0)<10 else 0,
                     **{x:y for x,y in v.items() if x!='frame'}} for k,v in STATUS.items()}})


async def save_config(request):
    async with CONFIG_LOCK:
        cfg=yaml.safe_load(await request.text())
        if not isinstance(cfg,dict) or not isinstance(cfg.get('cameras'),dict): raise web.HTTPBadRequest()
        for name in cfg['cameras']:
            if not ID.fullmatch(name): raise web.HTTPBadRequest()
        await apply_streams(cfg)
        tmp=CONFIG.with_suffix('.native-new');tmp.write_text(yaml.safe_dump(cfg));tmp.chmod(0o600);tmp.replace(CONFIG)
        render_go(cfg)
        await restart_cameras(cfg)
        return web.json_response({'success':True})


async def snapshot(request):
    state=STATUS.get(request.match_info['camera'],{})
    frame=state.get('frame') if time.time()-state.get('last_frame',0)<20 else None
    if frame is None: raise web.HTTPServiceUnavailable()
    ok,data=cv2.imencode('.jpg',frame)
    if not ok: raise web.HTTPServiceUnavailable()
    return web.Response(body=data.tobytes(),content_type='image/jpeg')


async def catalog(request):
    source=request.query.get('source','');target=request.query.get('target','')
    files=INDEX.catalog(source,target)
    with INDEX.connect() as db:
        expired=[r[0] for r in db.execute("SELECT path FROM copies WHERE source=? AND target=? AND state='expired'",(source,target))]
    return web.json_response({'target':target,'files':files,'expired':expired,'days':[],'complete':False})


PTZ={}
def ptz_services(camera):
    from onvif import ONVIFCamera
    if camera not in PTZ:
        c=config()['cameras'][camera]['onvif']
        device=ONVIFCamera(c['host'],c.get('port',2020),c['user'],c['password'])
        media=device.create_media_service();service=device.create_ptz_service()
        profile=media.GetProfiles()[0]
        if not profile.PTZConfiguration: raise ValueError('no pan tilt')
        PTZ[camera]=(service,profile.token)
    return PTZ[camera]


async def ptz_info(request):
    try:
        await asyncio.wait_for(asyncio.to_thread(ptz_services,request.match_info['camera']),4)
        return web.json_response({'features':['pt']})
    except Exception:
        return web.json_response({'features':[]})


async def ptz_ws(request):
    ws=web.WebSocketResponse();await ws.prepare(request)
    moving=set()
    async def move(camera,payload):
        service,token=await asyncio.to_thread(ptz_services,camera)
        if payload=='STOP':
            await asyncio.to_thread(service.Stop,{'ProfileToken':token,'PanTilt':True});moving.discard(camera)
        else:
            vectors={'MOVE_UP':(0,.4),'MOVE_DOWN':(0,-.4),'MOVE_LEFT':(-.4,0),'MOVE_RIGHT':(.4,0)}
            x,y=vectors[payload];moving.add(camera)
            await asyncio.to_thread(service.ContinuousMove,{'ProfileToken':token,'Velocity':{'PanTilt':{'x':x,'y':y}},'Timeout':'PT1S'})
    try:
        async for message in ws:
            if message.type!=aiohttp.WSMsgType.TEXT: continue
            data=json.loads(message.data);camera,topic=data['topic'].split('/')
            if camera not in STATUS or topic!='ptz': continue
            await asyncio.wait_for(move(camera,data['payload']),4)
    finally:
        for camera in list(moving):
            with contextlib.suppress(Exception): await move(camera,'STOP')
    return ws


class IndentedDumper(yaml.SafeDumper):
    def increase_indent(self,flow=False,indentless=False):
        return super().increase_indent(flow,False)


def render_go(cfg):
    path=DATA/'native'/'go2rtc.yaml';path.parent.mkdir(exist_ok=True)
    streams=cfg.get('go2rtc',{}).get('streams',{})
    path.write_text(yaml.dump({'api':{'listen':':1984'},'rtsp':{'listen':':8554'},'streams':streams},Dumper=IndentedDumper,sort_keys=False))
    path.chmod(0o600)


async def import_archives():
    for file in ROOT.glob('*/*/*/*.mp4'):
        try:
            day,hour,camera,name=file.relative_to(ROOT).parts
            at=dt.datetime.strptime(day+' '+hour+'.'+name[:-4],'%Y-%m-%d %H.%M.%S').replace(tzinfo=dt.timezone.utc).timestamp()
            with INDEX.connect() as db: known=db.execute('SELECT 1 FROM clips WHERE path=?',(file.relative_to(ROOT).as_posix(),)).fetchone()
            if not known: await asyncio.to_thread(INDEX.register,file,ROOT,camera,at,0)
        except (ValueError,OSError): continue


async def startup(app):
    global INDEX,MODEL_OK
    DATA.mkdir(parents=True,exist_ok=True);SPOOL.mkdir(parents=True,exist_ok=True);ROOT.mkdir(parents=True,exist_ok=True)
    INDEX=Index(DATA/'native-index.sqlite')
    try: CryModel();MODEL_OK=True
    except Exception: print('cry model unavailable; cry health is not ready',flush=True)
    await restart_cameras(config())
    app['workers']=[asyncio.create_task(f()) for f in [collect,retention,import_archives]]+[asyncio.create_task(uploader(s)) for s in ['r2','drive']]


async def cleanup(app):
    for task in [*TASKS.values(),*app['workers']]: task.cancel()
    await asyncio.gather(*TASKS.values(),*app['workers'],return_exceptions=True)


if __name__=='__main__':
    os.umask(0o077)
    if '--prepare' in __import__('sys').argv:
        DATA.mkdir(parents=True,exist_ok=True);render_go(config())
    else:
        app=web.Application(client_max_size=2*1024*1024)
        app.router.add_get('/api/stats',stats)
        app.router.add_post('/api/config/save',save_config)
        app.router.add_get('/api/{camera}/latest.jpg',snapshot)
        app.router.add_get('/api/{camera}/ptz/info',ptz_info)
        app.router.add_get('/api/catalog',catalog)
        app.router.add_get('/ws',ptz_ws)
        app.on_startup.append(startup);app.on_cleanup.append(cleanup)
        web.run_app(app,host='0.0.0.0',port=5000,access_log=None)
