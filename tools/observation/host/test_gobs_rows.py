"""Run against a backed-up work copy or installed host, using its existing venv."""
import argparse, hashlib, importlib.util, io, json, pathlib, struct, sys, unittest
import numpy as np

p=argparse.ArgumentParser()
p.add_argument('--host-root',type=pathlib.Path,required=True)
p.add_argument('--rows',type=pathlib.Path,required=True)
p.add_argument('--real-dir',type=pathlib.Path)
p.add_argument('--output',type=pathlib.Path,required=True)
args=p.parse_args()
sys.path[:0]=[str(args.host_root),str(pathlib.Path(__file__).resolve().parents[1])]
def module(name,path):
    spec=importlib.util.spec_from_file_location(name,path);m=importlib.util.module_from_spec(spec);spec.loader.exec_module(m);return m
rows_module=module('obs004_rows',args.rows)
bridge_module=module('obs004_bridge',pathlib.Path(__file__).with_name('gobs_rows.py'))
from live import LiveReader
from observation import Capture, Descriptor, Observation, NO_TAG, encode
import native
RESULT={}

def stream(powers=((35.,4.,100.,7.),),nint=3):
    d=Descriptor(1,8.,10.,1.,100,2,3,3)
    obs=[Observation(1,j,7,j*100,nint,NO_TAG,tuple(v)) for j,v in enumerate(powers)]
    return LiveReader(io.BytesIO(encode(Capture(bytes([0x55])*16,[d]+obs))),expected_source=3,avg_peak=True)

