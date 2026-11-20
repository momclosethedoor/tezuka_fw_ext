"""Decode all fixed-size GVIEW4 records, including coalesced UDP datagrams."""
import statistics
import struct


class Laps:
    def __init__(self, actual, timing_position=0):
        spectrum = actual["spectrum"]
        self.n = len(spectrum["positions"])
        self.anchor = spectrum["map_epoch"]
        self.generation = actual["generation"]
        self.timing_position = timing_position
        self.seen = set()
        self.boundaries = []
        self.ticks = []
        self.datagrams = self.records = self.missing = 0
        self.last = None

    def accept(self, data):
        assert len(data) and len(data) % 576 == 0, len(data)
        self.datagrams += 1
        for offset in range(0, len(data), 576):
            assert data[offset:offset+8] == b"GVIEW4\0\0"
            ordinal, _, epoch, tick, _, _, gen = struct.unpack_from("<7Q", data, offset+8)
            if gen != self.generation or epoch < self.anchor:
                continue
            pos = (epoch - self.anchor) % self.n
            self.seen.add(pos)
            if pos == self.timing_position and (not self.boundaries or epoch != self.boundaries[-1]):
                self.boundaries.append(epoch)
                self.ticks.append(tick)
            if self.last is not None:
                assert ordinal > self.last, (ordinal, self.last)
                self.missing += ordinal - self.last - 1
            self.last = ordinal
            self.records += 1

    def result(self):
        times = [(b-a)/62.5 for a,b in zip(self.ticks, self.ticks[1:])]
        return dict(datagrams=self.datagrams, records=self.records, missing_ordinals=self.missing,
                    covered_positions=len(self.seen), total=self.n, lap_us=times,
                    lap_median_us=statistics.median(times) if times else None)


def from_capture(path, actual):
    laps = Laps(actual)
    with path.open("rb") as capture:
        while size := capture.read(4):
            (n,) = struct.unpack("<I", size)
            data = capture.read(n)
            assert len(data) == n
            laps.accept(data)
    return laps.result()
