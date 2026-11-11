import hashlib,pathlib,tempfile,unittest
from sources import source_files,metadata
class SourceFingerprintTests(unittest.TestCase):
    def setUp(self):
        self.tmp=tempfile.TemporaryDirectory(); self.root=pathlib.Path(self.tmp.name); self.tree=self.root/'maia-httpd'; self.tree.mkdir()
    def tearDown(self): self.tmp.cleanup()
    def write(self,name,data):
        p=self.tree/name;p.parent.mkdir(parents=True,exist_ok=True);p.write_bytes(data)
    def test_order_is_case_sensitive_posix_strings_on_every_os(self):
        for n in ['a.rs','Z.rs','m/x.rs','m.rs']: self.write(n,b'fixture\n')
        self.assertEqual(list(source_files(self.root)),['maia-httpd/Z.rs','maia-httpd/a.rs','maia-httpd/m.rs','maia-httpd/m/x.rs'])
    def test_crlf_does_not_silently_match_linux_bytes(self):
        self.write('src/lib.rs',b'let x = 1;\n'); original=source_files(self.root)
        self.write('src/lib.rs',b'let x = 1;\r\n')
        self.assertNotEqual(source_files(self.root),original)
    def test_lockfile_is_input_and_build_output_is_not(self):
        self.write('Cargo.lock',b'locked dependencies\n'); original=metadata(self.root)
        self.write('target/release/maia-httpd',b'old cached executable')
        self.assertEqual(metadata(self.root),original)
        self.write('Cargo.lock',b'changed dependency\n')
        self.assertNotEqual(metadata(self.root),original)
if __name__=='__main__': unittest.main()
