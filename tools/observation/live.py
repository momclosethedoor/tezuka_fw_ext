"""Bounded incremental GOBS1 reader. Offline unordered replay is in obs.py.

Record payload is borrowed from one reusable bytearray until the next read.
There is no Capture, lifetime dedup set, epoch map, or cached payload history.
"""
import argparse, json, math, os, socket, struct, sys, time
from dataclasses import dataclass
from observation import MAGIC, UNKNOWN, MAX_BINS, Descriptor, validate_descriptor
MAX_RECORD=40+8*MAX_BINS
MAX_DESCRIPTORS=16

@dataclass(frozen=True)
class Frame:
    descriptor_id:int
    seq:int
    epoch:int
    first_tick:int
    nint:int
    tag:int
    payload:memoryview
    def power(self):
        return (v[0] for v in struct.iter_unpack('<f',self.payload))

class RecordReader:
    """Exact fragmented reads with strict bounds; descriptors precede observations."""
    def __init__(self,stream):
        self.stream=stream
        self.buffer=bytearray(MAX_RECORD)
        self.descriptors={}
        self.started=False
        self._fill(0,24)
        if self.buffer[:8]!=MAGIC or self.buffer[8:24]==bytes(16):
            raise ValueError('capture magic/UUID')
        self.session=bytes(self.buffer[8:24])
    def _fill(self,start,end,eof=False):
        view=memoryview(self.buffer)
        while start<end:
            if hasattr(self.stream,'recv_into'):n=self.stream.recv_into(view[start:end])
            else:n=self.stream.readinto(view[start:end])
            if not n:
                if eof:return False
                raise ValueError('truncated record/capture')
            start+=n
            eof=False
        return True
    def next(self):
        b=self.buffer
        while True:
            if not self._fill(0,8,True):return None
            size,kind,id=struct.unpack_from('<IHH',b)
            if not 8<=size<=MAX_RECORD or not id:raise ValueError('record bounds')
            if kind==1:
                if size!=48 or self.started or len(self.descriptors)==MAX_DESCRIPTORS or id in self.descriptors:
                    raise ValueError('descriptor bounds/order/redefinition')
                self._fill(8,size)
                for i in (8,16,24):
                    v=struct.unpack_from('<d',b,i)[0]
                    if math.isnan(v) and struct.unpack_from('<Q',b,i)[0]!=0x7ff8000000000000:raise ValueError('noncanonical NaN')
                d=Descriptor(id,*struct.unpack_from('<dddQIHH',b,8))
                validate_descriptor(d)
                self.descriptors[id]=d
            elif kind==2:
                d=self.descriptors.get(id)
                if d is None:raise ValueError('missing descriptor')
                if size!=40+4*d.bins*(2 if d.mode==3 else 1):raise ValueError('observation size')
                self._fill(8,size)
                seq,epoch,tick,nint,tag=struct.unpack_from('<QQQII',b,8)
                if nint==0 and d.source!=2 or (d.tick_hz==0)!=(tick==UNKNOWN):raise ValueError('nint/counter availability')
                if d.source==2 and (epoch or tag>7) or d.source!=2 and tag!=0xffffffff:raise ValueError('epoch/tag provenance')
                payload=memoryview(b)[40:size]
                for (bits,) in struct.iter_unpack('<I',payload):
                    if bits&0x80000000 and bits!=0x80000000 or bits&0x7f800000==0x7f800000 and bits!=0x7fc00000:
                        raise ValueError('power/noncanonical NaN')
                self.started=True
                return Frame(id,seq,epoch,tick,nint,tag,payload)
            else:raise ValueError('unknown record kind')

class LiveReader:
    """One ordered source lifetime. Non-increasing seq is an error, never recounted.

    First observed seq sets the late-join baseline: leading losses are unknown.
    Epoch IDs are opaque. Epoch zero does not erase the last known epoch.
    Lifetime non-reuse/binding is a producer obligation; no historical epoch map.
    """
    def __init__(self,stream,expected_source=None,nint1=False,avg_peak=False):
        self.reader=RecordReader(stream)
        self.expected_source=expected_source
        self.nint1=nint1
        self.avg_peak=avg_peak
        self.contributing_ffts=0
        self.first_seq=self.last_seq=self.tick=None
        self.epoch=0;self.descriptor=0
        self.observations=self.gaps=self.epoch_boundaries=self.unattributed=0
    def next(self):
        f=self.reader.next()
        if f is None:return None
        d=self.reader.descriptors[f.descriptor_id]
        if self.expected_source is not None and d.source!=self.expected_source:raise ValueError('source provenance mismatch; no synthetic fallback')
        if self.nint1 and (d.mode==3 or f.nint!=1 or d.source==2 or d.tick_hz==0):raise ValueError('live nint=1 one-plane profile')
        if self.avg_peak and (d.mode!=3 or d.source==2 or not d.tick_hz or not f.epoch or not f.nint):raise ValueError('live attributed avg+peak profile')
        if self.last_seq is not None and f.seq<=self.last_seq:raise ValueError('non-increasing live seq; reset needs new capture')
        if f.epoch and f.epoch==self.epoch and f.descriptor_id!=self.descriptor:raise ValueError('live epoch binding')
        if f.first_tick!=UNKNOWN and self.tick is not None and f.first_tick<self.tick:raise ValueError('source counter reset needs new capture')
        if self.last_seq is None:self.first_seq=f.seq
        else:self.gaps+=f.seq-self.last_seq-1
        if f.epoch:
            if self.epoch and f.epoch!=self.epoch:self.epoch_boundaries+=1
            self.epoch=f.epoch;self.descriptor=f.descriptor_id
        else:self.unattributed+=1
        if f.first_tick!=UNKNOWN:self.tick=f.first_tick
        self.last_seq=f.seq;self.observations+=1
        self.contributing_ffts+=f.nint
        return f
    def summary(self):
        return dict(capture_id=self.reader.session.hex(),observations=self.observations,
                    first_seq=self.first_seq,last_seq=self.last_seq,gaps=self.gaps,
                    epoch_boundaries=self.epoch_boundaries,unattributed=self.unattributed,contributing_ffts=self.contributing_ffts,
                    payload_buffer_bytes=len(self.reader.buffer),descriptors=len(self.reader.descriptors))

