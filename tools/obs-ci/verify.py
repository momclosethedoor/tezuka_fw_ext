"""Linux runner gates: real ELF, exact rootfs/FIT payloads, PL-5b, flash budget."""
import hashlib, json, lzma, os, pathlib, re, shutil, subprocess, sys, tempfile, zipfile
from sources import ROOT, metadata
out=pathlib.Path(sys.argv[1]).resolve(); images=out/'images'; arm_only='--arm-only' in sys.argv
def sha(p): return hashlib.sha256(p.read_bytes()).hexdigest()
def run(*a): return subprocess.check_output(list(map(str,a)),text=True)
def check(ok,msg):
    if not ok: raise RuntimeError(msg)
src=next((out/'build').glob('maia-httpd-*'))
subprocess.run([sys.executable,str(ROOT/'tools/obs-ci/sources.py'),'verify',str(src)],check=True)
check('Compiling maia-httpd v' in pathlib.Path('maia-build.log').read_text(),'No compilation evidence')
# Parse newc without extracting device nodes or following rootfs symlinks.
compiled=src/'maia-httpd/target/armv7-unknown-linux-gnueabihf/release/maia-httpd'
if arm_only:
    binary=compiled.read_bytes()
    files={p.relative_to(out/'target').as_posix():b'' for p in (out/'target').rglob('*')}
else:
    data=lzma.decompress((images/'rootfs.cpio.xz').read_bytes()); files={}; pos=0
    while True:
        check(data[pos:pos+6] in (b'070701',b'070702'),'Invalid newc')
        hdr=[int(data[pos+6+i*8:pos+14+i*8],16) for i in range(13)]
        size,namesize=hdr[6],hdr[11]; pos+=110
        name=data[pos:pos+namesize-1].decode().removeprefix('./'); pos=(pos+namesize+3)&~3
        if name=='TRAILER!!!': break
        files[name]=data[pos:pos+size]; pos=(pos+size+3)&~3
    binary=lzma.decompress(files['usr/bin/maia-httpd.xz'])
    check(binary==compiled.read_bytes(),'Rootfs and compiled binary differ')
artifact=ROOT/'artifacts/maia-httpd-nano'; artifact.mkdir(parents=True,exist_ok=True)
shutil.copyfile(ROOT/'doc/OBS-001-CI.md',artifact/'RUNBOOK.md')
elf=artifact/'maia-httpd'; elf.write_bytes(binary); elf.chmod(0o755)
header=run('readelf','-h',elf); program=run('readelf','-l',elf); attrs=run('readelf','-A',elf); dynamic=run('readelf','-d',elf)
check('ARM' in header and 'ELF32' in header and 'hard-float ABI' in header,'Wrong ARM ABI')
check('Tag_ABI_VFP_args: VFP registers' in attrs,'Wrong float convention')
interp=re.search(r'Requesting program interpreter: (.*?)\]',program)[1]
check(interp.lstrip('/') in files,'Missing ELF interpreter')
needed=re.findall(r'Shared library: \[(.*?)\]',dynamic)
# Include transitive shared libs; check required symbol versions against rootfs providers.
queue=[elf]; seen=set(); versions=[]
while queue:
    obj=queue.pop(); dyn=run('readelf','-d',obj)
    ver=run('readelf','-V',obj)
    for lib in re.findall(r'Shared library: \[(.*?)\]',dyn):
        candidates=list((out/'target/lib').glob(lib))+list((out/'target/usr/lib').glob(lib))
        check(bool(candidates),'Missing library '+lib)
        provider=candidates[0].resolve()
        check(any(n.endswith('/'+provider.name) for n in files),'Library absent from packed rootfs: '+str(provider))
        if provider not in seen: seen.add(provider); queue.append(provider)
    # readelf version needs entries pair each File with its required Names.
    for block in re.split(r'File: ',ver)[1:]:
        lib=block.split()[0]; req=re.findall(r'Name: (\S+)',block)
        candidates=list((out/'target/lib').glob(lib))+list((out/'target/usr/lib').glob(lib))
        if candidates:
            provided=run('readelf','-V',candidates[0])
            for v in req: check(v in provided,'Unsatisfied symbol version '+lib+':'+v)
