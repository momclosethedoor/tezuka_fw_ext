"""RAM-only TPC2 egress comparison with continuously reading benchmark sockets."""
import argparse
import json
import os
from pathlib import Path
import socket
import struct
import sys
import threading
import time
from urllib.error import HTTPError
import uuid

from planner_row99 import bench, stage_executables
from planner_control import lease_loop, owner_sample
from planner_sinks import Sinks

URL = "http://192.168.2.1:18080"
PATH = "/api/system/control"
HOST = "192.168.2.10"
VIEW = struct.Struct("<8s7Q")
SWEEP = struct.Struct("<8s13Q")
HEADER = struct.Struct("<4sII")
CLIP = 2 * 1024 * 1024


def row(lo, hi):
    return dict(lo_hz=lo, hi_hz=hi, enabled=True)


class Receiver(Sinks):
    def __init__(self, folder):
        self.acks = 0
        self.video_stats = {}
        self.field_stats = dict(begin=0, data=0, end=0, bytes=0, qualified=0, error=0)
        self.protocol_errors = []
        self.views = {}
        self.malformed_udp = 0
        super().__init__(HOST, folder)
        with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as dummy:
            self.udp = dummy.dup()
        self.udp.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 4 * 1024 * 1024)
        self.udp.bind((HOST, 18010))
        self.udp.settimeout(.2)
        self.udp_thread = threading.Thread(target=self.receive_view, daemon=True)
        self.udp_thread.start()

    def route(self, conn):
        with conn.makefile("rb") as stream:
            e = json.loads(stream.readline(1024))
        version = e["ack_version"]
        ident = " ".join(str(e[k]) for k in ("generation", "version", "epoch", "position", "marker_id", "route"))
        if version == 1:
            reply = f'ACK {e["marker_id"]}'
        elif version == 2:
            reply = f"ACK2 {ident}"
        elif version == 3:
            reply = 'ACK3 ' + " ".join(str(e[k]) for k in (
                "boot_id", "generation", "version", "route", "required_path"))
        elif version == 4:
            reply = f'ACK4 {e["boot_id"]} {ident} {e["required_path"]}'
        else:
            raise ValueError("unknown route protocol")
        if not self.missing:
            conn.sendall(reply.encode("ascii") + b"\n")
            self.acks += 1

    def receive_view(self):
        buf = bytearray(65535)
        while not self.stop.is_set():
            try:
                size = self.udp.recv_into(buf)
            except socket.timeout:
                continue
            except OSError:
                break
            now = time.perf_counter_ns()
            magic = buf[:8]
            if magic == b"GSWEEP1\x00" and size == SWEEP.size:
                _, gen, _, _, _, phase, _, _, _, _, _, expected, observed, flags = SWEEP.unpack_from(buf)
                stats = self.view_stats(gen)
                stats["sweep_wire_bytes"] += size
                stats["expected"] += expected
                stats["observed"] += observed
                stats["sender_drops"] += flags >> 1
                stats["A" if phase == 0 else "B"] += int(bool(flags & 1))
                stats["partial_notes"] += int(not (flags & 1) or expected != observed)
            elif magic == b"GVIEW4\x00\x00" and size in (576, 1088, 2112, 1152, 2176, 4224):
                record = {1152:576, 2176:1088, 4224:2112}.get(size, size)
                for at in range(0, size, record):
                    header = VIEW.unpack_from(buf, at)
                    if header[0] != b"GVIEW4\x00\x00":
                        self.malformed_udp += 1
                        continue
                    _, ordinal, _, _, tick, _, _, gen = header
                    stats = self.view_stats(gen)
                    previous = stats["last_ordinal"]
                    if previous is not None:
                        stats["gaps"] += max(0, ordinal - previous - 1)
                        stats["reordered"] += int(ordinal <= previous)
                    else:
                        stats["first_ordinal"] = ordinal
                        stats["first_host_ns"] = now
                        stats["first_tick"] = tick
                    stats["last_ordinal"] = max(ordinal, previous or 0)
                    stats["last_host_ns"] = now
                    stats["last_tick"] = tick
                    stats["received"] += 1
                    stats["view_wire_bytes"] += record
            else:
                self.malformed_udp += 1

    def view_stats(self, generation):
        return self.views.setdefault(generation, dict(received=0, gaps=0, reordered=0, A=0, B=0,
            expected=0, observed=0, sender_drops=0, partial_notes=0, view_wire_bytes=0, sweep_wire_bytes=0,
            first_ordinal=None, last_ordinal=None, first_host_ns=None, last_host_ns=None, first_tick=None, last_tick=None))

    def real8(self, conn):
        from sdr.source.field1_stream import Field1Stream
        field = Field1Stream()
        clips = {}
        with conn.makefile("rb") as stream:
            try:
                while not self.stop.is_set():
                    hdr = stream.read(HEADER.size)
                    if not hdr:
                        return
                    magic, m, n = HEADER.unpack(hdr)
                    if magic not in (b"R8V1", b"R8P2") or not 0 < m < 16384 or not 0 <= n <= 4*1024*1024:
                        raise ValueError("invalid real8 record")
                    meta = json.loads(stream.read(m))
                    raw = stream.read(n)
                    now = time.perf_counter_ns()
                    if len(raw) != n:
                        raise ValueError("short real8 packet")
                    if magic == b"R8P2":
                        kind = meta["kind"]
                        self.field_stats[kind] += 1
                        self.field_stats["bytes"] += n
                        result = field.accept(meta, raw)
                        if kind == "end":
                            self.field_stats["qualified"] += int(meta["qualified_field"])
                            self.field_stats["error"] += int(bool(meta["error"]))
                            self.field_stats["last_end"] = meta
                        if result and not (self.folder/"field.real8").exists():
                            info, payload = result
                            (self.folder/"field.real8").write_bytes(payload)
                            (self.folder/"field.json").write_text(json.dumps(info))
                        continue
                    gen = meta["generation"]
                    v = self.video_stats.setdefault(gen, dict(first_ns=now, last_ns=now, packets=0,
                        bytes=0, wire_bytes=0, pos=meta["pos"], gap_bytes=0, sequence_gaps=0,
                        seq=meta["seq"], reset_count=0, intervals=[]))
                    if v["packets"]:
                        v["intervals"].append(now-v["last_ns"])
                    v["gap_bytes"] += abs(v["pos"]-meta["pos"])
                    v["sequence_gaps"] += int(v["seq"]!=meta["seq"])
                    v["reset_count"] += int(meta["reset"])
                    v["last_ns"] = now
                    v["pos"] = meta["pos"] + n
                    v["seq"] = (meta["seq"] + 1) & 0xffff_ffff
                    v["packets"] += 1
                    v["bytes"] += n
                    v["wire_bytes"] += n + m + HEADER.size
                    if gen not in clips and not (self.folder/f"real8-{gen}.bin").exists():
                        clips[gen] = bytearray()
                        (self.folder/f"real8-{gen}.json").write_text(json.dumps(meta))
                    if gen in clips:
                        clips[gen].extend(raw)
                        if len(clips[gen]) >= CLIP:
                            (self.folder/f"real8-{gen}.bin").write_bytes(clips.pop(gen))
            except (ValueError, KeyError, struct.error) as exc:
                self.protocol_errors.append(str(exc))
            finally:
                for gen, clip in clips.items():
                    if clip:
                        (self.folder/f"real8-{gen}.bin").write_bytes(clip)

    def close(self):
        super().close()
        self.udp.close()
        self.udp_thread.join(1)

    def summary(self):
        snapshots = {}
        for generation, source in self.video_stats.items():
            row = {k: v for k, v in source.items() if k != "intervals"}
            d = sorted(source["intervals"])
            if d:
                row["interarrival_ms"] = {k:d[round((len(d)-1)*p)]/1e6 for k,p in (
                    ("p50", .5), ("p95", .95), ("p99", .99), ("max", 1))}
            snapshots[generation] = row
        return dict(video=snapshots, views=self.views, field=self.field_stats,
                    malformed_udp=self.malformed_udp, tcp_errors=self.protocol_errors, ack_stub=self.acks)


