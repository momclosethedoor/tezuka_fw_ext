"""Independent observation-v1 reader/writer. No host project or numpy dependency."""
from dataclasses import dataclass
import math,struct,uuid
MAGIC=b'GOBS1\r\n\0'
UNKNOWN=(1<<64)-1
NO_TAG=(1<<32)-1
MAX_BINS=65536
PREFIX=struct.Struct('<IHH')
DESC=struct.Struct('<dddQIHH')
OBS=struct.Struct('<QQQII')
@dataclass(frozen=True)
class Descriptor:
    id:int
    sample_rate:float
    bin0_hz:float
    enbw_bins:float
    tick_hz:int
    bins:int
    mode:int # 1 mean, 2 peak, 3 mean then peak
    source:int # 1 synthetic, 2 legacy WS, 3 hardware-stamped
@dataclass(frozen=True)
class Observation:
    descriptor_id:int
    seq:int
    epoch:int
    first_tick:int
    nint:int
    tag:int
    power:tuple
    @property
    def attributed(self): return self.epoch!=0
@dataclass
class Capture:
    session:bytes
    records:list

def _float(v,unknown=False):
    if math.isnan(v):
        if unknown:return
        raise ValueError('unknown float not allowed')
    if not math.isfinite(v):raise ValueError('nonfinite descriptor')
def validate_descriptor(d):
    if not 1<=d.id<=65535:raise ValueError('descriptor ID')
    if not 2<=d.bins<=MAX_BINS or d.bins&(d.bins-1):raise ValueError('FFT size')
    if d.mode not in (1,2,3) or d.source not in (1,2,3):raise ValueError('mode/source')
    _float(d.sample_rate);_float(d.bin0_hz,True);_float(d.enbw_bins)
    if d.sample_rate<=0 or not 0<d.enbw_bins<=d.bins:raise ValueError('rate/ENBW')
    if not 0<=d.tick_hz<UNKNOWN:raise ValueError('clock')
    if d.source==2 and (d.tick_hz!=0 or not math.isnan(d.bin0_hz)):raise ValueError('legacy has no authenticated RF grid/clock')
    if d.source!=2 and not math.isfinite(d.bin0_hz):raise ValueError('attributed grid required')
def validate_observation(o,d):
    if o.nint==0 and d.source!=2:raise ValueError('unknown nint only for legacy')
    if not 0<=o.nint<=0xffffffff:raise ValueError('nint')
    if d.tick_hz==0 and o.first_tick!=UNKNOWN or d.tick_hz!=0 and o.first_tick==UNKNOWN:raise ValueError('counter availability')
    if d.source==2:
        if o.epoch!=0 or o.tag not in range(8):raise ValueError('legacy epoch/tag')
    elif o.tag!=NO_TAG:raise ValueError('legacy tag outside legacy source')
    if o.epoch and d.source==2:raise ValueError('unproven legacy epoch')
    if len(o.power)!=d.bins*(2 if d.mode==3 else 1):raise ValueError('payload length')
    if any(math.isinf(x) or x<0 for x in o.power):raise ValueError('invalid power')
def _canonical_float(data,width):
    fmt='<I' if width==4 else '<Q';ff='<f' if width==4 else '<d'
    canonical=0x7fc00000 if width==4 else 0x7ff8000000000000
    v=struct.unpack(ff,data)[0]
    if math.isnan(v) and struct.unpack(fmt,data)[0]!=canonical:raise ValueError('noncanonical NaN')
    return v

def read(stream):
    """Read bounded records from a binary file-like object; preserve delivery order."""
    def exact(n):
        chunks=bytearray()
        while len(chunks)<n:
            b=stream.read(n-len(chunks))
            if not b:raise ValueError('truncated capture/record')
            chunks+=b
        return bytes(chunks)
    if exact(8)!=MAGIC:raise ValueError('capture magic/version')
    session=exact(16)
    if session==bytes(16):raise ValueError('zero capture UUID')
    records=[];descriptors={};epochs={};seqs={}
    while True:
        prefix=stream.read(1)
        if not prefix:break
        prefix+=exact(7)
        size,kind,id=PREFIX.unpack(prefix)
        if not 1<=id<=65535 or not 8<=size<=40+8*MAX_BINS:raise ValueError('record bounds')
        if kind==1:
            if size!=48 or id in descriptors:raise ValueError('descriptor size/redefinition')
            body=exact(40)
            fs=_canonical_float(body[0:8],8); f0=_canonical_float(body[8:16],8); enbw=_canonical_float(body[16:24],8)
            _,_,_,hz,n,mode,source=DESC.unpack(body)
            d=Descriptor(id,fs,f0,enbw,hz,n,mode,source);validate_descriptor(d)
            descriptors[id]=d;records.append(d)
        elif kind==2:
            if id not in descriptors:raise ValueError('missing descriptor')
            d=descriptors[id];expected=40+4*d.bins*(2 if d.mode==3 else 1)
            if size!=expected:raise ValueError('observation size')
            body=exact(size-8);seq,epoch,tick,nint,tag=OBS.unpack(body[:32])
            values=tuple(_canonical_float(body[i:i+4],4) for i in range(32,len(body),4))
            o=Observation(id,seq,epoch,tick,nint,tag,values);validate_observation(o,d)
            if epoch and epochs.setdefault(epoch,id)!=id:raise ValueError('RF epoch changed descriptor')
            wire=prefix+body
            if seq in seqs and seqs[seq]!=wire:raise ValueError('conflicting observation ID')
            seqs[seq]=wire;records.append(o)
        else:raise ValueError('unknown record kind')
    return Capture(session,records)

def decode(data):
    import io
    return read(io.BytesIO(data))
def encode(c):
    out=bytearray(MAGIC+c.session)
    for r in c.records:
        if isinstance(r,Descriptor):
            # Canonical unavailable RF bin0.
            f0=struct.pack('<Q',0x7ff8000000000000) if math.isnan(r.bin0_hz) else struct.pack('<d',r.bin0_hz)
            body=struct.pack('<d',r.sample_rate)+f0+struct.pack('<dQIHH',r.enbw_bins,r.tick_hz,r.bins,r.mode,r.source);kind=1;id=r.id
        else:
            body=OBS.pack(r.seq,r.epoch,r.first_tick,r.nint,r.tag)
            body+=b''.join(struct.pack('<I',0x7fc00000) if math.isnan(v) else struct.pack('<f',v) for v in r.power);kind=2;id=r.descriptor_id
        out+=PREFIX.pack(8+len(body),kind,id)+body
    result=bytes(out);decode(result) # Validate context, identities and bounds as well as scalars.
    return result

def load_vectors(path):
    return bytes.fromhex(path.read_text(encoding='ascii'))
