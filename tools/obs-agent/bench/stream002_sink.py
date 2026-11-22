"""Bounded TCP sink for the ARM-SCAN-STREAM-002 bench protocols."""
import argparse
import json
from hashlib import sha256
import socket
import struct
import time
from pathlib import Path


ROI = struct.Struct("<8sIHbb8Q")
ROI_RANGE = struct.Struct("<QHH")
COMPACT = struct.Struct("<8s7Q")
OBN3 = struct.Struct("<IHHQQQIIIIIIQ")
FIELD = struct.Struct("<4sII")
BIN_HZ = 15_000
COMPACT_BYTES = 24_704
COMPACT_PREFIX_BYTES = 128


def exact(conn, size, eof=False):
    out = bytearray(size)
    offset = 0
    while offset < size:
        try:
            got = conn.recv_into(memoryview(out)[offset:])
        except socket.timeout:
            if eof and not offset:
                raise
            raise EOFError("partial frame timed out") from None
        if not got:
            if eof and not offset:
                return None
            raise EOFError("partial frame at connection close")
        offset += got
    return out


def compact_header(slot):
    abi, revision, header_size, seq, epoch, tick, bins, k, tick_hz, flags, encoding, length, ordinal = (
        OBN3.unpack_from(slot))
    if ((abi, revision, header_size, bins, encoding, length, tick_hz) !=
            (0x334E424F, 4, 64, 4096, 3, COMPACT_BYTES, 62_500_000) or
            not 1 <= k <= 1023 or flags & ~0x3f or flags & 3 != 1 or any(slot[64:128])):
        raise ValueError("invalid OBN3 compact header")
    return ordinal, seq, epoch, tick, k, flags


def compact_frame(conn, header):
    magic,*words,generation=COMPACT.unpack(header)
    if magic!=b"GCMP1\x00\x00\x00" or not generation:
        raise ValueError("invalid full compact frame")
    observed=compact_header(exact(conn,COMPACT_BYTES))
    if observed!=tuple(words):
        raise ValueError("full compact envelope and OBN3 identity differ")
    return observed[0],4096,COMPACT.size+COMPACT_BYTES


def roi_frame(conn, header):
    magic, size, count, phase, side, generation, version, map_gen, anchor, position, turn, profile, center_or_low = (
        ROI.unpack(header))
    if (magic not in (b"GROI1\x00\x00\x00", b"GROI2\x00\x00\x00") or not 1 <= count <= 12 or
            phase not in (0, 1) or side not in (-1, 1) or min(generation, version, map_gen, anchor) == 0):
        raise ValueError("invalid ROI frame metadata")
    packed = magic == b"GROI2\x00\x00\x00"
    if packed:
        high, source_bins, display_bins = ROI_RANGE.unpack(exact(conn, ROI_RANGE.size))
        low = center_or_low
        if (not low < high or source_bins not in (512, 1024, 2048, 4096) or
                display_bins not in (512, 1024, 2048, 4096)):
            raise ValueError("invalid ROI grids")
    else:
        if size != COMPACT_BYTES:
            raise ValueError("invalid full ROI record length")
        low, high = center_or_low - 28_000_000, center_or_low + 28_000_000
        source_bins = 4096
    ranges = tuple(struct.iter_unpack("<HH", exact(conn, 4 * count)))
    prefix_bytes = COMPACT_PREFIX_BYTES if packed else size
    ordinal, _, epoch, _, _, _ = compact_header(exact(conn,prefix_bytes))
    if epoch<anchor:
        raise ValueError("ROI observation precedes map anchor")
    selected = 0
    prev = 0
    for lo, hi in ranges:
        if not prev <= lo < hi <= source_bins:
            raise ValueError("invalid or overlapping ROI intervals")
        stride = 4096 // source_bins
        for index in (lo * stride, hi * stride - 1):
            hz = profile + side * (index - 2048) * BIN_HZ
            if not low <= hz < high:
                raise ValueError("bin outside the requested RF range")
        selected += hi-lo
        prev = hi
    if packed:
        if size != selected * (6 if source_bins == 4096 else 1):
            raise ValueError("packed ROI payload length differs from its intervals")
        exact(conn,size)
    return ordinal, selected, ROI.size + 4 * count + size + (
        ROI_RANGE.size + COMPACT_PREFIX_BYTES if packed else 0)


