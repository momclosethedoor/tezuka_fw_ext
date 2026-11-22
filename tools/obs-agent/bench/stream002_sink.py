"""Bounded TCP sink for the ARM-SCAN-STREAM-002 bench protocols."""
import argparse
import json
import socket
import struct
import time


ROI = struct.Struct("<8sIHbb8Q")
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


def roi_frame(conn, header):
    magic, size, count, phase, side, generation, version, map_gen, anchor, position, turn, profile, center = (
        ROI.unpack(header))
    if (magic != b"GROI1\x00\x00\x00" or size != COMPACT_BYTES or not 1 <= count <= 12 or
            phase not in (0, 1) or side not in (-1, 1) or min(generation, version, map_gen, anchor) == 0):
        raise ValueError("invalid ROI frame metadata")
    ranges = tuple(struct.iter_unpack("<HH", exact(conn, 4 * count)))
    slot = exact(conn, size)
    abi, revision, header_size, seq, epoch, tick, bins, k, tick_hz, flags, encoding, length, ordinal = (
        OBN3.unpack_from(slot))
    if ((abi, revision, header_size, bins, encoding, length, tick_hz) !=
            (0x334E424F, 4, 64, 4096, 3, COMPACT_BYTES, 62_500_000) or
            not 1 <= k <= 1023 or flags & ~0x3f or flags & 3 != 1 or any(slot[64:128]) or epoch < anchor):
        raise ValueError("invalid OBN3 compact header")
    max_m = (1 if flags & 4 else k) << 35
    scale = 1.0 / (750668**2 * (1 if flags & 4 else k))
    power = []
    for low, high in struct.iter_unpack("<IH", memoryview(slot)[128:]):
        exponent = high >> 13
        mantissa = low | ((high & 8191) << 32)
        if exponent > 4 or mantissa > max_m:
            raise ValueError("invalid compact power")
        power.append((mantissa << (2 * exponent)) * scale)
    selected = []
    prev = 0
    for lo, hi in ranges:
        if not prev <= lo < hi <= len(power):
            raise ValueError("invalid or overlapping ROI intervals")
        for index in range(lo, hi):
            hz = profile + side * (index - 2048) * BIN_HZ
            if not center - 28_000_000 <= hz < center + 28_000_000:
                raise ValueError("bin outside 56 MHz window")
            selected.append((hz, power[index]))
        prev = hi
    return ordinal, len(selected), ROI.size + 4 * count + size


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
    parser.add_argument("mode", choices=("roi", "field"))
    parser.add_argument("port", type=int)
    parser.add_argument("--bind", default="0.0.0.0")
    parser.add_argument("--seconds", type=float, default=60)
    args = parser.parse_args()
    if not 0 < args.seconds <= 300:
        parser.error("--seconds must be in (0, 300]")
    start, cpu = time.monotonic(), time.process_time()
    summary = dict(mode=args.mode, complete=0, processed=0, wire_bytes=0, bins=0,
                   errors=0, incomplete=0, connections=0, outcomes=[0] * 8)
    with socket.create_server((args.bind, args.port)) as listener:
        print("READY", flush=True)
        listener.settimeout(1)
        while time.monotonic() - start < args.seconds:
            try:
                conn, _ = listener.accept()
            except socket.timeout:
                continue
            summary["connections"] += 1
            previous = None
            with conn:
                conn.settimeout(2)
                try:
                    while time.monotonic() - start < args.seconds:
                        head = exact(conn, (ROI if args.mode == "roi" else FIELD).size, eof=True)
                        if head is None:
                            break
                        if args.mode == "roi":
                            ordinal, bins, length = roi_frame(conn, head)
                            if previous is not None and ordinal <= previous:
                                raise ValueError("reordered or duplicate ordinal")
                            previous = ordinal
                            summary["bins"] += bins
                        else:
                            meta, length = field_frame(conn, head)
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