common={'scope':'arm-only; target staging checked, packed rootfs/FIT not checked','head_sha':os.environ['GITHUB_SHA'],'run_id':os.environ['GITHUB_RUN_ID'],'attempt':os.environ['GITHUB_RUN_ATTEMPT'],'sources':metadata(src),'target':'armv7-unknown-linux-gnueabihf','rust':run('rustc','+1.94.0','-Vv'),'gcc':run(out/'host/bin/arm-linux-gcc','--version'),'sysroot':run(out/'host/bin/arm-linux-gcc','-print-sysroot').strip(),'interpreter':interp,'needed':needed,'libraries':{str(p.relative_to(out/'target')):sha(p) for p in seen},'maia_sha256':sha(elf),'cargo_lock_sha256':sha(src/'maia-httpd/Cargo.lock')}
if arm_only:
    (artifact/'manifest.json').write_text(json.dumps(common,indent=2)+'\n')
    (artifact/'SHA256SUMS').write_text(sha(elf)+'  maia-httpd\n')
    (artifact/'elf.txt').write_text(header+program+attrs+dynamic)
    shutil.copyfile('maia-build.log',artifact/'maia-build.log')
    print(json.dumps(common,indent=2)); sys.exit(0)
xsa=ROOT/'board/tezuka/nano/bitstream/tezuka.xsa'
with zipfile.ZipFile(xsa) as z: check(z.read('system_top.bit')==(images/'system_top.bit').read_bytes(),'Stale XSA payload')
check(sha(images/'system_top.bit.bin')=='ac2b2a2c7c7588606222e5a26c0aff94767a42fe26ddd16c869478a08bc1c057','PL-5b changed')
# Verify actual FIT nodes, not only intermediate build files.
for node, filename in [('fpga@1','system_top.bit'),('ramdisk@1','rootfs.cpio.xz'),('linux_kernel@1','Image.lzma'),('fdt@1','nano.dtb')]:
    with tempfile.NamedTemporaryFile() as tmp:
        subprocess.run(['fdtget','-t','bx',str(images/'pluto.itb'),'/images/'+node,'data'],stdout=tmp,check=True)
        tmp.seek(0); payload=bytes(int(x,16) for x in tmp.read().split())
    check(payload==(images/filename).read_bytes(),'FIT payload mismatch '+node)
frm=images/'flash/pluto.frm'; check(frm.stat().st_size<=0x1e00000,'Flash budget exceeded')
itb=(images/'pluto.itb').read_bytes(); check(frm.read_bytes()==itb+hashlib.md5(itb).hexdigest().encode()+b'\n','FRM payload mismatch')
gcc=out/'host/bin/arm-linux-gcc'
manifest={'scope':'full firmware; packed rootfs and FIT verified','head_sha':os.environ['GITHUB_SHA'],'run_id':os.environ['GITHUB_RUN_ID'],'attempt':os.environ['GITHUB_RUN_ATTEMPT'],'sources':metadata(src),'target':'armv7-unknown-linux-gnueabihf','rust':run('rustc','+1.94.0','-Vv'),'gcc':run(gcc,'--version'),'sysroot':run(gcc,'-print-sysroot').strip(),'interpreter':interp,'needed':needed,'libraries':{str(p.relative_to(out/'target')):sha(p) for p in seen},'components':{n:sha(images/n) for n in ['rootfs.cpio.xz','system_top.bit','system_top.bit.bin','Image.lzma','nano.dtb','pluto.itb','flash/pluto.frm']},'xsa_sha256':sha(xsa),'maia_sha256':sha(elf),'cargo_lock_sha256':sha(src/'maia-httpd/Cargo.lock'),'checks':'compiled; source fingerprint; rootfs equality; ARM hard float; loader/libs/versions; XSA; PL-5b; FIT payloads; FRM; size'}
(artifact/'manifest.json').write_text(json.dumps(manifest,indent=2)+'\n')
(artifact/'SHA256SUMS').write_text(sha(elf)+'  maia-httpd\n')
(artifact/'elf.txt').write_text(header+program+attrs+dynamic)
shutil.copyfile('maia-build.log',artifact/'maia-build.log')
print(json.dumps(manifest,indent=2))
