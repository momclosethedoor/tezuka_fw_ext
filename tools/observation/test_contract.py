"""Contract tests use fixed shared byte vectors, not generated codec output as truth."""
import io,json,math,os,pathlib,struct,unittest
from observation import Capture,Descriptor,Observation,UNKNOWN,NO_TAG,decode,encode,read,load_vectors
from obs import replay
ROOT=pathlib.Path(os.environ.get('OBS_VECTOR_ROOT',pathlib.Path(__file__).resolve().parents[3]/'maia-sdr/maia-httpd/tests/observation-v1'))
class ContractTests(unittest.TestCase):
    def setUp(self):self.wire=load_vectors(ROOT/'golden.hex');self.c=decode(self.wire)
    def test_exact_shared_values_and_bytes(self):
        expected=json.loads((ROOT/'expected.json').read_text())
        obs=[x for x in self.c.records if isinstance(x,Observation)]
        ds=[x for x in self.c.records if isinstance(x,Descriptor)]
        for d,e in zip(ds,expected['descriptors']):
            for k,v in e.items():self.assertEqual(getattr(d,k),v)
        self.assertEqual(self.c.session.hex(),expected['session'])
        for x,e in zip(obs,expected['records']):
            for k in ['descriptor_id','seq','epoch','first_tick','nint']:self.assertEqual(getattr(x,k),e[k])
            for value,ref in zip(x.power,e['power']):self.assertTrue(math.isnan(value) if ref is None else value==ref)
        self.assertEqual(len(obs),len(expected['records']));self.assertEqual(encode(self.c),self.wire)
    def test_nint_is_fft_volume_not_dwell_or_delivery_delay(self):
        obs=[x for x in self.c.records if isinstance(x,Observation)]
        self.assertEqual(obs[0].power[:4],obs[0].power[4:])
        inputs=json.loads((ROOT/'expected.json').read_text())['nint3_input_ffts']
        self.assertEqual(obs[1].power[:4],tuple(sum(col)/3 for col in zip(*inputs)))
        self.assertEqual(obs[1].power[4:],tuple(max(col) for col in zip(*inputs)))
        self.assertEqual(obs[1].nint*4,12)
        self.assertEqual(obs[-1].epoch,0);self.assertFalse(obs[-1].attributed)
    def test_delayed_old_epoch_repeat_and_gap(self):
        delivered=[];sleeps=[];plan=[{'seq':0},{'seq':2,'delay_ms':20},{'seq':1,'delay_ms':30},{'seq':3},{'seq':2},{'seq':5},{'seq':6}]
        out,s=replay(self.c,plan,sleep=sleeps.append,emit=delivered.append)
        self.assertEqual(sleeps,[.02,.03]);self.assertEqual(s['observations'],6);self.assertEqual(s['presentations'],7)
        self.assertEqual(s['missing_seq_count'],1);self.assertEqual(s['repeated_presentations'],1)
        self.assertEqual(s['epoch_boundaries'],[{'after_seq':1,'before_seq':2,'from_epoch':7,'to_epoch':8}])
        self.assertEqual((delivered[2]['seq'],delivered[2]['epoch'],delivered[2]['descriptor_id']),(1,7,1))
        self.assertEqual(delivered[1]['payload_sha256'],delivered[4]['payload_sha256'])
        self.assertTrue(delivered[4]['repeat']);self.assertFalse(delivered[-2]['attributed'])
        self.assertEqual(encode(decode(encode(out))),encode(out))
    def test_legacy_does_not_invent_epoch_clock_or_nint(self):
        b=load_vectors(ROOT/'legacy.hex');c=decode(b);d,o=c.records
        self.assertEqual((d.source,d.tick_hz),(2,0));self.assertTrue(math.isnan(d.bin0_hz))
        self.assertEqual((o.epoch,o.first_tick,o.nint,o.tag),(0,UNKNOWN,0,3));self.assertTrue(math.isnan(o.power[0]))
        self.assertEqual(encode(c),b)
    def test_every_partial_record_is_rejected(self):
        bounds={24};p=24
        while p<len(self.wire):p+=struct.unpack_from('<I',self.wire,p)[0];bounds.add(p)
        for n in range(len(self.wire)):
            if n not in bounds:
                with self.assertRaises(ValueError,msg=str(n)):decode(self.wire[:n])
    def test_fragmented_file_reads(self):
        class Fragments(io.BytesIO):
            def read(self,n=-1):return super().read(min(n,3))
        self.assertEqual(encode(read(Fragments(self.wire))),self.wire)
    def test_huge_lengths_and_missing_descriptor(self):
        b=bytearray(self.wire);struct.pack_into('<I',b,24,0xffffffff)
        with self.assertRaises(ValueError):decode(b)
        b=self.wire[:24]+self.wire[120:]
        with self.assertRaises(ValueError):decode(b)
    def test_duplicate_is_same_observation_and_conflicts_rejected(self):
        c=Capture(self.c.session,self.c.records+[self.c.records[2]])
        _,s=replay(decode(encode(c)));self.assertEqual(s['observations'],6);self.assertEqual(s['presentations'],7)
        x=self.c.records[2];bad=Observation(x.descriptor_id,x.seq,x.epoch,x.first_tick,2,x.tag,x.power)
        with self.assertRaises(ValueError):encode(Capture(self.c.session,self.c.records+[bad]))
    def test_epoch_cannot_reference_a_different_grid(self):
        x=self.c.records[4];bad=Observation(2,x.seq,7,x.first_tick,x.nint,x.tag,x.power)
        with self.assertRaises(ValueError):encode(Capture(self.c.session,self.c.records[:4]+[bad]))
    def test_bad_modes_counts_and_power(self):
        for offset,value,fmt in [(68,4,'<H'),(152,0,'<I'),(160,float('inf'),'<f'),(160,-1.,'<f'),(160,0x7fc00001,'<I')]:
            b=bytearray(self.wire);struct.pack_into(fmt,b,offset,value)
            with self.assertRaises(ValueError):decode(b)
    def test_64bit_identity_is_not_truncated(self):
        b=bytearray(self.wire);b[128:136]=bytes([8,7,6,5,4,3,2,1]);c=decode(b)
        self.assertEqual(c.records[2].seq,0x0102030405060708);self.assertEqual(encode(c),b)
    @unittest.skipUnless(os.environ.get('OBS_RUST_REENCODED'),'Rust cross-check runs in CI')
    def test_rust_emits_the_same_reference_bytes(self):
        self.assertEqual(pathlib.Path(os.environ['OBS_RUST_REENCODED']).read_bytes(),self.wire)
if __name__=='__main__':unittest.main()
