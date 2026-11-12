import hashlib,io,json,os,pathlib,struct,unittest
import native
from live import LiveReader

ROOT=pathlib.Path(os.environ.get('OBS_NATIVE_VECTOR_ROOT',pathlib.Path(__file__).resolve().parents[3]/'maia-sdr/maia-httpd/tests/observation-native'))
def raw(name):return bytes.fromhex((ROOT/(name+'.hex')).read_text())
class NativeTests(unittest.TestCase):
    def test_authoritative_i5_all_bins_normalization_and_metadata(self):
        expected=json.loads((ROOT/'expected.json').read_text())
        self.assertEqual(expected['abi'],'OBN1-r1');self.assertEqual(native.ENBW,expected['normalization']['enbw_bins'])
        for row in expected['records']:
            with self.subTest(name=row['name']):
                b=raw(row['name']);self.assertEqual(hashlib.sha256(b).hexdigest(),row['native_sha256'])
                n=native.decode(b);m=row['metadata']
                self.assertEqual((n.seq,n.epoch,n.first_tick,n.clean,n.use_ddc),(m['seq'],m['epoch'],m['first_tick'],m['clean'],m['use_ddc']))
                self.assertEqual(hashlib.sha256(struct.pack('<4096f',*n.power)).hexdigest(),row['power_f32_sha256'])
    @unittest.skipUnless(os.environ.get('OBS_NATIVE_OUTPUT_DIR'),'Rust native conversion cross-check runs in CI')
    def test_rust_gobs_conversion_exact_powers_and_provenance(self):
        expected=json.loads((ROOT/'expected.json').read_text())
        for row in expected['records']:
            with self.subTest(name=row['name']):
                p=pathlib.Path(os.environ['OBS_NATIVE_OUTPUT_DIR'])/(row['name']+'.gobs')
                reader=LiveReader(io.BytesIO(p.read_bytes()),expected_source=1,nint1=True)
                self.assertEqual(reader.reader.session,bytes([0x44 if row['name'].startswith('dma-') else 0x33])*16)
                f=reader.next();m=row['metadata'];self.assertEqual((f.seq,f.epoch,f.first_tick,f.nint),(m['seq'],m['epoch'],m['first_tick'],1))
                self.assertEqual(hashlib.sha256(f.payload).hexdigest(),row['power_f32_sha256'])
                self.assertEqual(reader.reader.descriptors[f.descriptor_id].enbw_bins,native.ENBW)
                self.assertIsNone(reader.next())
        r=LiveReader(io.BytesIO((pathlib.Path(os.environ['OBS_NATIVE_OUTPUT_DIR'])/'native-sequence.gobs').read_bytes()),expected_source=1,nint1=True)
        ids=[]
        while True:
            f=r.next()
            if f is None:break
            ids.append((f.seq,f.epoch,f.descriptor_id))
        self.assertEqual(ids,[(0,7,1),(5,8,2),(6,0,1)])
        self.assertEqual((r.observations,r.gaps,r.epoch_boundaries,r.unattributed),(3,4,1,1))
    def test_native_fields_and_partial_record_fail_closed(self):
        b=raw('seq-0')
        for size in (0,1,32767,32769):
            with self.assertRaises(ValueError):native.decode((b+b'\0')[:size])
        for offset in (6,22,8*40+5,8*40+7):
            bad=bytearray(b);bad[offset]^=1
            with self.assertRaises(ValueError):native.decode(bad)
    def test_scale_no_legacy_fs_or_nint_factor_and_nyquist_bin_zero(self):
        b=bytearray(raw('seq-0'))
        # Zero payload while preserving native header lanes.
        for j in range(4096):b[8*j:8*j+6]=bytes(6)
        # Native j=reverse12(2048)=1 -> fftshift bin0; exact expected M/S^2.
        m=123456789;w=struct.unpack_from('<Q',b,8)[0]|m;struct.pack_into('<Q',b,8,w)
        n=native.decode(b);self.assertGreater(n.power[0],0);self.assertTrue(all(v==0 for v in n.power[1:]))
        self.assertEqual(n.power[0],struct.unpack('<f',struct.pack('<f',m/750668**2))[0])
if __name__=='__main__':unittest.main()
