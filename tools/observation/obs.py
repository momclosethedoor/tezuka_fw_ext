"""Offline replay and explicitly untrusted legacy import; never contacts hardware."""
import argparse,hashlib,json,math,pathlib,struct,time,uuid
from observation import Capture,Descriptor,Observation,UNKNOWN,NO_TAG,decode,encode,load_vectors

def packet(o):
    return struct.pack('<HQQQII',o.descriptor_id,o.seq,o.epoch,o.first_tick,o.nint,o.tag)+b''.join(struct.pack('<I',0x7fc00000) if math.isnan(v) else struct.pack('<f',v) for v in o.power)

def replay(c,plan=None,delay_ms=0.,sleep=time.sleep,emit=lambda x:None):
    if not math.isfinite(delay_ms) or delay_ms<0:raise ValueError('delivery delay')
    ds={r.id:r for r in c.records if isinstance(r,Descriptor)}
    obs=[r for r in c.records if isinstance(r,Observation)];by_seq={r.seq:r for r in obs}
    schedule=plan if plan is not None else [{'seq':r.seq} for r in obs]
    seen={};deliveries=[];repeated=0;start=time.monotonic()
    for step in schedule:
        seq=step['seq']
        if not isinstance(seq,int) or isinstance(seq,bool) or seq not in by_seq:raise ValueError('unknown plan sequence')
        delay=step.get('delay_ms',delay_ms)
        if not isinstance(delay,(int,float)) or not math.isfinite(delay) or delay<0:raise ValueError('delivery delay')
        if delay:sleep(delay/1000.)
        o=by_seq[seq];wire=packet(o);duplicate=seq in seen
        if duplicate and seen[seq]!=wire:raise ValueError('conflicting ID')
        if duplicate:repeated+=1
        seen[seq]=wire;deliveries.append(o);d=ds[o.descriptor_id]
        emit({'event':'delivery','capture_id':str(uuid.UUID(bytes=c.session)),'seq':seq,'epoch':o.epoch or None,'descriptor_id':o.descriptor_id,'source':{1:'synthetic',2:'legacy-ws',3:'hardware-stamped'}[d.source],'first_tick':None if o.first_tick==UNKNOWN else o.first_tick,'nint':o.nint or None,'legacy_tag':None if o.tag==NO_TAG else o.tag,'sample_volume':o.nint*d.bins if o.nint else None,'attributed_sample_volume':o.nint*d.bins if o.nint and o.epoch else None,'active_seconds':o.nint*d.bins/d.sample_rate if o.nint else None,'attributed':o.attributed,'repeat':duplicate,'delivery_elapsed_ms':round((time.monotonic()-start)*1000,3),'payload_sha256':hashlib.sha256(wire[34:]).hexdigest()})
    known=[by_seq[s] for s in sorted(seen) if by_seq[s].epoch]
    boundaries=[{'after_seq':a.seq,'before_seq':b.seq,'from_epoch':a.epoch,'to_epoch':b.epoch} for a,b in zip(known,known[1:]) if a.epoch!=b.epoch]
    gaps=max(seen)+1-len(seen) if seen else 0
    summary={'event':'summary','presentations':len(deliveries),'observations':len(seen),'repeated_presentations':repeated,'missing_seq_count':gaps,'epoch_boundaries':boundaries,'unattributed_observations':sum(by_seq[s].epoch==0 for s in seen)}
    emit(summary);return Capture(c.session,list(ds.values())+deliveries),summary

def legacy(npz,output,sample_rate,enbw,mode):
    import numpy as np # Only this optional command needs existing host numpy.
    src=pathlib.Path(npz);sha=hashlib.sha256(src.read_bytes()).hexdigest()
    if not math.isfinite(sample_rate) or sample_rate<=0:raise ValueError('sample rate')
    with np.load(src,allow_pickle=False) as z: frames=z['frames'].copy()
    if frames.ndim!=2 or not len(frames):raise ValueError('legacy frames shape')
    d=Descriptor(1,float(sample_rate),float('nan'),float(enbw),0,frames.shape[1],mode,2)
    k_db=-183.529+10*math.log10(sample_rate);scale=10**(k_db/10)
    records=[d]
    for ordinal,frame in enumerate(frames):
        tag=float(frame[0])
        if not tag.is_integer() or not 0<=tag<=7:raise ValueError('invalid legacy tag')
        values=tuple([float('nan')]+[float(np.float32(float(v)*scale)) for v in frame[1:]])
        records.append(Observation(1,ordinal,0,UNKNOWN,0,int(tag),values))
    c=Capture(uuid.uuid5(uuid.NAMESPACE_URL,'goofy:legacy-npz:sha256:'+sha).bytes,records)
    dest=pathlib.Path(output);dest.write_bytes(encode(c))
    meta={'origin':'legacy WS import, not new PL metadata','source_file_sha256':sha,'capture_id':str(uuid.UUID(bytes=c.session)),'seq_origin':'saved frame ordinal, not hardware sequence; no inference of hardware losses','RF_epoch':None,'hardware_time':None,'per_frame_nint':None,'RF_grid':None,'missing_bin':0,'mode_context':{1:'mean',2:'peak'}[mode],'power_conversion':{'sample_rate_context_hz':sample_rate,'enbw_context_bins':enbw,'formula':'linear FS^2 = old Maia WS power * 10^((-183.529 + 10log10(Fs))/10)'},'host_receive_timestamps':'not used or encoded','output_sha256':hashlib.sha256(dest.read_bytes()).hexdigest()}
    dest.with_suffix(dest.suffix+'.json').write_text(json.dumps(meta,indent=2)+'\n',encoding='utf-8')
    return meta

def main():
    p=argparse.ArgumentParser(description=__doc__);sub=p.add_subparsers(dest='command',required=True)
    r=sub.add_parser('replay');r.add_argument('input',type=pathlib.Path);r.add_argument('--plan',type=pathlib.Path);r.add_argument('--delay-ms',type=float,default=0.);r.add_argument('--output',type=pathlib.Path)
    l=sub.add_parser('legacy');l.add_argument('input');l.add_argument('output');l.add_argument('--sample-rate',type=float,required=True);l.add_argument('--enbw',type=float,required=True);l.add_argument('--mode',choices=['mean','peak'],required=True)
    a=p.parse_args()
    if a.command=='legacy':print(json.dumps(legacy(a.input,a.output,a.sample_rate,a.enbw,1 if a.mode=='mean' else 2)));return
    c=decode(load_vectors(a.input) if a.input.suffix=='.hex' else a.input.read_bytes())
    result,_=replay(c,json.loads(a.plan.read_text()) if a.plan else None,a.delay_ms,emit=lambda x:print(json.dumps(x,allow_nan=False)))
    if a.output:a.output.write_bytes(encode(result))
if __name__=='__main__':main()
