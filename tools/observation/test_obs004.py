"""Targeted OBS-004 checks. Python tests do not execute the Rust implementation."""
import io, os, pathlib, struct, unittest
import native
from live import LiveReader
from observation import Capture, Descriptor, Observation, NO_TAG, encode

VECTORS=pathlib.Path(os.environ.get('OBS_NATIVE_VECTOR_ROOT',pathlib.Path(__file__).resolve().parents[3]/'maia-sdr/maia-httpd/tests/observation-native'))

class Obs004(unittest.TestCase):
    def test_ram_whole_extent_edges_and_no_join_between_calls(self):
        first=(4096-544)*8
        a=bytes.fromhex((VECTORS/'dma-seq-0.hex').read_text())
        b=bytes.fromhex((VECTORS/'dma-seq-1.hex').read_text())
        raw=bytearray(3*native.BYTES)
        raw[first:first+native.BYTES]=a
        raw[first+native.BYTES:first+2*native.BYTES]=b
        self.assertEqual(native.ram_records(raw),(a,b))
        for length in (0,1,native.BYTES-1,3*native.BYTES-1,9*native.BYTES):
            with self.assertRaises(ValueError):native.ram_records(bytes(length))
        self.assertEqual(native.ram_records(raw[:native.BYTES]),())
        self.assertEqual(native.ram_records(raw[2*native.BYTES:]),())
        # Swapping physical slots is not repaired by matching modulo indices.
        with self.assertRaises(ValueError):native.ram_records(raw[native.BYTES:]+raw[:native.BYTES])
        # An otherwise valid but unrelated record inside a series is rejected.
        raw[first+native.BYTES:first+2*native.BYTES]=a
        with self.assertRaises(ValueError):native.ram_records(raw)
        self.assertEqual(len(native.decode(a).power),4096) # ordinary unshifted strict parser
        with self.assertRaises(ValueError):native.decode(bytes(raw[:native.BYTES]))

    def test_reader_avg_peak_one_measurement_and_explicit_profile(self):
        d=Descriptor(1,8.,10.,1.,100,2,3,3)
        o=Observation(1,0,7,25,3,NO_TAG,(35.,4.,100.,7.))
        wire=encode(Capture(bytes([0x55])*16,[d,o]))
        r=LiveReader(io.BytesIO(wire),expected_source=3,avg_peak=True)
        f=r.next();self.assertEqual(tuple(f.power()),o.power)
        self.assertEqual((r.observations,r.contributing_ffts),(1,3))
        self.assertIsNone(r.next());self.assertEqual(r.observations,1)
        with self.assertRaises(ValueError):LiveReader(io.BytesIO(wire),nint1=True).next()
        single=Descriptor(1,8.,10.,1.,100,2,1,3)
        raw=encode(Capture(bytes([1])*16,[single,Observation(1,9,7,25,1,NO_TAG,(2.,3.))]))
        r=LiveReader(io.BytesIO(raw),expected_source=3,nint1=True)
        self.assertEqual(r.next().nint,1)
        with self.assertRaises(ValueError):LiveReader(io.BytesIO(raw),avg_peak=True).next()

    @unittest.skipUnless(os.environ.get('OBS_NATIVE_OUTPUT_DIR'),'requires compiled Rust output; Python oracle is not Rust execution')
    def test_rust_avg_peak_independent_numbers_and_wire_identity(self):
        p=pathlib.Path(os.environ['OBS_NATIVE_OUTPUT_DIR'])/'avg-peak.gobs'
        r=LiveReader(io.BytesIO(p.read_bytes()),expected_source=3,avg_peak=True)
        f=r.next()
        windows=((1.,4.),(100.,1.),(4.,7.))
        mean=tuple(sum(row[k] for row in windows)/3 for k in range(2))
        peak=tuple(max(row[k] for row in windows) for k in range(2))
        self.assertEqual(tuple(f.power()),mean+peak)
        self.assertEqual((f.seq,f.first_tick,f.nint,f.epoch),(0,0,3,7))
        self.assertEqual(r.reader.session,bytes([0x55])*16)
        self.assertIsNone(r.next())

if __name__=='__main__':unittest.main()
