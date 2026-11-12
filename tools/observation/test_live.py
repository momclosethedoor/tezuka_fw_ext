import io, json, os, pathlib, struct, unittest
import observation as offline
from live import LiveReader, RecordReader, MAX_RECORD, verify_synthetic

ROOT=pathlib.Path(os.environ.get('OBS_VECTOR_ROOT',pathlib.Path(__file__).resolve().parents[3]/'maia-sdr/maia-httpd/tests/observation-v1'))
def golden():return offline.load_vectors(ROOT/'golden.hex')
class Fragmented(io.BytesIO):
    def readinto(self,b):return super().readinto(b[:min(3,len(b))])

class LiveTests(unittest.TestCase):
    def test_shared_values_fragmentation_and_streamed_rust(self):
        b=golden();r=LiveReader(Fragmented(b))
        records=[]
        while True:
            f=r.next()
            if f is None:break
            # Keep only this tiny test fixture, never used by live-reader.
            records.append((f.seq,f.epoch,f.first_tick,f.nint,list(f.power())))
        self.assertEqual([v[0] for v in records],[0,1,2,3,5,6])
        self.assertEqual(records[1],(1,7,101,3,[2,4,6,8,3,6,9,12]))
        self.assertEqual(records[2][:4],(2,8,104,1))
        self.assertEqual(r.gaps,1);self.assertEqual(r.epoch_boundaries,1);self.assertEqual(r.unattributed,1)
        self.assertEqual(len(r.reader.buffer),MAX_RECORD)
        rust=os.environ.get('OBS_RUST_STREAMED')
        if rust:self.assertEqual(pathlib.Path(rust).read_bytes(),b)
    def test_live_duplicate_not_counted_offline_reordering_separate(self):
        c=offline.decode(golden());c.records=c.records[:4]+[c.records[2]]
        r=LiveReader(io.BytesIO(offline.encode(c)))
        self.assertEqual(r.next().seq,0);self.assertEqual(r.next().seq,1)
        with self.assertRaisesRegex(ValueError,'non-increasing'):r.next()
        self.assertEqual(r.observations,2)
        # A valid offline delayed old epoch is intentionally rejected in live order.
        c=offline.decode(golden());c.records=c.records[:3]+[c.records[4],c.records[3]]
        r=LiveReader(io.BytesIO(offline.encode(c)));r.next();r.next()
        with self.assertRaisesRegex(ValueError,'non-increasing'):r.next()
    def test_late_join_and_unattributed_do_not_invent_origin(self):
        c=offline.decode(golden());c.records=c.records[:2]+c.records[4:]
        r=LiveReader(io.BytesIO(offline.encode(c)))
        self.assertEqual(r.next().seq,2);self.assertEqual(r.gaps,0)
        while r.next() is not None:pass
        self.assertEqual((r.epoch,r.descriptor),(8,2));self.assertEqual(r.gaps,1)
    def test_profile_and_provenance_fail_closed(self):
        for opts in [dict(expected_source=3),dict(nint1=True)]:
            r=LiveReader(io.BytesIO(golden()),**opts)
            with self.assertRaises(ValueError):r.next()
        r=LiveReader(io.BytesIO(offline.load_vectors(ROOT/'legacy.hex')),expected_source=3)
        with self.assertRaises(ValueError):r.next()
    def test_boundaries_and_sizes(self):
        b=golden()
        for size in (0,1,23,25,71,121,551):
            with self.subTest(size=size),self.assertRaises((ValueError,EOFError)):
                r=LiveReader(Fragmented(b[:size]))
                while r.next() is not None:pass
        b=bytearray(b);struct.pack_into('<I',b,24,0xffffffff)
        with self.assertRaises(ValueError):RecordReader(io.BytesIO(b)).next()
    def test_monotonic_seq_cannot_rebind_epoch_or_reset_tick(self):
        b=bytearray(golden());struct.pack_into('<Q',b,280,7) # seq2 epoch field: descriptor 2 cannot reuse epoch7
        with self.assertRaises(ValueError):
            r=LiveReader(io.BytesIO(b))
            while r.next() is not None:pass
        b=bytearray(golden());struct.pack_into('<Q',b,216,0) # seq1 tick offset
        with self.assertRaises(ValueError):
            r=LiveReader(io.BytesIO(b));r.next();r.next()

if __name__=='__main__':unittest.main()