class Probe:
    def __init__(self, board, receiver, folder):
        self.board = board
        self.sink = receiver
        self.folder = folder
        self.cases = []

    def request(self, cmd=None):
        return self.board.request(URL, "POST" if cmd else "GET", PATH, cmd, timeout=3) if cmd else (
            self.board.request(URL, "GET", PATH, timeout=3))

    def until(self, condition, seconds):
        limit = time.monotonic() + seconds
        while time.monotonic() < limit:
            try:
                status = self.request()
                if condition(status):
                    return status
            except OSError:
                pass
            time.sleep(.07)
        raise TimeoutError("no owner readback before deadline")

    def command(self, label, cmd, seconds=15):
        ident = uuid.uuid4().hex
        start = time.perf_counter_ns()
        try:
            result = self.request(dict(cmd, id=ident))
        except HTTPError as error:
            row = dict(label=label, command=cmd, id=ident, http=error.code,
                       error=json.load(error), elapsed_ns=time.perf_counter_ns()-start, status=self.request())
        else:
            assert result["accepted_id"] == ident
            status = self.until(lambda s: ident in (s["applied_id"], s["failed_id"]), seconds)
            row = dict(label=label, command=cmd, id=ident, http=202, status=status,
                       elapsed_ns=time.perf_counter_ns()-start)
        self.cases.append(row)
        self.save()
        print(label, row["http"], row["status"]["phase"], row["status"].get("error"), flush=True)
        return row["status"]["actual"]

    def save(self):
        self.board.write_json(self.folder/"egress.json", dict(commands=self.cases, **self.sink.summary()))

    def counters(self):
        return dict(owner=owner_sample(self.board),
                    network=self.board.ssh("cat /proc/net/snmp /proc/net/dev", 5))


