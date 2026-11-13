"""Linux runner gates: real ELF, exact rootfs/FIT payloads, PL-5b, flash budget."""
import hashlib, json, lzma, os, pathlib, re, shutil, subprocess, sys, tempfile, zipfile
from sources import ROOT, metadata
out=pathlib.Path(sys.argv[1]).resolve(); images=out/'images'; arm_only='--arm-only' in sys.argv
def sha(p): return hashlib.sha256(p.read_bytes()).hexdigest()
def run(*a): return subprocess.check_output(list(map(str,a)),text=True)
def check(ok,msg):
    if not ok: raise RuntimeError(msg)
src=next((out/'build').glob('maia-httpd-*'))
diagnostics=ROOT/'artifacts/maia-httpd-nano'; diagnostics.mkdir(parents=True,exist_ok=True)
(diagnostics/'actual-sources.json').write_text(json.dumps(metadata(src),indent=2)+'\n')
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
shutil.copyfile(ROOT/'doc/OBN1-ABI-r1.md',artifact/'OBN1-ABI-r1.md')
shutil.copyfile(ROOT/'doc/OBSERVATION-V1.md',artifact/'OBSERVATION-V1.md')
elf=artifact/'maia-httpd'; elf.write_bytes(binary); elf.chmod(0o755)
header=run('readelf','-h',elf); program=run('readelf','-l',elf); attrs=run('readelf','-A',elf); dynamic=run('readelf','-d',elf)
synthetic=artifact/'gobs-synthetic'
shutil.copyfile(compiled.with_name('gobs-synthetic'),synthetic); synthetic.chmod(0o755)
synth_info=''.join(run('readelf',flag,synthetic) for flag in ['-h','-l','-A','-d'])
check('ARM' in synth_info and 'ELF32' in synth_info and 'hard-float ABI' in synth_info and 'Tag_ABI_VFP_args: VFP registers' in synth_info,'Wrong synthetic exerciser ARM ABI')
(artifact/'gobs-synthetic-elf.txt').write_text(synth_info)
check('ARM' in header and 'ELF32' in header and 'hard-float ABI' in header,'Wrong ARM ABI')
check('Tag_ABI_VFP_args: VFP registers' in attrs,'Wrong float convention')
interp=re.search(r'Requesting program interpreter: (.*?)\]',program)[1]
check(interp.lstrip('/') in files,'Missing ELF interpreter')
synthetic_interp=re.search(r'Requesting program interpreter: (.*?)\]',synth_info)[1]
check(synthetic_interp==interp,'Synthetic exerciser interpreter mismatch')
needed=re.findall(r'Shared library: \[(.*?)\]',dynamic)
# Include transitive shared libs; check required symbol versions against rootfs providers.
queue=[elf,synthetic]; seen=set(); versions=[]
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
    # Execute the actual delivered ARM ELF under the target loader/sysroot.
    # This mode exits before UIO/IIO/proc ownership checks and emits no observations.
    config={'capture_id':'11111111-1111-4111-8111-111111111111','coordinated_boot_id':'22222222-2222-4222-8222-222222222222','externally_settled':True,'sample_rate':61440000,'rx_lo_hz':5190000000,'epoch':7,'series_windows':8,'run_seconds':30,'bind':'127.0.0.1:9009'}
    config_path=artifact/'native-config-example.json';config_path.write_text(json.dumps(config,indent=2)+'\n')
    smoke=subprocess.check_output(['qemu-arm','-L',str(out/'target'),str(elf),'--check-native-observations',str(config_path)],text=True,timeout=20)
    check('no hardware access' in smoke,'Offline ARM native config check did not complete')
    avg=dict(config,ram_load_test=True,accumulate_windows=4,derived_capture_id='33333333-3333-4333-8333-333333333333')
    avg_path=artifact/'native-avg-peak-example.json';avg_path.write_text(json.dumps(avg,indent=2)+'\n')
    avg_smoke=subprocess.check_output(['qemu-arm','-L',str(out/'target'),str(elf),'--check-native-observations',str(avg_path)],text=True,timeout=20)
    check('no hardware access' in avg_smoke,'Offline ARM avg/peak config check did not complete')
    resident=dict(config,resident={'run_id':1,'map_id':18446744073709551615,'initial_slot':0,'initial_route':0,'repeat':True,'profiles':[{'slot':0,'lo_hz':5190000000,'bytes':[0]*16}],'entries':[{'slot':0,'marker':False,'last':0,'route':0,'settling_ticks':0,'dwell_ticks':1000000,'context':1}],'route_bind':'127.0.0.1:9010','publish_period_ms':100,'exclusive_controllers_quiesced':True,'ps_fastlock_outputs_zero':True,'coordinated_external_slot':True})
    resident_path=artifact/'native-resident-example.json';resident_path.write_text(json.dumps(resident,indent=2)+'\n')
    resident_smoke=subprocess.check_output(['qemu-arm','-L',str(out/'target'),str(elf),'--check-native-observations',str(resident_path)],text=True,timeout=20)
    check('no hardware access' in resident_smoke,'Offline ARM resident config check did not complete')
    for mode in ('scan_ws','sink'):
        diag=dict(resident,ram_load_test=True,series_windows=8,diagnostics={'mode':mode,'trace':True})
        diag_path=artifact/('native-diagnostic-'+mode+'.json');diag_path.write_text(json.dumps(diag,indent=2)+'\n')
        diagnostic_smoke=subprocess.check_output(['qemu-arm','-L',str(out/'target'),str(elf),'--check-native-observations',str(diag_path)],text=True,timeout=20)
        check('no hardware access' in diagnostic_smoke,'Offline diagnostic schema did not complete')
    invalid=[(dict(config,externally_settled=False),'settling'),
             (dict(avg,series_windows=7),'eight-window'),
             (dict(avg,derived_capture_id=config['capture_id']),'UUID'),
             (dict(avg,accumulate_windows=0),'avg/peak'),
             (dict(config,accumulate_windows=4),'avg/peak')]
    with tempfile.TemporaryDirectory() as tmp:
        path=pathlib.Path(tmp)/'invalid.json'
        for bad,reason in invalid:
            path.write_text(json.dumps(bad))
            r=subprocess.run(['qemu-arm','-L',str(out/'target'),str(elf),'--check-native-observations',str(path)],capture_output=True,text=True,timeout=20)
            check(r.returncode!=0 and reason in r.stderr,'Unsafe native config was not rejected: '+reason)
    (artifact/'native-arm-offline-check.txt').write_text(smoke+'\n'+avg_smoke+'\nFive negative native/avg-peak cases rejected. No hardware accessed.\n')
    continuous=dict(capture_id='66666666-6666-4666-8666-666666666666',coordinated_boot_id='22222222-2222-4222-8222-222222222222',externally_settled=True,controllers_quiesced=True,sample_rate=61440000,rx_lo_hz=2400000000,epoch=7,k=2,peak=False,publish_stride=250,run_seconds=60)
    continuous_path=artifact/'continuous-config-example.json';continuous_path.write_text(json.dumps(continuous,indent=2)+'\n')
    smoke2=subprocess.check_output(['qemu-arm','-L',str(out/'target'),str(elf),'--check-continuous-observations',str(continuous_path)],text=True,timeout=20)
    check('no hardware access' in smoke2,'Offline OBN2 config check failed')
    obn2_results=[]
    for golden in sorted((src/'maia-httpd/tests/observation-continuous').glob('*.bin')):
        text2=subprocess.check_output(['qemu-arm','-L',str(out/'target'),str(elf),'--check-obn2-record',str(golden)],text=True,timeout=20)
        check('bins=4096' in text2 and 'no hardware access' in text2,'ARM OBN2 golden decode failed')
        obn2_results.append({'file':golden.name,'sha256':sha(golden),'result':text2.strip()})
    check(len(obn2_results)==4,'Missing i5 OBN2 golden records')
    (artifact/'obn2-arm-offline.json').write_text(json.dumps(obn2_results,indent=2)+'\n')
    common['continuous_abi']={'id':'OBN2-r2/encoding2','contract_commit':'449b79e839010089836e8202cdae9c6b77d12492','hardware_accepted':False,'static_config':smoke2.strip(),'layout_goldens':obn2_results}
    common['native_abi']={'id':'OBN1-r1','pl_commit':'370d03a02fface2a287d8d7cafb6cee7422217ee','pl_bit_sha256':'311b49bc1d82646ad2f73d33ff475b304318b0e4b0d9049b83f31b087a6b19f9','pl_bin_sha256':'089e9227e72593c546821ca79711caf46246fecff074f741925b7c783ed6ad12','abi_document_sha256':sha(ROOT/'doc/OBN1-ABI-r1.md'),'arm_qemu_offline_config_check':True,'hardware_accepted':False}
    common['synthetic_exerciser']={'sha256':sha(synthetic),'source':'explicit synthetic; not installed or automatically started','interpreter':synthetic_interp,'needed':re.findall(r'Shared library: \[(.*?)\]',synth_info)}
    (artifact/'manifest.json').write_text(json.dumps(common,indent=2)+'\n')
    (artifact/'SHA256SUMS').write_text(sha(elf)+'  maia-httpd\n'+sha(synthetic)+'  gobs-synthetic\n')
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
(artifact/'SHA256SUMS').write_text(sha(elf)+'  maia-httpd\n'+sha(synthetic)+'  gobs-synthetic\n')
(artifact/'elf.txt').write_text(header+program+attrs+dynamic)
shutil.copyfile('maia-build.log',artifact/'maia-build.log')
print(json.dumps(manifest,indent=2))
