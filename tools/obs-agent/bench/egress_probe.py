"""RAM-only TPC2 egress comparison with continuously reading benchmark sockets."""
import argparse
import ctypes
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


def host_sample():
    values = dict(clock_ns=time.perf_counter_ns(), cpu_s=time.process_time(), python_threads=threading.active_count())
    if os.name == "nt":
        from ctypes import wintypes
        class Memory(ctypes.Structure):
            _fields_ = [("cb", wintypes.DWORD), ("faults", wintypes.DWORD)] + [
                (field, ctypes.c_size_t) for field in ("peak_working_set", "working_set", "quota_peak_paged",
                "quota_paged", "quota_peak_nonpaged", "quota_nonpaged", "pagefile", "peak_pagefile")]
        memory = Memory()
        memory.cb = ctypes.sizeof(memory)
        query = ctypes.WinDLL("psapi").GetProcessMemoryInfo
        query.argtypes = (wintypes.HANDLE, ctypes.POINTER(Memory), wintypes.DWORD)
        if query(wintypes.HANDLE(-1), ctypes.byref(memory), memory.cb):
            values.update(rss_kib=memory.working_set // 1024, peak_rss_kib=memory.peak_working_set // 1024)
    return values


def row(lo, hi):
    return dict(lo_hz=lo, hi_hz=hi, enabled=True)


class Receiver(Sinks):
    obn5 = False
    record_views = False

    def sample_view(self, generation, ordinal, flags, k, view):
        pass

    def __init__(self, folder, capture_pal=False, window_s=30):
        self.capture_pal = capture_pal
        self.window_ns = window_s * 1_000_000_000
        self.acks = 0
        self.pause_video = threading.Event()
        self.video_established = threading.Event()
        self.video_stats = {}
        self.field_stats = dict(begin=0, data=0, end=0, bytes=0, qualified=0, error=0, outcomes={})
        self.protocol_errors = []
        self.views = {}
        self.malformed_udp = 0
        super().__init__(HOST, folder)
        self.udp = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
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
                _, gen, version, anchor, pass_id, phase, first, last, _, _, last_tick, expected, observed, flags = SWEEP.unpack_from(buf)
                if not (gen and version and first >= anchor and first <= last and phase == pass_id % 2 and 0 <= observed <= expected):
                    self.malformed_udp += 1
                    continue
                stats = self.view_stats(gen)
                previous_pass = stats["last_pass_id"]
                previous_tick = stats["last_pass_tick"]
                if previous_pass is not None:
                    stats["pass_note_gaps"] += max(0, pass_id - previous_pass - 1)
                    stats["pass_note_reordered"] += int(pass_id <= previous_pass)
                if (previous_pass is not None and pass_id == previous_pass + 1 and previous_tick is not None
                        and flags & 1 and observed and last_tick > previous_tick):
                    timer = stats["pass_ticks"]["AB"[phase]]
                    timer["count"] += 1
                    timer["sum"] += last_tick - previous_tick
                if previous_pass is None or pass_id > previous_pass:
                    stats["last_pass_tick"] = last_tick if flags & 1 and observed else None
                stats["last_pass_id"] = max(pass_id, previous_pass or 0)
                stats["sweep_wire_bytes"] += size
                stats["expected"] += expected
                stats["observed"] += observed
                stats["sender_drops"] += flags >> 1
                stats["A" if phase == 0 else "B"] += int(bool(flags & 1))
                stats["partial_notes"] += int(not (flags & 1) or expected != observed)
            elif magic == b"GVIEW4\x00\x00":
                widths = (128, 256, 512, 1024, 2048, 4096) if self.obn5 else (512, 1024, 2048)
                records = [64+width for width in widths if size in (64+width, 2*(64+width))]
                if len(records) != 1:
                    self.malformed_udp += 1
                    continue
                record, = records
                for at in range(0, size, record):
                    header = VIEW.unpack_from(buf, at)
                    if header[0] != b"GVIEW4\x00\x00":
                        self.malformed_udp += 1
                        continue
                    _, ordinal, _, _, tick, k, flags, gen = header
                    mode = (flags >> 3) & 3
                    if self.obn5:
                        mode |= (flags >> 5) & 4
                        valid = mode in (1,2,3,4) and (record-64)*(8,4,2,1)[mode-1] in (1024,2048,4096)
                    else:
                        valid = record == 64 + (0,512,1024,2048)[mode]
                    if not (gen and 1 <= k <= 1023 and valid):
                        self.malformed_udp += 1
                        continue
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
                    actual_k_counts = stats["actual_k_counts"]
                    actual_k_counts[k] = actual_k_counts.get(k, 0) + 1
                    if self.obn5 and flags & 64:
                        hold_k_counts = stats["hold_k_counts"]
                        hold_k_counts[k] = hold_k_counts.get(k, 0) + 1
                    stats["view_wire_bytes"] += record
                    if self.record_views:
                        self.sample_view(gen, ordinal, flags, k, memoryview(buf)[at+64:at+record])
            else:
                self.malformed_udp += 1

    def view_stats(self, generation):
        return self.views.setdefault(generation, dict(received=0, gaps=0, reordered=0, A=0, B=0,
            expected=0, observed=0, sender_drops=0, partial_notes=0, view_wire_bytes=0, sweep_wire_bytes=0,
            last_pass_id=None, last_pass_tick=None, pass_ticks={p:dict(count=0,sum=0) for p in "AB"},
            pass_note_gaps=0, pass_note_reordered=0, first_ordinal=None, last_ordinal=None, first_host_ns=None, last_host_ns=None, first_tick=None, last_tick=None, actual_k_counts={}, hold_k_counts={}))

    def real8(self, conn):
        from sdr.source.field1_stream import Field1Stream
        field = Field1Stream()
        clips = {}
        with conn.makefile("rb") as stream:
            try:
                while not self.stop.is_set():
                    if self.pause_video.is_set():
                        self.video_established.set()
                        while self.pause_video.is_set() and not self.stop.wait(.02):
                            pass
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
                        self.protocol_errors.append(dict(error="short real8 packet", magic=magic.decode("ascii"),
                            generation=meta.get("generation"), seq=meta.get("seq"), bytes=n, received=len(raw)))
                        return
                    if magic == b"R8P2":
                        kind = meta["kind"]
                        self.field_stats[kind] += 1
                        self.field_stats["bytes"] += n
                        result = field.accept(meta, raw)
                        if kind == "end":
                            self.field_stats["qualified"] += int(meta["qualified_field"])
                            self.field_stats["error"] += int(bool(meta["error"]))
                            outcome = f'{meta["pl_outcome"]}/{meta["error"]}'
                            outcomes = self.field_stats["outcomes"]
                            outcomes[outcome] = outcomes.get(outcome, 0) + 1
                            self.field_stats["last_end"] = meta
                        if self.capture_pal and result and not (self.folder/"field.real8").exists():
                            info, payload = result
                            (self.folder/"field.real8").write_bytes(payload)
                            (self.folder/"field.json").write_text(json.dumps(info))
                        continue
                    gen = meta["generation"]
                    if n == 0 or n % 4 or meta["code"] != 24 or not gen:
                        raise ValueError("invalid R8V1 payload/context")
                    v = self.video_stats.setdefault(gen, dict(first_ns=now, last_ns=now, packets=0,
                        bytes=0, wire_bytes=0, pos=meta["pos"], gap_bytes=0, sequence_gaps=0,
                        seq=meta["seq"], reset_count=0, intervals=[], window_bytes=0, window_packets=0))
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
                    if now-v["first_ns"] < self.window_ns:
                        v["window_bytes"] += n
                        v["window_packets"] += 1
                    if self.capture_pal:
                        if not clips:
                            clips[gen] = meta, bytearray()
                        if gen in clips:
                            _, clip = clips[gen]
                            remain = CLIP-len(clip)
                            if remain:
                                clip.extend(raw[:remain])
            except (ValueError, KeyError, struct.error) as exc:
                self.protocol_errors.append(str(exc))
            finally:
                for gen, (meta, clip) in clips.items():
                    if clip:
                        (self.folder/f"real8-{gen}.bin").write_bytes(clip)
                        (self.folder/f"real8-{gen}.json").write_text(json.dumps(meta))

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
            if source["window_packets"] >= 2 and source["last_ns"]-source["first_ns"] >= self.window_ns:
                window = sorted(source["intervals"][:source["window_packets"]-1])
                row["fixed_window"] = dict(seconds=self.window_ns/1e9, bytes=source["window_bytes"],
                    frames=source["window_packets"], interarrival_ms={k:window[round((len(window)-1)*p)]/1e6
                    for k,p in (("p50", .5), ("p95", .95), ("p99", .99), ("max", 1))})
            snapshots[generation] = row
        return dict(video=snapshots, views=self.views, field=self.field_stats,
                    malformed_udp=self.malformed_udp, tcp_errors=self.protocol_errors, ack_stub=self.acks)


class Probe:
    def __init__(self, board, receiver, folder):
        self.board = board
        self.sink = receiver
        self.folder = folder
        self.cases = []
        self.checks = {}

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
            accepted = time.perf_counter_ns()
            status = self.until(lambda s: ident in (s["applied_id"], s["failed_id"]), seconds)
            row = dict(label=label, command=cmd, id=ident, http=202, status=status,
                       host_post_ns=start, accepted_ns=accepted-start, elapsed_ns=time.perf_counter_ns()-start)
        self.cases.append(row)
        self.save()
        print(label, row["http"], row["status"]["phase"], row["status"].get("error"), flush=True)
        return row["status"]["actual"]

    def save(self):
        self.board.write_json(self.folder/"egress.json", dict(commands=self.cases, checks=self.checks, **self.sink.summary()))

    def counters(self):
        system = self.board.ssh(
            "for name in maia-delivery iio_readdev iiod obs-field1; do "
            "for p in $(pidof $name); do for t in /proc/$p/task/*; do cat $t/stat; done; "
            "grep -E 'VmRSS|Threads' /proc/$p/status; ls /proc/$p/fd | wc -l; done; done; "
            "cat /proc/net/snmp /proc/net/dev", 5)
        return dict(host=host_sample(), owner=owner_sample(self.board), processes_and_network=system)


def main():
    cli = argparse.ArgumentParser(description=__doc__)
    cli.add_argument("output", type=Path)
    cli.add_argument("--agent", type=Path, required=True)
    cli.add_argument("--video-hz", type=int, default=5865000000)
    cli.add_argument("--seconds", type=int, default=30, choices=range(5, 121))
    cli.add_argument("--video-only", action="store_true")
    cli.add_argument("--scan-only", action="store_true")
    cli.add_argument("--field-only", action="store_true")
    cli.add_argument("--pal-check", action="store_true", help="capture continuous real8 before the FIELD watch")
    cli.add_argument("--field-hz", type=int, default=5864800000)
    cli.add_argument("--candidate", action="store_true")
    cli.add_argument("--native-usb", action="store_true")
    cli.add_argument("--phase-timing", action="store_true")
    args = cli.parse_args()
    if sum((args.video_only, args.field_only, args.scan_only)) > 1 or (args.native_usb and (args.field_only or args.scan_only)):
        cli.error("choose one data subset; native USB requires the video comparison")
    if args.pal_check and not args.field_only:
        cli.error("--pal-check requires --field-only")
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
                    catalog_on_boot=not (args.video_only or args.field_only), k=2, run=True,
                    ranges=[row(args.video_hz-1_500_000, args.video_hz+1_500_000)])
        analysis = dict(op="analysis", center_hz=args.video_hz, width_hz=24_000_000, k=700,
                        gain_db=35, agc="manual")
        video = dict(op="video", center_hz=args.video_hz, width_hz=8_000_000)
        stop = dict(scan, run=False)
        config = dict(boot_id=boot, output=remote+"/control", view=HOST+":18010",
                    video=HOST+":18009", route=HOST+":18008", scan=stop, analysis=analysis)
        if args.field_only or not args.candidate:
            config["field_preview"] = dict(collector=remote+"/obs-field1",
                target_hz=args.field_hz if args.field_only else args.video_hz, timeout_ms=90)
        cfg = folder/"egress-config.json"
        board.write_json(cfg, config)
        board.upload(cfg, remote, cfg.name)
        receiver = Receiver(folder, args.pal_check, args.seconds)
        lease_stop, lease_ready, silence = threading.Event(), threading.Event(), threading.Event()
        lease = threading.Thread(target=lease_loop, args=(lease_stop, lease_ready, folder, silence), daemon=True)
        lease.start()
        probe = Probe(board, receiver, folder)
        try:
            assert lease_ready.wait(8), "board lease not ready"
            profile = "OBS_PHASE_TIMING=1 " if args.phase_timing else ""
            board.ssh(f"cd {remote} && ({profile}timeout 240 ./maia-delivery --system-control {cfg.name} --listen 192.168.2.1:18080 </dev/null >owner.log 2>&1 & echo $! > owner.pid)")
            startup = probe.until(lambda s: s["phase"] == "stopped", 95)
            board.write_json(folder/"initial-arm.json", startup)
            board.write_json(folder/"counters-initial.json", probe.counters())
            if args.field_only:
                if args.pal_check:
                    probe.command("analysis for PAL", analysis)
                    probe.command("continuous PAL real8", video)
                    time.sleep(2)
                    returned = probe.command("RETURN from PAL", dict(op="return"))
                    probe.checks["return_before_field"] = returned["mode"] == "analysis" and returned["running"]
                probe.command("finite FIELD", scan)
                time.sleep(args.seconds)
                probe.command("STOP after FIELD", stop)
                return
            analysis_actual = probe.command("analysis", analysis)
            if not args.scan_only:
                board.write_json(folder/"counters-video-before.json", probe.counters())
                running_video = probe.command("continuous real8", video)
                time.sleep(args.seconds)
                probe.checks["real8_fresh"] = receiver.video_stats.get(running_video["generation"], {}).get("packets", 0) > 0
                board.write_json(folder/"counters-video.json", probe.counters())
                returned = probe.command("RETURN", dict(op="return"))
                probe.checks["return_analysis"] = returned["mode"] == "analysis" and returned["running"] and returned["k"] == 700 and returned["rx_lo_hz"] == analysis_actual["rx_lo_hz"]
                if args.candidate:
                    probe.command("video for scan transition", video)
                    from_video = probe.command("video to scan", scan)
                    time.sleep(2)
                    probe.checks["video_to_scan"] = from_video["mode"] == "scan" and from_video["running"] and (
                        receiver.view_stats(from_video["generation"])["received"] > 0)
                    probe.command("STOP after video to scan", stop)
                else:
                    probe.command("finite FIELD", scan)
                    time.sleep(8)
                    probe.command("STOP after FIELD", stop)
            if not args.video_only:
                for k in (1, 1023):
                    probe.command(f"narrow K{k}", dict(scan, k=k))
                    time.sleep(3)
                    board.write_json(folder/f"narrow-K{k}.json", dict(status=probe.request(), sink=receiver.summary()))
                    probe.command(f"STOP K{k}", stop)
                    (folder/f"reader-K{k}.txt").write_text(board.ssh(f"cat {remote}/control/reader/result.json || true"))
                mixed = dict(scan, ranges=[row(5_996_000_000,6_004_000_000),row(2_428_480_000,2_428_560_000)])
                probe.command("mixed seam narrow DC", mixed)
                time.sleep(3)
                wide = probe.command("wide stream", dict(scan, ranges=[row(70_000_000,12_000_000_000)]))
                before = dict(status=probe.request(), sink=receiver.summary(), system=probe.counters())
                board.write_json(folder/"wide-before.json", before)
                time.sleep(args.seconds)
                after = dict(status=probe.request(), sink=receiver.summary(), system=probe.counters())
                board.write_json(folder/"wide-after.json", after)
                blocked = probe.command("TX conflict during scan", dict(op="tx", on=False, frequency_hz=2_450_000_000, attenuation_mdb=89750))
                if args.candidate:
                    probe.checks["tx_conflict_http_409"] = probe.cases[-1]["http"] == 409
                    probe.checks["tx_conflict_execution_unchanged"] = blocked["running"] and all(
                        blocked[key] == wide[key] for key in ("generation", "dma_run", "map_version"))
                    count = receiver.view_stats(wide["generation"])["received"]
                    time.sleep(.4)
                    probe.checks["tx_conflict_fresh_view"] = receiver.view_stats(wide["generation"])["received"] > count
                probe.command("STOP after wide", stop)
                (folder/"reader-wide.txt").write_text(board.ssh(f"cat {remote}/control/reader/result.json || true"))
                probe.command("analysis retained", analysis)
                probe.command("manual route direct", dict(op="route", kind="direct", antenna=0))
                if args.candidate:
                    mixer = probe.command("manual route mixer", dict(op="route", kind="mixer", antenna=1, multiplier=2, side=1))
                    probe.checks["manual_mixer"] = mixer["route"]["kind"] == mixer["analysis_route"]["kind"] == "mixer"
                    probe.checks["manual_mixer_waits_for_lo"] = not mixer["running"] and mixer["waiting"] == "mixer LO off"
                    tx = dict(op="tx", on=True, frequency_hz=2_500_000_000, attenuation_mdb=89750)
                    lo = probe.command("manual mixer LO on", tx)
                    probe.checks["manual_mixer_lo_readback"] = lo["running"] and lo["tx"]["on"] and lo["if_hz"] == args.video_hz-5_000_000_000
                    off = probe.command("manual mixer LO off", dict(tx, on=False))
                    probe.checks["manual_mixer_lo_off"] = not off["running"] and off["tx"]["powerdown"] == 1
                    restored = probe.command("manual route back to direct", dict(op="route", kind="direct", antenna=0))
                    probe.checks["manual_direct"] = restored["route"]["kind"] == restored["analysis_route"]["kind"] == "direct"
                    receiver.missing = True
                    waiting_id = uuid.uuid4().hex
                    wait_cmd = dict(op="route", kind="mixer", antenna=1, multiplier=2, side=1, id=waiting_id)
                    assert probe.request(wait_cmd)["accepted_id"] == waiting_id
                    waited = probe.until(lambda st: bool(st["route_pending"]) or st["failed_id"] == waiting_id, 3)
                    probe.checks["withheld_route_pending"] = bool(waited["route_pending"])
                    board.write_json(folder/"withheld-route.json", waited)
                    probe.command("STOP waiting route", stop)
                    receiver.missing = False
                    probe.command("analysis after cancelled route", analysis)
                    probe.command("STOP analysis", stop)
            if args.candidate and not args.scan_only:
                for i in range(2):
                    probe.command(f"analysis cycle {i}", analysis)
                    probe.command(f"video cycle {i}", video)
                    probe.command(f"STOP video cycle {i}", stop)
                    metrics = probe.cases[-1]["status"]["metrics"]["owner"]
                    probe.checks[f"nominal_stop_video_{i}"] = bool(metrics) and not metrics["error"] and not metrics["cleanup_error"]
                    board.write_json(folder/f"counters-cycle-{i}.json", probe.counters())
                probe.command("analysis before consumer pause", analysis)
                receiver.pause_video.set()
                queued_id = uuid.uuid4().hex
                assert probe.request(dict(video, id=queued_id))["accepted_id"] == queued_id
                assert receiver.video_established.wait(6)
                blocked = probe.until(lambda st: st["failed_id"] == queued_id or (
                    st["applied_id"] == queued_id and (st["metrics"]["owner"] or {}).get("send_would_block", 0)), 5)
                board.write_json(folder/"consumer-paused.json", blocked)
                probe.command("STOP with consumer paused", stop)
                receiver.pause_video.clear()
                probe.command("analysis after pause", analysis)
                probe.command("video after consumer resume", video)
                time.sleep(2)
            probe.command("last STOP", stop)
            if args.candidate and not args.scan_only:
                metrics = probe.cases[-1]["status"]["metrics"]["owner"]
                probe.checks["fresh_after_paused_consumer"] = bool(metrics) and not metrics["error"] and not metrics["cleanup_error"] and metrics["video_bytes"] > 0
            board.write_json(folder/"counters-final.json", probe.counters())
            if args.native_usb:
                from egress_usb import measure
                board.ssh(f"cd {remote}; kill -TERM \"$(cat owner.pid)\"; i=0; while pidof maia-delivery >/dev/null && test \"$i\" -lt 40; do i=$((i+1)); usleep 50000; done; ! pidof maia-delivery")
                try:
                    measure(board, folder, args.seconds, analysis_actual["rx_lo_hz"], host_sample)
                except (OSError, RuntimeError) as exc:
                    (folder/"native-error.txt").write_text(str(exc), encoding="utf-8")
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
