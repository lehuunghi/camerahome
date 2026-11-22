#!/usr/bin/env python3
"""One explicitly confirmed full local-video purge, triggered by CameraHome admin."""
import json, os, shutil, sqlite3, subprocess, time
from pathlib import Path
DATA=Path("/opt/camerahome-data")
STORAGE=Path("/opt/frigate/storage")
REQUEST=DATA/"purge-request.json"
STATUS=DATA/"purge-status.json"
VIDEO_SUFFIXES={".mp4",".mkv",".mov",".avi",".webm",".ts",".m4s",".h264",".h265"}
def run(*args):
    return subprocess.run(args,check=True,capture_output=True,text=True,timeout=120)
def status(job,state,message,**extra):
    temp=STATUS.with_suffix(".new")
    temp.write_text(json.dumps(dict(job=job,state=state,message=message,at=int(time.time()),**extra)))
    os.chmod(temp,0o600);os.replace(temp,STATUS)
def is_video(path):
    return path.suffix.lower() in VIDEO_SUFFIXES or ".mp4." in path.name.lower()
def clear_video_storage(root):
    if root.is_symlink() or root.resolve()!=root: raise ValueError("Unexpected storage root")
    removed=0
    for name in ("recordings","camerahome-clips","exports"):
        folder=root/name
        if folder.is_symlink(): raise ValueError("Unexpected video directory")
        if not folder.exists(): continue
        for child in folder.iterdir():
            if child.is_dir() and not child.is_symlink():
                removed+=sum(1 for p in child.rglob("*") if p.is_file())
                shutil.rmtree(child)
            else: child.unlink();removed+=1
    for folder,dirs,files in os.walk(root,followlinks=False):
        for name in files:
            path=Path(folder)/name
            if is_video(path): path.unlink();removed+=1
    remaining=sum(1 for folder,dirs,files in os.walk(root,followlinks=False) for name in files if is_video(Path(name)))
    if remaining: raise RuntimeError("Video files remain")
    return removed
def clear_index(path):
    with sqlite3.connect(path,timeout=30) as db:
        tables={row[0] for row in db.execute("select name from sqlite_master where type='table'")}
        for table in ("recordings","reviewsegment","export","exports"):
            if table in tables: db.execute('DELETE FROM "'+table+'"')
        if "event" in tables: db.execute("UPDATE event SET has_clip=0")
        db.commit()
        db.execute("PRAGMA wal_checkpoint(TRUNCATE)")
def main():
    if not REQUEST.exists(): return
    request=json.loads(REQUEST.read_text())
    job=str(request.get("job",""))
    if request.get("confirm")!="XOA TAT CA" or not job.isdigit(): raise ValueError("Invalid purge request")
    REQUEST.unlink()
    status(job,"running","Đang dừng ghi hình và xóa toàn bộ video trên máy chủ.")
    timer_active=False;stopped=False;removed=None;error=None
    try:
        timer_active=subprocess.run(["systemctl","is-active","--quiet","camerahome-clips.timer"]).returncode==0
        run("systemctl","stop","camerahome-clips.timer","camerahome-clips.service")
        run("docker","stop","--time","30","frigate");stopped=True
        removed=clear_video_storage(STORAGE)
        clear_index(Path("/opt/frigate/config/frigate.db"))
        progress=DATA/"clip-progress.json"
        temp=progress.with_suffix(".new");temp.write_text("{}");os.chmod(temp,0o600);os.replace(temp,progress)
    except Exception as exc:
        error=type(exc).__name__
    finally:
        if stopped:
            try:
                # Recreating clears the old container's video cache as well.
                run("docker","compose","-f","/opt/frigate/compose.yaml","up","-d","--force-recreate","--no-deps","--pull","never","frigate")
            except Exception as exc: error="Restart "+type(exc).__name__
        if timer_active:
            try: run("systemctl","start","camerahome-clips.timer")
            except Exception as exc: error="Timer "+type(exc).__name__
    if error:
        status(job,"error","Chưa hoàn tất xóa hoặc khởi động lại ghi hình. Cần kiểm tra dịch vụ; có thể thử lại.",error=error)
        raise RuntimeError(error)
    status(job,"done","Đã xóa toàn bộ video cũ trên máy chủ, kiểm tra còn 0 video trước khi bật ghi lại. Camera đã được bật ghi tiếp.",removed=removed,remaining_at_cleanup=0)
if __name__=="__main__": main()
