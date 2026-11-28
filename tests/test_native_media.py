"""Run inside the native image to exercise real FFmpeg and the installed YAMNet model."""
import asyncio
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

sys.path.insert(0,str(Path(__file__).resolve().parents[1]/'engine'))
HAS_CV=bool(importlib.util.find_spec('cv2'))


@unittest.skipUnless(HAS_CV,'requires native engine dependencies')
class NativeMediaTests(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self):
        import service
        self.service=service
        self.temp=tempfile.TemporaryDirectory();self.root=Path(self.temp.name)
        self.old={k:getattr(service,k) for k in ['DATA','ROOT','SPOOL','INDEX','EXPORT_LOCK','STATUS','ffmpeg']}
        service.DATA=self.root/'data';service.ROOT=self.root/'recordings';service.SPOOL=self.root/'spool'
        for p in [service.DATA,service.ROOT,service.SPOOL]:p.mkdir()
        from core import Index
        service.INDEX=Index(service.DATA/'index.sqlite');service.EXPORT_LOCK=asyncio.Lock()

    async def asyncTearDown(self):
        for key,value in self.old.items():setattr(self.service,key,value)
        self.temp.cleanup()

    async def test_remux_preserves_hd_and_sound_then_indexes_only_final_clip(self):
        service=self.service
        source=service.SPOOL/'sample.ts'
        result=await asyncio.create_subprocess_exec('ffmpeg','-loglevel','error','-y',
            '-f','lavfi','-i','testsrc2=size=1920x1080:rate=5','-f','lavfi','-i','sine=frequency=440:sample_rate=16000',
            '-t','1','-c:v','libx264','-preset','ultrafast','-threads','1','-c:a','aac','-f','mpegts',str(source))
        self.assertEqual(await result.wait(),0)
        rows=[{'path':str(source),'start':1795860000.,'end':1795860001.}]
        with service.INDEX.connect() as db:
            db.execute("INSERT INTO segments(path,camera,start,end,state) VALUES(?,?,?,?,?)",(str(source),'test',rows[0]['start'],rows[0]['end'],'selected'))
        await service.export_segments('test',rows)
        files=list(service.ROOT.glob('*/*/*/*.mp4'));self.assertEqual(len(files),1)
        info=json.loads(subprocess.check_output(['ffprobe','-v','error','-show_streams','-of','json',str(files[0])]))
        video=next(s for s in info['streams'] if s['codec_type']=='video')
        audio=next(s for s in info['streams'] if s['codec_type']=='audio')
        self.assertEqual((video['codec_name'],video['width'],video['height']),('h264',1920,1080))
        self.assertEqual(audio['codec_name'],'aac')
        with service.INDEX.connect() as db:
            self.assertEqual(db.execute('SELECT count(*) FROM clips').fetchone()[0],1)
            self.assertEqual(db.execute('SELECT state FROM segments').fetchone()[0],'done')
        self.assertFalse(list(service.ROOT.rglob('*.working')))

    async def test_tapo_g711_audio_survives_buffer_and_mp4(self):
        import csv
        service=self.service;source=self.root/'tapo.mkv'
        proc=await asyncio.create_subprocess_exec('ffmpeg','-loglevel','error','-y',
            '-f','lavfi','-i','testsrc2=size=640x360:rate=5','-f','lavfi','-i','sine=frequency=440:sample_rate=8000',
            '-t','2','-c:v','libx264','-preset','ultrafast','-threads','1','-c:a','pcm_alaw',str(source))
        self.assertEqual(await proc.wait(),0)
        service.ffmpeg=lambda camera:['ffmpeg','-hide_banner','-loglevel','error','-nostdin','-i',str(source)]
        service.STATUS={'test':{}}
        await asyncio.wait_for(service.capture('test'),10)
        listing=next(service.SPOOL.glob('test/*/segments.csv'))
        row=next(csv.reader(listing.open()))
        segments=[{'path':row[0],'start':1795860000.,'end':1795860000.+float(row[2])-float(row[1])}]
        await service.export_segments('test',segments)
        output=next(service.ROOT.glob('*/*/*/*.mp4'))
        info=json.loads(subprocess.check_output(['ffprobe','-v','error','-show_streams','-of','json',str(output)]))
        self.assertTrue(any(s['codec_type']=='audio' and s['codec_name']=='aac' for s in info['streams']))

    async def test_stopping_a_full_video_pipe_does_not_hang(self):
        proc=await self.service.spawn(['ffmpeg','-loglevel','error','-f','lavfi','-i',
            'testsrc2=size=1920x1080:rate=30','-pix_fmt','yuv420p','-f','rawvideo','pipe:1'],True)
        await asyncio.sleep(.3)
        await asyncio.wait_for(self.service.stop(proc,drain=True),8)
        self.assertIsNotNone(proc.returncode)

    async def test_paused_purge_does_not_export(self):
        service=self.service
        (service.DATA/'purge-request.json').write_text('{}')
        await service.export_segments('test',[{'path':'never-opened','start':1,'end':2}])
        self.assertFalse(list(service.ROOT.rglob('*.mp4')))

    async def test_real_cry_model_has_correct_label_and_rejects_silence(self):
        service=self.service
        if not service.MODEL.exists(): self.skipTest('YAMNet model not mounted')
        import numpy as np
        model=service.CryModel()
        self.assertEqual(model.cry,20)
        self.assertLess(model.score(np.zeros(model.samples,dtype=np.float32)),.35)

@unittest.skipUnless(HAS_CV,'requires native engine dependencies')
class MotionTests(unittest.TestCase):
    def test_static_noise_and_global_brightness_do_not_trigger(self):
        import numpy as np
        from vision import MotionDetector
        detector=MotionDetector()
        frame=np.full((180,320,3),80,np.uint8)
        for _ in range(20):self.assertFalse(detector.feed(frame)[0])
        rng=np.random.default_rng(42)
        for _ in range(20):
            noisy=frame+rng.integers(0,4,size=frame.shape,dtype=np.uint8)
            self.assertFalse(detector.feed(noisy)[0])
        self.assertFalse(detector.feed(np.full_like(frame,140))[0])
        self.assertFalse(detector.feed(np.full_like(frame,140))[0])

    def test_localized_motion_requires_confirmation(self):
        import numpy as np
        from vision import MotionDetector
        detector=MotionDetector();frame=np.zeros((180,320,3),np.uint8)
        for _ in range(20):detector.feed(frame)
        moving=frame.copy();moving[50:90,100:150]=200
        self.assertFalse(detector.feed(moving)[0])
        self.assertTrue(detector.feed(moving)[0])

if __name__=='__main__':unittest.main()
