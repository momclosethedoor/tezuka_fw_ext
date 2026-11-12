"""Receive-loop bridge: validated GOBS1 avg/peak -> one precomputed host row.

No socket owner, thread, drawing timer, calibration or legacy Maia conversion.
Construct a new bridge per capture UUID; the caller owns descriptor generation.
Use LiveReader(avg_peak=True), then accept(frame, descriptor, generation).
Times remain source-clock seconds; a UI/wall-clock anchor is a separate task.
"""
import hashlib
import struct
import numpy as np


class GobsRows:
    def __init__(self, rows, capture_id):
        if len(capture_id) != 16 or not any(capture_id):
            raise ValueError('nonzero capture UUID required')
        self.rows = rows
        self.capture_id = bytes(capture_id)
        self.last_seq = None
        self.signature = None
        self.measurements = self.repeated = self.unusable = self.gaps = 0

    def accept(self, frame, descriptor, generation):
        """Frame payload is borrowed: append/copy it before the next reader read."""
        d, f = descriptor, frame
        if d.source != 3 or d.mode != 3 or not d.tick_hz or f.descriptor_id != d.id:
            raise ValueError('hardware avg/peak descriptor required')
        signature = hashlib.sha256(struct.pack('<QQQII', f.seq, f.epoch, f.first_tick, f.nint, f.tag)
                                   + repr(d).encode() + bytes(f.payload)).digest()
        if self.last_seq is not None and f.seq <= self.last_seq:
            if f.seq == self.last_seq and signature == self.signature:
                self.repeated += 1
                return False
            raise ValueError('out of order/conflicting observation; new source needs new bridge')
        values = np.frombuffer(f.payload, dtype='<f4')
        if values.size != 2*d.bins:
            raise ValueError('avg/peak payload size')
        if not f.epoch or not np.isfinite(values).all():
            self._advance(f.seq, signature)
            self.unusable += 1
            return False
        t0 = f.first_tick / d.tick_hz
        self.rows.append_precomputed(values[:d.bins], values[d.bins:], t0,
                                     t0 + f.nint*d.bins/d.sample_rate,
                                     d.bin0_hz + d.sample_rate/2, d.sample_rate,
                                     generation, f.epoch, f.nint)
        self._advance(f.seq, signature)
        self.measurements += 1
        return True

    def _advance(self, seq, signature):
        if self.last_seq is not None:
            self.gaps += seq - self.last_seq - 1
        self.last_seq, self.signature = seq, signature
