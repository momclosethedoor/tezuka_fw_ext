"""Prepare/export/verify the exact Buildroot patch stack, without local Rust."""
import argparse, hashlib, json, pathlib, re, subprocess, sys
ROOT = pathlib.Path(__file__).resolve().parents[2]
def git(tree, *args, **kw):
    return subprocess.check_output(['git', '-c', 'safe.directory='+str(tree.resolve()).replace('\\','/'), '-C', str(tree), *args], **kw)
def inputs():
    recipe = (ROOT/'package/maia-httpd/maia-httpd.mk').read_text()
    rev = re.search(r'^MAIA_HTTPD_VERSION = (\w+)', recipe, re.M)[1]
    return rev, sorted((ROOT/'package/maia-httpd').glob('*.patch'))
def source_files(tree):
    # Explicit string ordering is identical on Windows and Linux. Path ordering
    # is case-insensitive on Windows and must not define a cross-platform hash.
    result={}
    for p in sorted((tree/'maia-httpd').rglob('*'),key=lambda p:p.relative_to(tree).as_posix()):
        if p.is_file() and 'target' not in p.relative_to(tree).parts:
            result[p.relative_to(tree).as_posix()]=hashlib.sha256(p.read_bytes()).hexdigest()
    return result

def metadata(tree):
    rev, patches = inputs(); files=source_files(tree); h=hashlib.sha256()
    for name, sha in files.items(): h.update(name.encode()+b'\0'+bytes.fromhex(sha))
    return {'fingerprint_format':'exact bytes; UTF-8 POSIX path string order; v2',
            'upstream':rev,'patches':[{'name':p.name,'sha256':hashlib.sha256(p.read_bytes()).hexdigest()} for p in patches],
            'maia_httpd_tree_sha256':h.hexdigest(),'files':files}

if __name__ == '__main__':
    a=argparse.ArgumentParser(); a.add_argument('action',choices=['prepare','export','verify']); a.add_argument('tree',type=pathlib.Path); a.add_argument('--patch',default='0002-local.patch'); args=a.parse_args(); t=args.tree
    rev, patches=inputs()
    if args.action=='prepare':
        git(t,'config','core.autocrlf','false')
        if git(t,'status','--porcelain').strip(): sys.exit('Refusing dirty tree')
        git(t,'checkout','--detach',rev)
        for p in patches: git(t,'apply','--index',str(p))
        git(t,'-c','user.name=OBS local','-c','user.email=obs@localhost','commit','--allow-empty','-m','Local Buildroot patch stack baseline')
        git(t,'branch','-f','obs-patch-base','HEAD')
    elif args.action=='export':
        if git(t,'ls-files','--others','--exclude-standard').strip(): sys.exit('Stage new source files with git add first')
        data=git(t,'diff','--binary','obs-patch-base','--','maia-httpd')
        if not data: sys.exit('No additional source changes')
        dest=ROOT/'package/maia-httpd'/args.patch
        if dest.exists(): sys.exit('Patch already exists; prepare a fresh baseline before further editing')
        dest.write_bytes(data)
    else:
        # CI's freshly extracted and patched Buildroot tree versus expected local source.
        expected=json.loads((ROOT/'package/maia-httpd/source.json').read_text())
        actual=metadata(t)
        if expected!=actual:
            old=expected.get('files',{}); new=actual['files']
            sys.exit('Source/patch fingerprint differs: '+json.dumps({'actual':actual,'missing':sorted(old.keys()-new.keys()),'unexpected':sorted(new.keys()-old.keys()),'changed':[n for n in old.keys() & new.keys() if old[n]!=new[n]]}))
    if args.action!='verify':
        (ROOT/'package/maia-httpd/source.json').write_text(json.dumps(metadata(t),indent=2)+'\n',encoding='utf-8',newline='\n')
