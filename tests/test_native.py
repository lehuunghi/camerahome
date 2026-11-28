import asyncio
import json
import sys
import tempfile
import time
import unittest
from pathlib import Path
sys.path.insert(0,str(Path(__file__).resolve().parents[1]/'engine'))
from core import Index, destination_targets, eligible_local_delete, selected_mode


class NativeIndexTests(unittest.TestCase):
    def setUp(self):
        self.tmp=tempfile.TemporaryDirectory();self.root=Path(self.tmp.name)
        self.index=Index(self.root/'index.sqlite')
        self.file=self.root/'2026-11-28/10/camera/00.00.mp4'
        self.file.parent.mkdir(parents=True);self.file.write_bytes(b'video')
        self.path=self.index.register(self.file,self.root,'camera',time.time(),5)

    def tearDown(self): self.tmp.cleanup()

    def test_event_or_semantics_and_pre_post_buffer(self):
        self.index.event('camera',100,'crying',.8)
        self.assertTrue(self.index.active('camera',94,96))
        self.assertTrue(self.index.active('camera',105,110))
        self.assertFalse(self.index.active('other',94,96))
        self.assertFalse(self.index.active('camera',114,120))
        self.assertEqual(selected_mode({'audio':{'enabled':True},'record':{'motion':{'days':3}}}),(True,True))

    def test_upload_recovers_after_restart_and_is_idempotent(self):
        self.index.enqueue('r2','bucket')
        self.assertIsNotNone(self.index.next_copy('r2','bucket'))
        self.assertIsNone(self.index.next_copy('r2','bucket'))
        restarted=Index(self.root/'index.sqlite')
        self.assertIsNotNone(restarted.next_copy('r2','bucket'))
        restarted.complete(self.path,'r2','bucket')
        restarted.enqueue('r2','bucket')
        self.assertIsNone(restarted.next_copy('r2','bucket'))

    def test_different_destination_must_get_its_own_copy(self):
        self.index.enqueue('drive','old-folder');self.index.complete(self.path,'drive','old-folder')
        self.assertFalse(self.index.confirmed(self.path,[('drive','new-folder')]))
        self.assertEqual(self.index.catalog('drive','new-folder'),{})

    def test_retention_needs_all_enabled_copies(self):
        targets=[('r2','bucket'),('drive','folder')]
        for source,target in targets:self.index.enqueue(source,target)
        policy={'backup_enabled':True,'local_enabled':False,'local_off_since':0,'local_days':0}
        row={'path':self.path,'at':time.time()}
        self.index.complete(self.path,'r2','bucket')
        self.assertFalse(eligible_local_delete(row,policy,targets,self.index,time.time()))
        self.index.complete(self.path,'drive','folder')
        self.assertTrue(eligible_local_delete(row,policy,targets,self.index,time.time()))
        policy['backup_enabled']=False
        self.assertFalse(eligible_local_delete(row,policy,targets,self.index,time.time()))

    def test_switch_never_deletes_old_or_only_original(self):
        row={'path':self.path,'at':time.time()}
        p={'backup_enabled':True,'local_enabled':False,'local_off_since':time.time()*1000,'local_days':0}
        self.assertFalse(eligible_local_delete(row,p,[],self.index,time.time()))
        self.index.enqueue('r2','bucket');self.index.complete(self.path,'r2','bucket')
        self.assertFalse(eligible_local_delete(row,p,[('r2','bucket')],self.index,time.time()-10))

    def test_error_backoff_is_persisted(self):
        self.index.enqueue('drive','folder')
        row=self.index.next_copy('drive','folder')
        self.index.fail(row['path'],'drive','folder',0,'temporarily unavailable')
        self.assertIsNone(self.index.next_copy('drive','folder'))
        with self.index.connect() as db:
            result=db.execute('SELECT retry,state,attempts FROM copies').fetchone()
            self.assertGreater(result['retry'],time.time())
            self.assertEqual((result['state'],result['attempts']),('error',1))

    def test_disabled_targets_do_not_get_enabled_by_existing_credentials(self):
        (self.root/'cloud-policy.json').write_text(json.dumps({'backup_enabled':False}))
        (self.root/'drive.conf').touch();(self.root/'drive-folder').write_text('camera')
        self.assertEqual(destination_targets(self.root)[1],{})
        (self.root/'cloud-policy.json').write_text(json.dumps({'backup_enabled':True,'drive_enabled':True}))
        self.assertEqual(destination_targets(self.root)[1]['drive']['target'],'drive:camera/recordings')


if __name__=='__main__': unittest.main()
