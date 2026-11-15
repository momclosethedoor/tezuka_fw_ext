"""Portable OBN2-r2 encoding goldens. Python 3 stdlib only; no FPGA tools.

These are byte-layout vectors, including representable mantissa extremes.
Real FFT/integer-accumulator equivalence is tested separately.
"""
import hashlib
import json
from pathlib import Path
import struct
import sys

MAGIC = 0x324E424F
HEADER = '<IHHQQQIIIIIIQ'


def encode(mantissa, exponent, *, seq, epoch, first_tick, actual_k,
           peak=False, source_ddc=False, ordinal=0):
    n = len(mantissa)
    assert n >= 64 and len(exponent) == n and 1 <= actual_k <= 1023
    header = struct.pack(HEADER, MAGIC, 2, 64, seq, epoch, first_tick, n,
        actual_k, 62500000, 1 | (int(source_ddc)<<1) | (int(peak)<<2),
        2, n*8, ordinal)
    assert len(header) == 64
    words = []
    for j, (m, e) in enumerate(zip(mantissa, exponent)):
        assert 0 <= int(m) < (1<<47) and 0 <= int(e) <= 4
        words.append(int(m) | (int(e)<<47) | ((header[j] if j<64 else 0)<<56))
    return struct.pack('<'+'Q'*n, *words)


def decode(raw):
    words = struct.unpack('<'+'Q'*(len(raw)//8), raw)
    header = bytes(w>>56 for w in words[:64])
    magic, rev, size, seq, epoch, tick, n, k, hz, flags, enc, size_bytes, ordinal = struct.unpack(HEADER,header)
    assert (magic,rev,size,enc,size_bytes,n*8)==(MAGIC,2,64,2,len(raw),len(raw))
    assert 1<=k<=1023 and hz==62500000 and flags&1 and not flags&~7 and epoch!=0
    assert all((w>>50)&63==0 for w in words)
    assert all(w>>56==0 for w in words[64:])
    exponent=[(w>>47)&7 for w in words]
    assert max(exponent)<=4
    return dict(seq=seq,epoch=epoch,first_tick=tick,actual_k=k,n=n,ordinal=ordinal,
                peak=bool(flags&4),source_ddc=bool(flags&2),
                mantissa=[w&((1<<47)-1) for w in words],exponent=exponent)


def generate(directory):
    directory=Path(directory);directory.mkdir(parents=True,exist_ok=True)
    manifest={'format':'OBN2-r2/encoding2','purpose':'byte-layout, not RF or FFT reference','vectors':[]}
    for i,(k,peak) in enumerate([(1,False),(2,False),(3,True),(1023,False)]):
        name=f'layout-k{k}-'+('peak' if peak else 'sum')+'.bin'
        mantissa=[((j+1)*0x123456789AB+i*0xFFEEDDCCBB)&((1<<47)-1) for j in range(4096)]
        mantissa[0]=0;mantissa[1]=(1<<47)-1
        exponent=[(j+i)%5 for j in range(4096)]
        params=dict(seq=0x1234567800000000+i*1024,epoch=0xFEDCBA9876543210+i,
                    first_tick=0xABCDEF0012340000+i,actual_k=k,peak=peak,
                    source_ddc=bool(i&1),ordinal=0x100000000+i)
        raw=encode(mantissa,exponent,**params)
        d=decode(raw)
        assert d['mantissa']==mantissa and d['exponent']==exponent
        (directory/name).write_bytes(raw)
        manifest['vectors'].append(dict(file=name,bytes=len(raw),sha256=hashlib.sha256(raw).hexdigest(),**params))
    (directory/'vectors.json').write_bytes((json.dumps(manifest,indent=2)+'\n').encode())
    return manifest


if __name__=='__main__':
    print(json.dumps(generate(sys.argv[1] if len(sys.argv)>1 else 'golden'),indent=2))