class HostTests(unittest.TestCase):
    def test_existing_reader_cli_connects_to_precomputed_row_input(self):
        # Offline socket substitute tests the actual CLI hook, never a live claim.
        import contextlib
        from unittest.mock import patch
        import live
        class Socket(io.BytesIO):
            def recv_into(self,b):return self.readinto(b)
            def settimeout(self,t):pass
        d=Descriptor(1,8.,10.,1.,100,2,3,3)
        wire=encode(Capture(bytes([0x55])*16,[d,Observation(1,0,7,25,3,NO_TAG,(35.,4.,100.,7.))]))
        output=io.StringIO()
        with patch.object(sys,'argv',['live.py','--connect','offline:1','--profile','avg-peak','--host-rows-root',str(args.host_root),'--row-ring-rows','4']),patch.object(live.socket,'create_connection',return_value=Socket(wire)),contextlib.redirect_stdout(output):
            live.main()
        report=json.loads(output.getvalue())
        self.assertEqual((report['observations'],report['contributing_ffts'],report['host_rows']['head_seq'],report['host_rows']['measurements']),(1,3,1,1))

    def test_one_group_no_renormalization_no_reaverage_and_repeated_display(self):
        h=rows_module.RowHistory(ring_rows=4,dc_suppress=True)
        r=stream();b=bridge_module.GobsRows(h,r.reader.session);f=r.next();d=r.reader.descriptors[1]
        self.assertTrue(b.accept(f,d,1));i=h.slot(h.head)
        np.testing.assert_allclose(h.avg[i],10*np.log10([35.,4.]),rtol=1e-6)
        np.testing.assert_allclose(h.mx[i],10*np.log10([100.,7.]),rtol=1e-6)
        self.assertEqual((h.frames[i],h.epoch[i],h.frames_total,b.measurements),(3,7,3,1))
        for _ in range(100):
            self.assertFalse(b.accept(f,d,1));h.tick(1000.);bytes(h.out.frame)
        self.assertEqual((h.head,b.measurements,b.repeated),(1,1,100))
        h.append_precomputed(np.array([1.,1.,100.,1.]),np.array([1.,1.,100.,1.]),1.,2.,10.,8.,2,8,1)
        self.assertEqual(h.mx[h.slot(h.head),2],20.) # preserve actual DC bin

    def test_bounded_ring_slow_cursor_gaps_and_legacy_unchanged(self):
        h=rows_module.RowHistory(ring_rows=4)
        slow=h.reader('slow');fast=h.reader('fast')
        r=stream();b=bridge_module.GobsRows(h,r.reader.session);f=r.next();d=r.reader.descriptors[1]
        b.accept(f,d,1);size=h.memory_bytes()
        def allocated():
            return (sum(v.nbytes for v in vars(h).values() if isinstance(v,np.ndarray))
                    + len(h.out.frame) + h.out._scratch.nbytes + h.out._q.nbytes)
        actual=allocated()
        from live import Frame
        for j in range(1,10001):
            f=Frame(1,j,7,j*100,3,NO_TAG,f.payload)
            b.accept(f,d,1);fast.available();fast.next_seq=h.head+1
        first,last=slow.available()
        self.assertEqual(last,h.head);self.assertLessEqual(last-first+1,3)
        self.assertGreater(slow.rows_lost,0)
        self.assertEqual(h.memory_bytes(),size);self.assertEqual(allocated(),actual);self.assertEqual(h.k,0)
        RESULT['bounded_host']=dict(groups=b.measurements,ring_rows=h.R,array_and_frame_bytes=actual,existing_memory_estimator_bytes=size,slow_rows_lost=slow.rows_lost,fast_rows_lost=fast.rows_lost)
        legacy=rows_module.RowHistory(ring_rows=4,dc_suppress=False)
        legacy.feed_lin(np.array([1.,4.],dtype=np.float32),0.,0.,10.,8.,1,1)
        legacy.feed_lin(np.array([9.,16.],dtype=np.float32),0.,0.001,10.,8.,1,1)
        legacy.close();i=legacy.slot(legacy.head)
        np.testing.assert_allclose(legacy.avg[i],10*np.log10([5.,10.]),rtol=1e-6)
        np.testing.assert_allclose(legacy.mx[i],10*np.log10([9.,16.]),rtol=1e-6)

    def test_bad_widths_conflicting_ids_dirty_and_peak_validation(self):
        from live import Frame
        h=rows_module.RowHistory(ring_rows=4);r=stream();b=bridge_module.GobsRows(h,r.reader.session)
        f=r.next();d=r.reader.descriptors[1];b.accept(f,d,1)
        bad=Frame(1,0,8,0,3,NO_TAG,f.payload)
        with self.assertRaises(ValueError):b.accept(bad,d,1)
        dirty=Frame(1,2,0,200,3,NO_TAG,f.payload)
        self.assertFalse(b.accept(dirty,d,1));self.assertEqual((b.unusable,b.gaps,h.head),(1,1,1))
        for epoch,nint,gen in ((1,65536,1),(1<<63,3,1),(1,3,1<<32)):
            with self.assertRaises(ValueError):h.append_precomputed(np.array([1.,2.]),np.array([1.,2.]),0.,1.,10.,8.,gen,epoch,nint)
        with self.assertRaises(ValueError):h.append_precomputed(np.array([2.,2.]),np.array([1.,2.]),0.,1.,10.,8.,1,7,3)

    @unittest.skipUnless(args.real_dir,'local accepted hardware bytes not distributed')
    def test_saved_hardware_ram_exact_bytes_and_independent_reduction_to_host(self):
        folder=args.real_dir
        raw=(folder/'raw-three.bin').read_bytes();records=native.ram_records(raw)
        self.assertEqual(len(records),2)
        accepted=['41ae8fd387782f9fa445c56121bcefe059526033a58b9a71db6291456c732c80','8fb597631da79dd2f37f8de81f8293223f5797c1c82dbee693acda66ece11892']
        decoded=[];proof=[]
        for j,record in enumerate(records):
            self.assertEqual(record,(folder/f'aligned-record-{j}.bin').read_bytes())
            n=native.decode(record);decoded.append(n)
            sha=hashlib.sha256(struct.pack('<4096f',*n.power)).hexdigest()
            self.assertEqual(sha,accepted[j]);proof.append(dict(seq=n.seq,epoch=n.epoch,first_tick=n.first_tick,power_sha256=sha))
        powers=np.array([n.power for n in decoded],dtype=np.float64)
        mean=powers.mean(axis=0).astype(np.float32);peak=powers.max(axis=0).astype(np.float32)
        d=Descriptor(1,30.72e6,2.4e9-15.36e6,native.ENBW,62500000,4096,3,3)
        o=Observation(1,0,7,decoded[0].first_tick,2,NO_TAG,tuple(mean)+tuple(peak))
        wire=encode(Capture(bytes([0x55])*16,[d,o]))
        r=LiveReader(io.BytesIO(wire),expected_source=3,avg_peak=True)
        h=rows_module.RowHistory(ring_rows=4);b=bridge_module.GobsRows(h,r.reader.session)
        self.assertTrue(b.accept(r.next(),d,1));i=h.slot(h.head)
        np.testing.assert_allclose(h.avg[i],10*np.log10(np.maximum(mean,np.float32(1e-30))),atol=1e-5)
        np.testing.assert_allclose(h.mx[i],10*np.log10(np.maximum(peak,np.float32(1e-30))),atol=1e-5)
        self.assertEqual(h.frames[i],2)
        RESULT['real_replay']=dict(raw_sha256=hashlib.sha256(raw).hexdigest(),records=proof,output_sha256=hashlib.sha256(wire).hexdigest(),bins=4096,nint=2,note='Python independent reduction + host adapter; not live ARM or Rust execution')
        args.output.with_suffix('.gobs').write_bytes(wire)

suite=unittest.defaultTestLoader.loadTestsFromTestCase(HostTests)
result=unittest.TextTestRunner(verbosity=2).run(suite)
RESULT.update(tests=result.testsRun,failures=len(result.failures),errors=len(result.errors),skipped=len(result.skipped))
args.output.write_text(json.dumps(RESULT,indent=2)+'\n',encoding='utf-8')
raise SystemExit(0 if result.wasSuccessful() else 1)