def field_frame(conn, header):
    magic, meta_bytes, length = FIELD.unpack(header)
    if magic != b"R8P2" or not 0 < meta_bytes < 512 or length > 337_924:
        raise ValueError("invalid FIELD frame size")
    meta = json.loads(exact(conn, meta_bytes))
    if meta["kind"] != "field" or not 1 <= meta["outcome"] <= 7 or meta["started"] not in (0, 1):
        raise ValueError("invalid FIELD outcome")
    exact(conn, length)
    if length != meta["consumed_field_bytes"] or length and (
            meta["outcome"] != 1 or meta["payload_status"] or
            length != (meta["end"] - meta["first"]) % (1 << 32)):
        raise ValueError("FIELD bytes differ from FFM1 extent")
    return meta, FIELD.size + meta_bytes + length


class FieldChunks:
    """Validate wire offsets and exact extents without storing a field."""

    def __init__(self):
        self.active = None
        self.received = 0
        self.digest = None

    def read(self, conn, header):
        magic, meta_bytes, length = FIELD.unpack(header)
        if magic != b"R8P2" or not 0 < meta_bytes < 512 or length > 262144:
            raise ValueError("invalid FSB1 frame size")
        meta = json.loads(exact(conn, meta_bytes))
        payload = exact(conn, length)
        kind = meta["kind"]
        if kind == "begin":
            if self.active is not None or length:
                raise ValueError("overlapping FSB1 attempts")
            self.active = meta
            self.received = 0
            self.digest = sha256()
        else:
            if self.active is None or meta["stream_tag"] != self.active["stream_tag"]:
                raise ValueError("FSB1 stream identity mismatch")
            if kind == "data":
                if not length or meta["offset"] != self.received:
                    raise ValueError("FSB1 DATA gap or overlap")
                self.digest.update(payload)
                self.received += length
            elif kind == "end":
                for key in ("epoch", "map_version", "target_position", "target_hz", "watch_generation", "watch_revision"):
                    if meta[key] != self.active[key]:
                        raise ValueError("FSB1 END changed visit identity")
                if length or meta["generation_bytes"] != self.received or not 1 <= meta["pl_outcome"] <= 7:
                    raise ValueError("FSB1 END byte count or outcome")
                if meta["qualified_field"]:
                    expected = (meta["source_end"] - meta["source_first"]) % (1 << 32)
                    if (meta["pl_outcome"] != 1 or meta["error"] or not expected or expected != self.received
                            or meta["source_first"] != self.active["source_first"]
                            or meta["first"] != 0 or meta["end"] != self.received):
                        raise ValueError("FSB1 SUCCESS extent mismatch")
                    meta["sha256"] = self.digest.hexdigest()
                self.active = None
            else:
                raise ValueError("invalid FSB1 frame kind")
        return meta, FIELD.size + meta_bytes + length, length


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("mode", choices=("compact", "roi", "field"))
    parser.add_argument("port", type=int)
    parser.add_argument("--bind", default="0.0.0.0")
    parser.add_argument("--seconds", type=float, default=60)
    parser.add_argument("--done-file", type=Path)
    args = parser.parse_args()
    if not 0 < args.seconds <= 300:
        parser.error("--seconds must be in (0, 300]")
    sensor={"compact": (COMPACT,compact_frame), "roi": (ROI,roi_frame), "field": (FIELD,field_frame)}
    header, receive=sensor[args.mode]
    start, cpu = time.monotonic(), time.process_time()
    def running():
        return time.monotonic()-start<args.seconds and not (args.done_file and args.done_file.exists())
    summary = dict(mode=args.mode, complete=0, processed=0, wire_bytes=0, bins=0,
                   errors=0, incomplete=0, connections=0, outcomes=[0] * 8)
    with socket.create_server((args.bind, args.port)) as listener:
        print("READY", flush=True)
        listener.settimeout(1)
        while running():
            try:
                conn, _ = listener.accept()
            except socket.timeout:
                continue
            summary["connections"] += 1
            previous = None
            with conn:
                conn.settimeout(2)
                try:
                    while running():
                        try:
                            head = exact(conn, header.size, eof=True)
                        except socket.timeout:
                            continue
                        if head is None:
                            break
                        if args.mode != "field":
                            ordinal, bins, length = receive(conn, head)
                            if previous is not None and ordinal <= previous:
                                raise ValueError("reordered or duplicate ordinal")
                            previous = ordinal
                            summary["bins"] += bins
                        else:
                            meta, length = receive(conn, head)
                            summary["outcomes"][meta["outcome"]] += 1
                        summary["wire_bytes"] += length
                        summary["complete"] += 1
                        summary["processed"] += 1
                except (EOFError, socket.timeout, ConnectionResetError):
                    summary["incomplete"] += 1
                except (ValueError, KeyError, struct.error):
                    summary["errors"] += 1
    summary.update(elapsed_s=time.monotonic() - start, cpu_s=time.process_time() - cpu)
    print(json.dumps(summary))


if __name__ == "__main__":
    main()
