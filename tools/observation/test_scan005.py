"""Independent stdlib wire check of the actual Rust scan-frame output in CI."""
import os
import pathlib
import struct
import unittest


class ScanWire(unittest.TestCase):
    def test_actual_rust_frame(self):
        root = os.environ.get('OBS_NATIVE_OUTPUT_DIR')
        if not root:
            self.skipTest('actual Rust output requires existing CI; no local Rust toolchain')
        data = (pathlib.Path(root)/'scan-frame.scn').read_bytes()
        h = struct.unpack_from('<4sHH16sQQQQQQQddIIII', data)
        self.assertEqual(h[:3], (b'SCN1', 1, 112))
        self.assertEqual(h[3:8], (b'\x55'*16, 2**64-1, 0xf0000000, 0, 0))
        self.assertEqual(h[8:11], (100, 62500200, 400))
        self.assertEqual(h[11:], (99997952.0, 1.0, 6144, 2, 1, 62500000))
        part = struct.Struct('<IIQQQIIIIIIQ')
        a, b = [part.unpack_from(data, 112+i*64) for i in range(2)]
        self.assertEqual(a[:5], (1, 0, 1, (0xf0000000 << 32)|2, 100))
        self.assertEqual(b[:5], (1, 1, 2, (0xf0000000 << 32)|4, 200))
        owners = list(struct.iter_unpack('<HH', data[240:240+6144*4]))
        self.assertEqual(owners[:4096], [(0, k) for k in range(4096)])
        self.assertEqual(owners[4096:], [(1, k) for k in range(2048, 4096)])
        self.assertEqual(struct.unpack_from('<6144f', data, 240+6144*4), (3.,)*4096+(5.,)*2048)
        self.assertEqual(len(data), 112+64*2+8*6144)


if __name__ == '__main__':
    unittest.main()
