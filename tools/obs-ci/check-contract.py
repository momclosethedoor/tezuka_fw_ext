"""Run native Rust + independent Python contract checks before expensive ARM work."""
import hashlib,json,os,pathlib,shutil,subprocess,sys,tarfile,tempfile,urllib.request
ROOT=pathlib.Path(__file__).resolve().parents[2]
sys.path.insert(0,str(ROOT/'tools/obs-ci'))
from sources import inputs,metadata

def command(args,cwd,env,log):
    r=subprocess.run(list(map(str,args)),cwd=cwd,env=env,stdout=subprocess.PIPE,stderr=subprocess.STDOUT,text=True)
    log.write_text(r.stdout,encoding='utf-8');print(r.stdout)
    if r.returncode:raise SystemExit(r.returncode)

def main():
    output=ROOT/'artifacts/observation-contract';output.mkdir(parents=True,exist_ok=True)
    rev,patches=inputs();version=next(l.split(' = ')[1] for l in (ROOT/'package/maia-httpd/maia-httpd.mk').read_text().splitlines() if l.startswith('MAIA_HTTPD_RUST_VERSION = '))
    with tempfile.TemporaryDirectory(prefix='observation-contract-') as td:
        work=pathlib.Path(td);archive=work/'upstream.tar.gz';tree=work/'source';tree.mkdir()
        urllib.request.urlretrieve('https://github.com/F5OEO/maia-sdr/archive/'+rev+'.tar.gz',archive)
        with tarfile.open(archive) as tf:
            for item in tf.getmembers():
                parts=item.name.split('/')[1:]
                if not parts or parts[0]!='maia-httpd':continue
                dest=tree.joinpath(*parts)
                if not dest.resolve().is_relative_to(tree.resolve()):raise ValueError('unsafe archive path')
                if item.isfile():dest.parent.mkdir(parents=True,exist_ok=True);dest.write_bytes(tf.extractfile(item).read())
                elif not item.isdir():raise ValueError('unsupported archive member')
        def git(*args):subprocess.run(['git','-C',str(tree),*args],check=True)
        git('init','-q');git('config','core.autocrlf','false');git('add','--','maia-httpd')
        for p in patches:git('apply','--index',str(p))
        actual=metadata(tree);expected=json.loads((ROOT/'package/maia-httpd/source.json').read_text())
        (output/'source.json').write_text(json.dumps(actual,indent=2)+'\n')
        if actual!=expected:raise SystemExit('Contract test source differs from Buildroot patch manifest')
        vectors=tree/'maia-httpd/tests/observation-v1';env=dict(os.environ,OBS_VECTOR_ROOT=str(vectors),OBS_RUST_REENCODED=str(output/'rust-roundtrip.gobs'),PYTHONUTF8='1')
        exe=work/'contract-tests'
        command(['rustc','+'+version,'--edition=2024','--test',tree/'maia-httpd/src/observation.rs','-o',exe],tree,env,output/'rust-compile.log')
        command([exe,'--nocapture'],tree,env,output/'rust-tests.log')
        command([sys.executable,'-m','unittest','discover','-s',ROOT/'tools/observation','-p','test_*.py','-v'],ROOT,env,output/'python-tests.log')
        for name in ['golden','legacy']:
            data=bytes.fromhex((vectors/(name+'.hex')).read_text());(output/(name+'.gobs')).write_bytes(data)
        shutil.copyfile(vectors/'expected.json',output/'expected.json')
        shutil.copyfile(ROOT/'doc/OBSERVATION-V1.md',output/'OBSERVATION-V1.md')
        (output/'SHA256SUMS').write_text(''.join(hashlib.sha256(p.read_bytes()).hexdigest()+'  '+p.name+'\n' for p in sorted(output.iterdir()) if p.is_file() and p.name!='SHA256SUMS'))
        print('Observation contract verified in both languages; ARM build may proceed.')
if __name__=='__main__':main()