def main():
    cli = argparse.ArgumentParser(description=__doc__)
    cli.add_argument("output", type=Path)
    cli.add_argument("--agent", type=Path, required=True)
    cli.add_argument("--video-hz", type=int, default=5865000000)
    cli.add_argument("--seconds", type=int, default=30, choices=range(5, 121))
    cli.add_argument("--video-only", action="store_true")
    args = cli.parse_args()
    board = bench.accepted
    reference = Path("C:/proj/pluto_ag/evidence")
    modules = reference/"ARM-SCAN-CONTROL-003/build-ee339de/modules"
    bit = reference/"ARM-SCAN-027-ACCEPT-001/tpc2/ram/system_top.bit.bin"
    os.environ.update(PLANNER_BENCH_XTG1=str(bit), PLANNER_BENCH_MODULES=str(modules))

    def run(folder, profiles, boot):
        remote = board.REMOTE
        assert not board.ssh("pidof maia-delivery || true").strip()
        staged = board.ssh(f"cd {remote} && sha256sum maia-delivery obs_ad9361.ko obs_dma.ko row99.bit.bin && devmem 0x7c46033c")
        assert staged.splitlines()[-1] == "0x32435054"
        (folder/"ram-loaded.txt").write_text(staged)
        field = board.member(args.agent.resolve(), "obs-field1")
        board.upload(field, remote, field.name)
        board.ssh(f"chmod 700 {remote}/{field.name}")
        scan = dict(op="scan", planner_version=2, path_ack_version=1, sweep_geometry="27/9", xtg1_wait_ticks=256,
                    catalog_on_boot=not args.video_only, k=2, run=True,
                    ranges=[row(args.video_hz-1_500_000, args.video_hz+1_500_000)])
        analysis = dict(op="analysis", center_hz=args.video_hz, width_hz=24_000_000, k=700,
                        gain_db=35, agc="manual")
        video = dict(op="video", center_hz=args.video_hz, width_hz=8_000_000)
        stop = dict(scan, run=False)
        config = dict(boot_id=boot, output=remote+"/control", view=HOST+":18010",
                    video=HOST+":18009", route=HOST+":18008", scan=stop, analysis=analysis,
                    field_preview=dict(collector=remote+"/obs-field1", target_hz=args.video_hz, timeout_ms=90))
        cfg = folder/"egress-config.json"
        board.write_json(cfg, config)
        board.upload(cfg, remote, cfg.name)
        receiver = Receiver(folder)
        lease_stop, lease_ready, silence = threading.Event(), threading.Event(), threading.Event()
        lease = threading.Thread(target=lease_loop, args=(lease_stop, lease_ready, folder, silence), daemon=True)
        lease.start()
        probe = Probe(board, receiver, folder)
        try:
            assert lease_ready.wait(8), "board lease not ready"
            board.ssh(f"cd {remote} && (timeout 240 ./maia-delivery --system-control {cfg.name} --listen 192.168.2.1:18080 </dev/null >owner.log 2>&1 & echo $! > owner.pid)")
            startup = probe.until(lambda s: s["phase"] == "stopped", 95)
            board.write_json(folder/"initial-arm.json", startup)
            board.write_json(folder/"counters-initial.json", probe.counters())
            probe.command("analysis", analysis)
            probe.command("continuous real8", video)
            time.sleep(12 if args.video_only else args.seconds)
            board.write_json(folder/"counters-video.json", probe.counters())
            probe.command("RETURN", dict(op="return"))
            probe.command("finite FIELD", scan)
            time.sleep(8)
            probe.command("STOP after FIELD", stop)
            if not args.video_only:
                for k in (1, 1023):
                    probe.command(f"narrow K{k}", dict(scan, k=k))
                    time.sleep(2)
                mixed = dict(scan, ranges=[row(5_996_000_000,6_004_000_000),row(2_428_480_000,2_428_560_000)])
                probe.command("mixed seam narrow DC", mixed)
                time.sleep(3)
                probe.command("wide stream", dict(scan, ranges=[row(70_000_000,12_000_000_000)]))
                before = dict(status=probe.request(), sink=receiver.summary(), system=probe.counters())
                board.write_json(folder/"wide-before.json", before)
                time.sleep(args.seconds)
                after = dict(status=probe.request(), sink=receiver.summary(), system=probe.counters())
                board.write_json(folder/"wide-after.json", after)
                probe.command("TX conflict during scan", dict(op="tx", on=False, frequency_hz=2_450_000_000, attenuation_mdb=89750))
                probe.command("STOP after wide", stop)
                probe.command("analysis retained", analysis)
                probe.command("manual route direct", dict(op="route", kind="direct", antenna=0))
                probe.command("last STOP", stop)
        finally:
            try:
                board.ssh(f"cd {remote}; if test -f owner.pid; then p=$(cat owner.pid); if test \"$(cat /proc/$p/comm 2>/dev/null || :)\" = timeout; then kill -TERM \"$p\"; fi; fi; sleep 1")
                if (last := board.ssh(f"cd {remote}; cat owner.log; ./obs-fl-ctl status; cat /sys/bus/iio/devices/iio:device0/out_altvoltage1_TX_LO_powerdown", 10)):
                    (folder/"owner-final.txt").write_text(last)
            finally:
                lease_stop.set()
                lease.join(3)
                receiver.close()
                probe.save()

    bench.accepted.stage = stage_executables
    bench.accepted.video_smoke = run
    sys.argv = [sys.argv[0], str(args.output), "--agent", str(args.agent), "--host-ip", HOST, "--reference"]
    bench.main()


if __name__ == "__main__":
    main()
