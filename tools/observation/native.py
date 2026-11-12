"""Independent OBN1-r1 reference decode. Offline simulations are never hardware provenance."""
import struct
from dataclasses import dataclass
ABI=0x314e424f
BINS=4096
BYTES=32768
COEFFICIENT_SUM=750668
ENBW=4096*275837336/(COEFFICIENT_SUM**2)

def ram_records(data):
    """One completed contiguous RAM-load extent, never a modulo-ring rotation.

    Caller proves stability/series ownership. No suffix survives a call. Production
    additionally requires all eight slots, loss=0 and matching completion seq.
    The excluded prefix/suffix together cost one record, not a native DMA loss.
    """
    if not BYTES<=len(data)<=8*BYTES or len(data)%BYTES:
        raise ValueError('RAM extent requires 1..8 complete adjacent slots')
    first=(BINS-544)*8
    result=tuple(bytes(data[first+j*BYTES:first+(j+1)*BYTES])
                 for j in range(len(data)//BYTES-1))
    previous=None
    for record in result:
        n=decode(record)
        if previous is not None and n.seq!=previous+1:
            raise ValueError('unrelated records inside RAM extent')
        previous=n.seq
    return result
@dataclass(frozen=True)
class Native:
    seq:int
    epoch:int
    first_tick:int
    clean:bool
    use_ddc:bool
    power:tuple

def decode(data):
    if len(data)!=BYTES:raise ValueError('native record size')
    h=bytearray(64)
    for j in range(32):h[2*j:2*j+2]=data[8*j+6:8*j+8]
    magic,rev,size,seq,epoch,tick,bins,nint,hz,flags,encoding,length,reserved=struct.unpack('<IHHQQQIIIIIIQ',h)
    if (magic,rev,size,bins,nint,hz,encoding,length,reserved)!=(ABI,1,64,4096,1,62500000,1,32768,0):raise ValueError('native ABI/header constants')
    if flags&~3 or bool(flags&1)!=bool(epoch) or tick==(1<<64)-1:raise ValueError('native flags/epoch/tick')
    words=struct.unpack('<4096Q',data)
    if any((w>>40)&255 or j>=32 and w>>48 or (w>>37)&7>4 for j,w in enumerate(words)):raise ValueError('native reserved bits/exponent')
    values=[]
    scale=1/(COEFFICIENT_SUM**2)
    for k in range(4096):
        # String reversal is an intentionally independent reference, not the Rust bit intrinsic.
        j=int(f'{(k+2048)%4096:012b}'[::-1],2);w=words[j]
        v=(w&((1<<37)-1))*(1<<(2*((w>>37)&7)))*scale
        values.append(struct.unpack('<f',struct.pack('<f',v))[0])
    return Native(seq,epoch,tick,bool(flags&1),bool(flags&2),tuple(values))
