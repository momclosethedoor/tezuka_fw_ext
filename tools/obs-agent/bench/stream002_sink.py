"""Bounded TCP sink for the ARM-SCAN-STREAM-002 bench protocols."""
import argparse
import json
import socket
import struct
import time
from pathlib import Path


ROI = struct.Struct("<8sIHbb8Q")
COMPACT = struct.Struct("<8s7Q")
OBN3 = struct.Struct("<IHHQQQIIIIIIQ")
FIELD = struct.Struct("<4sII")
BIN_HZ = 15_000
COMPACT_BYTES = 24_704


def exact(conn, size, eof=False):
    out = bytearray(size)
    offset = 0
    while offset < size:
        got = conn.recv_into(memoryview(out)[offset:])
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
    magic, size, count, phase, side, generation, version, map_gen, anchor, position, turn, profile, center = (
        ROI.unpack(header))
    if (magic != b"GROI1\x00\x00\x00" or size != COMPACT_BYTES or not 1 <= count <= 12 or
            phase not in (0, 1) or side not in (-1, 1) or min(generation, version, map_gen, anchor) == 0):
        raise ValueError("invalid ROI frame metadata")
    ranges = tuple(struct.iter_unpack("<HH", exact(conn, 4 * count)))
    ordinal, _, epoch, _, _, _ = compact_header(exact(conn,size))
    if epoch<anchor:
        raise ValueError("ROI observation precedes map anchor")
    selected = 0
    prev = 0
    for lo, hi in ranges:
        if not prev <= lo < hi <= 4096:
            raise ValueError("invalid or overlapping ROI intervals")
        for index in (lo, hi-1):
            hz = profile + side * (index - 2048) * BIN_HZ
            if not center - 28_000_000 <= hz < center + 28_000_000:
                raise ValueError("bin outside 56 MHz window")
        selected += hi-lo
        prev = hi
    return ordinal, selected, ROI.size + 4 * count + size


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
                        head = exact(conn, header.size, eof=True)
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
                except (EOFError, socket.timeout):
                    summary["incomplete"] += 1
                except (ValueError, KeyError, struct.error):
                    summary["errors"] += 1
    summary.update(elapsed_s=time.monotonic() - start, cpu_s=time.process_time() - cpu)
    print(json.dumps(summary))


if __name__ == "__main__":
    main()
