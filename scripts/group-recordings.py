#!/usr/bin/env python3
"""Produce short CameraHome clips from finalized Frigate recordings."""
import collections
import datetime as dt
import json
import os
from pathlib import Path
import re
import subprocess
import time

STORAGE = Path(os.environ.get("CAMERA_CLIP_STORAGE", "/opt/frigate/storage"))
STATE = Path(os.environ.get("CAMERA_CLIP_STATE", "/opt/camerahome-data/clip-progress.json"))
FRIGATE = os.environ.get("CAMERA_CLIP_CONTAINER", "frigate")
RAW = STORAGE / "recordings"
OUTPUT = STORAGE / "camerahome-clips"
ID = re.compile(r"[A-Za-z0-9_-]{1,100}")
FILENAME = re.compile(r"\d{2}\.\d{2}\.mp4")
WAIT_SECONDS = 35
CLIP_SECONDS = 180


def recording_time(file, root):
    day, hour, camera, name = file.relative_to(root).parts
    if not ID.fullmatch(camera) or not FILENAME.fullmatch(name):
        return None
    try:
        at = dt.datetime.strptime(f"{day} {hour}.{name[:-4]}", "%Y-%m-%d %H.%M.%S")
    except ValueError:
        return None
    return at.replace(tzinfo=dt.timezone.utc).timestamp(), camera


def files(root):
    for file in root.glob("*/*/*/*.mp4"):
        parsed = recording_time(file, root)
        if parsed and not file.is_symlink() and file.is_file():
            yield (*parsed, file)


def write_progress(value):
    STATE.parent.mkdir(parents=True, exist_ok=True)
    temp = STATE.with_suffix(".tmp")
    temp.write_text(json.dumps(value), encoding="utf-8")
    temp.replace(STATE)


def batches(items, now):
    batch = []
    for item in items:
        if batch and (item[0] - batch[-1][0] > 12 or item[0] - batch[0][0] > CLIP_SECONDS - 10):
            yield batch
            batch = []
        batch.append(item)
    if batch and (batch[-1][0] - batch[0][0] >= CLIP_SECONDS - 10
                  or now - batch[-1][1].stat().st_mtime >= WAIT_SECONDS):
        yield batch


def export(batch):
    relative = batch[0][1].relative_to(RAW)
    output = OUTPUT / relative
    if output.is_file():
        return
    output.parent.mkdir(parents=True, exist_ok=True)
    temporary = output.with_suffix(".working")
    output_in_container = Path("/media/frigate/camerahome-clips") / temporary.relative_to(OUTPUT)
    playlist = "".join(f"file 'file:/media/frigate/{file.relative_to(STORAGE).as_posix()}'\n" for _, file in batch)
    ffmpeg = ["docker", "exec", "-i", FRIGATE, "timeout", "120s", "nice", "-n", "10",
              "/usr/lib/ffmpeg/7.0/bin/ffmpeg", "-loglevel", "error", "-y", "-threads", "2",
              "-protocol_whitelist", "file,pipe", "-f", "concat", "-safe", "0", "-i", "pipe:0"]
    common = ["-t", str(CLIP_SECONDS), "-movflags", "+faststart", "-f", "mp4", str(output_in_container)]
    compressed = ["-map", "0:v:0", "-map", "0:a:0?", "-c:v", "libsvtav1", "-preset", "10",
                  "-crf", "36", "-svtav1-params", "lp=2", "-pix_fmt", "yuv420p",
                  "-c:a", "aac", "-b:a", "32k", "-ac", "1"]
    try:
        source_bytes = sum(file.stat().st_size for _, file in batch)
        try:
            subprocess.run(ffmpeg + compressed + common, input=playlist, text=True,
                           stderr=subprocess.PIPE, check=True, timeout=130)
            use_compressed = 0 < temporary.stat().st_size < source_bytes
        except (OSError, subprocess.SubprocessError):
            use_compressed = False
        if not use_compressed:
            subprocess.run(ffmpeg + ["-c", "copy"] + common, input=playlist, text=True,
                           stderr=subprocess.PIPE, check=True, timeout=130)
        if not temporary.stat().st_size:
            raise ValueError("empty clip")
        temporary.replace(output)
    finally:
        temporary.unlink(missing_ok=True)


def eligible_recordings():
    code = r"""import json,sqlite3,yaml
with open('/config/config.yml') as f: cfg=yaml.safe_load(f)
c=sqlite3.connect('file:/config/frigate.db?mode=ro',uri=True)
allowed=[]
for camera,settings in cfg['cameras'].items():
 motion=settings.get('record',{}).get('motion',{}).get('days',0)>0
 cry=settings.get('audio',{}).get('enabled',False)
 for path,start,end,movement in c.execute('select path,start_time,end_time,motion from recordings where camera=?',(camera,)):
  if (motion and (movement or 0)>0) or (cry and c.execute("select 1 from event where camera=? and label='crying' and start_time<? and coalesce(end_time,?)>? limit 1",(camera,end,end,start)).fetchone()): allowed.append(path)
print(json.dumps(allowed))"""
    result = subprocess.run(['docker','exec',FRIGATE,'python3','-c',code],
                            capture_output=True,text=True,check=True,timeout=25)
    return {p.removeprefix('/media/frigate/recordings/') for p in json.loads(result.stdout)}


def main():
    now = time.time()
    progress = json.loads(STATE.read_text()) if STATE.exists() else {}
    groups = collections.defaultdict(list)
    eligible = eligible_recordings()
    for at, camera, file in files(RAW):
        if file.relative_to(RAW).as_posix() not in eligible:
            continue
        if at > progress.get(camera, 0) and now - file.stat().st_mtime > 2:
            groups[camera].append((at, file))
    for camera, items in groups.items():
        try:
            for batch in batches(sorted(items), now):
                export(batch)
                progress[camera] = batch[-1][0]
                write_progress(progress)
        except (OSError, ValueError, subprocess.SubprocessError) as error:
            print(f"{camera}: retry next run ({type(error).__name__})", flush=True)
    # Finalized clips are retained by CameraHome only after cloud-copy verification.


if __name__ == "__main__":
    main()