def verify_synthetic(f,d,context_every,unclean_every):
    """Independent oracle for the explicit exerciser, not a PL model."""
    epoch=1+f.seq//context_every
    if f.descriptor_id!=1+(epoch-1)%2 or f.epoch!=(0 if unclean_every and f.seq%unclean_every==0 else epoch):raise ValueError('synthetic context corruption')
    if f.first_tick!=f.seq*d.bins or f.nint!=1 or d.source!=1:raise ValueError('synthetic counter/count/provenance')
    for k,value in enumerate(f.power()):
        if value!=((f.seq%1024)*16+k%16)/16384:raise ValueError('synthetic payload/seq corruption')

def main():
    p=argparse.ArgumentParser(description='Headless GOBS1 live reader; hardware by default, never a fallback source')
    p.add_argument('--connect',required=True,help='host:port')
    p.add_argument('--expect-source',choices=['hardware','synthetic'],default='hardware')
    p.add_argument('--profile',choices=['single','avg-peak'],default='single')
    p.add_argument('--host-rows-root',help='optional existing goofy_v2 root; direct precomputed rows, no GUI/server')
    p.add_argument('--row-ring-rows',type=int,default=64)
    p.add_argument('--read-delay-ms',type=float,default=0)
    p.add_argument('--seconds',type=float,default=240,help='maximum reader scenario duration, <=270')
    p.add_argument('--verify-synthetic',action='store_true')
    p.add_argument('--context-every',type=int,default=10000)
    p.add_argument('--unclean-every',type=int,default=0)
    p.add_argument('--output',help='JSON summary path')
    p.add_argument('--pid-file',help='Instrumentation: write the actual reader PID, including behind a venv launcher')
    a=p.parse_args()
    if not 0<a.seconds<=270 or not 0<=a.read_delay_ms<=1000 or a.context_every<=0 or a.unclean_every<0:p.error('duration/delay/context bounds')
    if a.verify_synthetic and a.expect_source!='synthetic':p.error('synthetic oracle requires explicit synthetic provenance')
    if not 2<=a.row_ring_rows<=4096:p.error('row ring bounds 2..4096')
    if a.host_rows_root and (a.profile!='avg-peak' or a.expect_source!='hardware'):p.error('host row bridge requires hardware avg-peak profile')
    if a.pid_file:
        import pathlib
        pathlib.Path(a.pid_file).write_text(str(os.getpid()),encoding='ascii')
    host,port=a.connect.rsplit(':',1);start=time.monotonic();cpu=time.process_time()
    with socket.create_connection((host,int(port)),timeout=10) as s:
        s.settimeout(min(10,a.seconds))
        r=LiveReader(s,expected_source=1 if a.expect_source=='synthetic' else 3,nint1=a.profile=='single',avg_peak=a.profile=='avg-peak')
        bridge=None
        if a.host_rows_root:
            import sys
            sys.path.insert(0,a.host_rows_root)
            from sdr.dsp.rows import RowHistory
            from sdr.source.gobs_rows import GobsRows
            bridge=GobsRows(RowHistory(ring_rows=a.row_ring_rows),r.reader.session)
        while time.monotonic()-start<a.seconds:
            f=r.next()
            if f is None:break
            if a.verify_synthetic:verify_synthetic(f,r.reader.descriptors[f.descriptor_id],a.context_every,a.unclean_every)
            if bridge:bridge.accept(f,r.reader.descriptors[f.descriptor_id],f.descriptor_id)
            if a.read_delay_ms:time.sleep(a.read_delay_ms/1000)
        else:raise TimeoutError('scenario duration limit')
    result=dict(r.summary(),source=a.expect_source,pid=os.getpid(),seconds=time.monotonic()-start,cpu_seconds=time.process_time()-cpu,
                synthetic_oracle=a.verify_synthetic,read_delay_ms=a.read_delay_ms)
    if bridge:result['host_rows']=dict(bridge.rows.status(),measurements=bridge.measurements,repeated=bridge.repeated)
    if a.output:
        import pathlib
        pathlib.Path(a.output).write_text(json.dumps(result,indent=2)+'\n',encoding='utf-8')
    print(json.dumps(result))
if __name__=='__main__':main()
