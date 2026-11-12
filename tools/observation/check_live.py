"""Real TCP stress of the EXPLICIT SYNTHETIC source; no hardware access.

Runs bounded child processes, samples source RSS/CPU without retaining samples,
checks all received payloads/labels and compares fast/slow client progress.
"""
import argparse, ctypes, json, os, pathlib, subprocess, sys, time, uuid

def process_usage(pid):
    if os.name=='nt':
        from ctypes import wintypes as w
        class Counters(ctypes.Structure):
            _fields_=[('cb',w.DWORD),('faults',w.DWORD)]+[(n,ctypes.c_size_t) for n in ('peak_working','working','peak_paged','paged','peak_nonpaged','nonpaged','pagefile','peak_pagefile')]
        k=ctypes.WinDLL('kernel32',use_last_error=True);p=ctypes.WinDLL('psapi',use_last_error=True)
        k.OpenProcess.argtypes=[w.DWORD,w.BOOL,w.DWORD];k.OpenProcess.restype=w.HANDLE
        k.CloseHandle.argtypes=[w.HANDLE]
        p.GetProcessMemoryInfo.argtypes=[w.HANDLE,ctypes.POINTER(Counters),w.DWORD]
        k.GetProcessTimes.argtypes=[w.HANDLE,ctypes.POINTER(w.FILETIME),ctypes.POINTER(w.FILETIME),ctypes.POINTER(w.FILETIME),ctypes.POINTER(w.FILETIME)]
        h=k.OpenProcess(0x410,False,pid)
        if not h:return None
        try:
            c=Counters();c.cb=ctypes.sizeof(c)
            if not p.GetProcessMemoryInfo(h,ctypes.byref(c),c.cb):return None
            times=[w.FILETIME() for _ in range(4)]
            if not k.GetProcessTimes(h,*[ctypes.byref(t) for t in times]):return None
            ticks=lambda t:(t.dwHighDateTime<<32)|t.dwLowDateTime
            return c.working,(ticks(times[2])+ticks(times[3]))/1e7
        finally:k.CloseHandle(h)
    try:
        rss=int(next(l.split()[1] for l in pathlib.Path(f'/proc/{pid}/status').read_text().splitlines() if l.startswith('VmRSS:')))*1024
        stat=pathlib.Path(f'/proc/{pid}/stat').read_text().rsplit(')',1)[1].split()
        return rss,(int(stat[11])+int(stat[12]))/os.sysconf('SC_CLK_TCK')
    except (FileNotFoundError,StopIteration):return None

def run(exe,output,seconds):
    output.mkdir(parents=True,exist_ok=True)
    capture=uuid.uuid4().hex
    args=[str(exe),'--synthetic','--capture-id',capture,'--seconds',str(seconds),'--rate','2000','--bins','4096','--queue','8','--clients','2','--wait-clients','2','--context-every','1000','--unclean-every','997']
    source=subprocess.Popen(args,stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True,encoding='utf-8')
    children=[];files=[];start=time.monotonic();baseline=peak=last_rss=0;cpu=0;last_uses={};mem={}
    try:
        banner=source.stdout.readline().strip();ready=source.stdout.readline().strip()
        if banner!='SYNTHETIC ONLY; no PL/DMA/RF access' or not ready.startswith('READY '):raise RuntimeError('source did not start explicitly synthetic: '+banner+ready)
        address=ready[6:]
        for name,delay in [('fast',0),('slow',20)]:
            log=(output/(name+'.log')).open('w',encoding='utf-8');files.append(log)
            pid_file=output/(name+'.pid')
            if pid_file.exists():raise FileExistsError('Use a fresh evidence directory: '+str(pid_file))
            cmd=[sys.executable,str(pathlib.Path(__file__).with_name('live.py')),'--connect',address,'--expect-source','synthetic','--verify-synthetic','--context-every','1000','--unclean-every','997','--read-delay-ms',str(delay),'--seconds',str(seconds+25),'--output',str(output/(name+'.json')),'--pid-file',str(pid_file)]
            children.append(subprocess.Popen(cmd,stdout=log,stderr=subprocess.STDOUT))
        while source.poll() is None or any(p.poll() is None for p in children):
            elapsed=time.monotonic()-start
            if elapsed>seconds+30:raise TimeoutError('bounded stress timeout')
            for name,p in [('source',source),*zip(['fast','slow'],children)]:
                pid=p.pid
                if name!='source':
                    pid_file=output/(name+'.pid')
                    if not pid_file.exists():continue
                    pid=int(pid_file.read_text(encoding='ascii'))
                usage=process_usage(pid)
                if usage:
                    rss,used=usage;last_uses[name]=usage
                    m=mem.setdefault(name,dict(pid=pid,launcher_pid=p.pid,peak=0,warm_min=None,warm_max=0))
                    m['peak']=max(m['peak'],rss)
                    if 3<elapsed<seconds:
                        m['warm_min']=rss if m['warm_min'] is None else min(m['warm_min'],rss);m['warm_max']=max(m['warm_max'],rss)
            time.sleep(.1)
        text,err=source.communicate(timeout=2)
        (output/'source.log').write_text(banner+'\n'+ready+'\n'+text+err,encoding='utf-8')
        if source.returncode or any(p.returncode for p in children):raise RuntimeError('stress subprocess failed: inspect logs')
        src=json.loads(text.strip().splitlines()[-1]);fast=json.loads((output/'fast.json').read_text());slow=json.loads((output/'slow.json').read_text())
        assert src['source']=='synthetic' and src['produced']==src['published']+src['contention_drops']
        assert src['produced']>=seconds*1900,(src,'source did not progress at requested test rate')
        assert src['client_missed']>0 and slow['gaps']>0,(src,slow,'slow client did not exercise ring overwrite')
        assert fast['observations']>slow['observations']>0
        assert fast['capture_id']==slow['capture_id']==capture
        assert src['partial_disconnects']==src['sender_errors']==0,src
        assert fast['observations']+slow['observations']==src['sent_records'],(src,fast,slow)
        assert sum(r['gaps']+r['first_seq']+(src['produced']-1-r['last_seq']) for r in (fast,slow))==src['client_missed']+2*src['contention_drops'],(src,fast,slow,'drop accounting')
        for name,m in mem.items():
            if seconds>5:
                assert m['warm_min'] is not None and m['warm_max']-m['warm_min']<16*1024*1024,(name,m,'RSS growth beyond bounded stress allowance')
            m['last_cpu_seconds']=last_uses[name][1]
        for name,r in [('fast',fast),('slow',slow)]:
            assert mem[name]['pid']==r['pid'],(name,'RSS sampled from wrong process')
        report=dict(kind='explicit synthetic TCP; no hardware claims',source=src,fast=fast,slow=slow,memory=mem,wall_seconds=time.monotonic()-start,commands=dict(source=args))
        (output/'stress.json').write_text(json.dumps(report,indent=2)+'\n',encoding='utf-8');print(json.dumps(report,indent=2));return report
    finally:
        for p in [source,*children]:
            if p.poll() is None:p.terminate()
        for p in [source,*children]:
            try:p.wait(timeout=3)
            except subprocess.TimeoutExpired:p.kill();p.wait(timeout=3)
        for f in files:f.close()

if __name__=='__main__':
    p=argparse.ArgumentParser();p.add_argument('executable',type=pathlib.Path);p.add_argument('output',type=pathlib.Path);p.add_argument('--seconds',type=float,default=30);a=p.parse_args()
    if not 1<=a.seconds<=240:p.error('1..240 seconds required')
    run(a.executable.resolve(),a.output.resolve(),a.seconds)
