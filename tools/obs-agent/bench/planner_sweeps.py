"""Check independent A/B completion with the active map and GVIEW4 epochs."""
from collections import defaultdict
import json
import struct
from time import perf_counter_ns


class Sweeps:
    def __init__(self, actual, path):
        self.actual = actual
        self.desc = actual["spectrum"]
        self.path = path
        self.first = self.desc["map_epoch"]
        self.rows = self.desc["positions"]
        self.views = defaultdict(dict)
        self.notes = {}
        self.notes_out_of_order = 0

    def accept(self, data):
        host = perf_counter_ns()
        if data[:8] == b"GSWEEP1\0":
            assert len(data) == 112
            generation, version, anchor, sweep, phase, lo, hi, first, last, tick, expected, observed, flags = (
                struct.unpack_from("<13Q", data, 8))
            if generation != self.actual["generation"]:
                return
            assert version == self.desc["version"] and anchor == self.first
            assert phase == sweep % 2 and lo <= hi
            required = [p for p,row in enumerate(self.rows)
                        if not row["transition_only"] and row["sweep_phase"] == "AB"[phase]]
            base = anchor + (sweep // 2) * len(self.rows)
            assert expected == len(required) and (lo,hi)==(base+required[0],base+required[-1])
            assert sweep not in self.notes
            if self.notes and sweep <= max(self.notes):
                self.notes_out_of_order += 1
            self.notes[sweep] = dict(sweep=sweep,phase="AB"[phase],lo=lo,hi=hi,first=first,last=last,
                tick=tick,expected=expected,observed=observed,executed=bool(flags&1),
                tx_drop=flags>>1,host_ns=host)
        elif data[:8] == b"GVIEW4\0\0":
            assert len(data)%576==0
            for offset in range(0,len(data),576):
                ordinal,_,epoch,tick,_,_,generation=struct.unpack_from("<7Q",data,offset+8)
                if generation!=self.actual["generation"] or epoch<self.first:
                    continue
                cycle,index=divmod(epoch-self.first,len(self.rows))
                row=self.rows[index]
                if not row["transition_only"]:
                    sweep=2*cycle+"AB".index(row["sweep_phase"])
                    self.views[sweep].setdefault(epoch,dict(ordinal=ordinal,tick=tick,host_ns=host))

    def result(self):
        out=[]
        for sweep,note in sorted(self.notes.items()):
            view=self.views[sweep]
            if view:
                first,last=view[min(view)],view[max(view)]
                note.update(first_record_host_ns=first["host_ns"],last_record_host_ns=last["host_ns"],
                            first_record_tick=first["tick"],last_record_tick=last["tick"])
            note.update(received=len(view),missing=note["expected"]-len(view),
                        delivery_complete=note["executed"] and len(view)==note["expected"])
            out.append(note)
        self.path.write_text(json.dumps(dict(passes=out,out_of_order=self.notes_out_of_order),indent=2))
        return dict(passes=len(out),passes_complete=sum(n["delivery_complete"] for n in out),
                    tail_incomplete=sum(not n["delivery_complete"] for n in out))
