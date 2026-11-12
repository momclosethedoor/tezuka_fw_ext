# Observation v1 — executed spectra and offline replay

Status: OBS-002 implementation; **not connected to live DMA, `/waterfall`, or host product code**.
All public reference vectors are synthetic, including the legacy-format example.

## Existing input, established from source

- Maia `maia-httpd/src/spectrometer.rs:65-117` reads the *current* input rate,
  detector and nint, then iterates completed DMA buffers. It decodes 47 mantissa
  bits, exponent `(word >> 56) & 3`, and three tag bits `word >> 61`. The first
  output float is the tag, replacing the missing power at bin zero. The scale is
  `4e6/(nint*Fs)` for Average and `4e6/Fs` for PeakDetect. Output uses native-endian
  float32 (little-endian on this ARM target), 4096 elements / 16384 bytes.
- `src/fpga.rs:470` and `src/rxbuffer.rs` expose DMA ring buffers, not a persistent
  sequence, RF epoch or first-sample timestamp. A ring index / three-bit tag is
  not an epoch or absolute sequence. It cannot expose losses of whole ring laps.
- The selected Maia source `maia-hdl/maia_hdl/spectrum_integrator.py:241` implements
  FFT reordering/fftshift; `spectrometer.py:113` selects symmetric Blackman-Harris.
  This source inspection does not assert that the installed PL-5b XSA is rebuilt
  from this particular HDL checkout. No new hardware metadata is attributed to PL-5b.
- Host `sdr/source/maia/ws.py` reads `<f4`; `source.py:206-273` models fftshift,
  `f[k] = center - Fs/2 + k*Fs/4096`, replaces tag bin zero by bin one for display,
  and applies `dBFS=10log10(P)+K(Fs)`. `sdr/dsp/levels.py:123-145` defines
  `K(Fs)=-183.529+10log10(Fs)` and 0 dBFS as a complex full-scale tone (2048 ADC units).
- `sdr/dsp/rows.py:350-418` reduces frames in **linear** power (mean and max) and
  then converts to dB. `sdr/cls/observation.py` is an existing in-memory host object,
  not this wire format: dBFS, valid mask, fftshift grid and host-derived time/epoch.
  Max of K already-averaged legacy frames is not the true peak of their N*K FFTs.
  Host receive timestamps/current RF configuration cannot authenticate old spectra.

## Scope and identity

One capture is one producer sequence/counter lifetime. Its 128-bit nonzero capture
ID survives copying and replay. A restart, counter wrap/reset or new independent
acquisition needs a new ID. Identity is **(capture ID, seq)**, never delivery time.
`seq` starts at zero and increments when observations are produced, before loss or
suppression in delivery. Delayed/out-of-order records retain it. Identical repeats
are allowed and are one observation; conflicting bytes for the same ID are rejected.

Descriptors are immutable capture-local IDs 1..65535, defined before use; redefining
an ID is rejected. They hold stable frequency grid/mode/provenance. Multiple epochs
may reuse a descriptor (e.g. returning to the same grid), but a nonzero RF epoch
must never refer to different descriptors. Allocate a fresh epoch for each RF visit
or RF-state/configuration generation; never reuse an old visit ID for new data.

There is no scheduling request, dwell command, next-hop acknowledgement or queue
in this format. ARM prepares active/next maps; PL executes a boundary switch.
Metadata describes what was executed. The packet is not permission to continue.

## Binary layout

All fields explicitly **little endian**, packed, no native structs/padding.
A capture starts with 24 bytes: magic `47 4f 42 53 31 0d 0a 00` (`GOBS1\r\n\0`),
then the 16 capture-ID bytes (UUID byte order). Version is only in that magic.

Every record starts with an 8-byte prefix:

| Offset | Type | Meaning |
|---:|---|---|
| 0 | u32 | Total record bytes, including prefix |
| 4 | u16 | Kind: 1 descriptor, 2 observation |
| 6 | u16 | Descriptor ID / descriptor reference |

Descriptor record is **48 bytes**, including prefix:

| Offset | Type | Meaning |
|---:|---|---|
| 8 | f64 | FFT input samples/second, positive finite |
| 16 | f64 | RF frequency of bin zero; canonical NaN only for legacy unknown grid |
| 24 | f64 | Window ENBW in bins, positive finite and <= N |
| 32 | u64 | Source clock Hz; 0 means unavailable (MAX is forbidden) |
| 40 | u32 | N = FFT length = bins per plane; power of 2, 2..65536 |
| 44 | u16 | Reduction: 1 mean, 2 peak, 3 mean followed by peak |
| 46 | u16 | Provenance: 1 synthetic, 2 imported legacy Maia WS, 3 hardware-stamped |

Observation has **40 bytes of header**, including prefix, then float32 planes:

| Offset | Type | Meaning |
|---:|---|---|
| 8 | u64 | Producer seq, scoped by capture ID |
| 16 | u64 | RF epoch; 0 = unattributed/uncertain, not clean new-epoch data |
| 24 | u64 | First contributing sample source-clock tick; MAX if unavailable |
| 32 | u32 | Actual contributing consecutive nonoverlapping FFT count nint; 0 only for unknown legacy count |
| 36 | u32 | Legacy three-bit tag 0..7; MAX outside legacy provenance |
| 40 | N or 2N f32 | Linear full-scale-squared power per bin, fftshift order |

Record length is exactly `40+4*N` (mean/peak) or `40+8*N` (mean+peak).
At N=4096 overhead is 40/16384=0.2441% for one plane, 0.1221% for two planes;
there is one 24-byte capture preamble and one 48-byte record per distinct descriptor.
No per-record version, flags, repeated frequency grid or delivery timestamp.

Float32 power is finite >=0, or canonical quiet NaN `0x7fc00000` for an invalid bin;
zero is measured zero, not missing. RF-grid NaN is `0x7ff8000000000000`. Other NaN
representations/infinities are rejected on wire. Payload has no legacy tag bin:
legacy bin zero becomes invalid NaN, not invented neighbour power. For finite
bins, dBFS is `10log10(power)`; sum of bin powers divided by ENBW gives coherent
signal-power convention. A binwise peak envelope is not simultaneous total RF
power when its bins peak at different times. This is not RF dBm calibration.

Mean/peak use the **same** contributing FFT windows: arithmetic mean of linear
bin powers and per-bin maximum, respectively. Mode 3 is one observation with two
planes, not two samples of the RF event. nint=1 is one FFT; its paired planes are
identical. Mean payload is already divided by nint. A consumer must not divide it
again, nor apply the old Maia K(Fs) to this normalized payload.

N*nint is contributing sample volume. Only attributed records can claim clean
volume in one RF epoch. `N*nint/Fs` is active FFT sample duration, **not settling,
dwell, delivery latency or measured scan throughput**. Settling/dwell remain
separate scheduling/measurement concepts. Contributions within a record are
consecutive; do not hide RF gaps or invalid FFTs inside nint. Counter Hz is the
producer-declared clock domain, with tick latched at the first sample. When Hz=0,
tick must be MAX; when Hz>0 it must be available. It is not Unix/host receive time.
Synthetic counters/epochs are explicitly fictional. A tick does not itself prove
settling or purity of any FFT.

A nonzero RF epoch asserts every contributing FFT window belongs to that epoch.
A window crossing a change must be suppressed (seq loss remains visible) or
reported with epoch=0; it must never be relabelled as clean new-epoch data.
Consumers must exclude unattributed records from RF-attributed processing. The
codec checks structural/context invariants; it cannot physically verify this
producer assertion. OBS-003 must supply real metadata atomically with the data.

Legacy provenance requires epoch=0, clock Hz=0/tick=MAX, unknown RF bin0 and tag
0..7. nint=0 means unknown, not nint=1. The explicit import context provides the recorded mean/peak mode and Fs/ENBW for
unit conversion; it does not turn configuration readback into per-FFT hardware
metadata. Original raw tag is diagnostic only, never an RF epoch/map generation.

Readers reject unknown kinds, missing/redefined descriptors, inconsistent sizes,
invalid scalars, conflicting seq IDs and epoch/grid relabelling. Maximum record is
524328 bytes. EOF inside a field/record is an error; EOF exactly at a complete
record boundary is structurally valid (including descriptor-only captures).
Detect boundary-aligned file truncation using a storage hash/length, not this codec.

## Implementations, vectors and commands

Rust: `maia-httpd/src/observation.rs`, exposed by `lib.rs` only. No live caller.
Python: `tools/observation/observation.py` (stdlib reader/writer), `obs.py` CLI.
The reference Capture APIs materialize a finite capture in memory. OBS-003 must
add the bounded incremental live producer/transport adapter; do not collect an
unbounded live stream into Capture. The wire layout need not change.
Shared exact byte vectors are hex-encoded text in
`maia-httpd/tests/observation-v1/{golden,legacy}.hex`, so Buildroot GNU patch can
transfer them without unsupported Git binary patches. `expected.json` gives
independent semantic expectations and underlying synthetic FFT powers.
**Both vectors are synthetic tests**; legacy.hex illustrates legacy semantics.
CI emits the same bytes as `.gobs`, Rust re-encoding, test logs and SHA256SUMS.

```powershell
$py = 'C:/proj/goofy_v2/.venv/Scripts/python.exe'
& $py -m unittest discover -s firmware/tools/observation -p 'test_*.py' -v
& $py firmware/tools/observation/obs.py replay maia-sdr/maia-httpd/tests/observation-v1/golden.hex
& $py firmware/tools/observation/obs.py replay maia-sdr/maia-httpd/tests/observation-v1/golden.hex --plan evidence/obs-002-delay-plan.json --output evidence/obs-002-delayed.gobs
```

A plan is a JSON array such as
`[{"seq":0},{"seq":2,"delay_ms":20},{"seq":1,"delay_ms":30},{"seq":3},{"seq":2},{"seq":5},{"seq":6}]`.
Default is fast replay; `--delay-ms` supplies a default per-presentation wait.
Events expose source provenance, original IDs, counters and payload hash.
`delivery_elapsed_ms` is a replay diagnostic, never copied into observation time.
Summary counts unique IDs, presentations/repeats, missing seq IDs in 0..max delivered
and changes between known epochs **in acquisition seq order**, not arrival order.
Missing seq counts describe delivered IDs; they do not distinguish intentional
omission from source loss. Epoch changes across missing/unattributed records are
observed bounds, not proof of a precise hardware transition boundary.
Output preserves capture/record IDs and payloads, including repeats.

Optional `legacy input.npz output.gobs --sample-rate FS --enbw ENBW --mode mean` uses existing
host NumPy only to read saved `frames`; normal reader/replay/tests are stdlib.
It derives a deterministic import capture ID from source-file SHA256, numbers
saved frames by ordinal (not hardware seq), marks epoch/tick/nint unknown, retains
tags, and converts `old_power*10^((-183.529+10log10(Fs))/10)`. A JSON sidecar records
context/provenance and omitted receive timestamps. The input bytes and output
stay private under evidence/. Re-import of identical input bytes preserves IDs;
physical identity across independently repackaged legacy files is unknowable.

## OBS-003 integration boundary

- Replace the absence of metadata at FPGA FFT/integration/DMA production, not at
  websocket delivery: latch RF epoch/map generation at input window boundaries,
  track first-sample clock and producer seq/actual nint, reject crossing windows,
  and carry the snapshot through integration and DMA with matching buffer data.
- Active/next map switch must name an exact boundary; slot/profile ID without
  generation is insufficient when slots/maps are reused. No per-hop ARM ACK.
- `fpga.rs::get_spectrometer_buffers` / rxbuffer layout currently return only
  sample bytes. Extend the agreed PL/driver/ARM packet as one matched set later.
- `Spectrometer::run` currently reads live settings before iterating old buffers.
  Do not use that readback to stamp old data. Encoding belongs after validation of
  carried buffer metadata and normalization, before broadcast. This slice does
  not fix the old path by guessing its provenance.
- Keep the existing `/waterfall` sender/route unchanged; select an agreed separate
  transport in OBS-003. Python reference decode must precede host level conversion
  / row aggregation. Preserve NaN validity and already-reduced mean/peak semantics;
  adapt `MaiaSource.try_spectrum`, row input and observation epoch handling together.
- Missing today: authoritative persistent seq, RF visit/map generation, boundary
  purity, first-sample counter/domain, completed accumulation count tied to buffer,
  and simultaneous mean+true-peak product. PL-5b cannot be claimed to emit them.
- Tracker IndexError 66/64 remains open. The saved OBS-001 trace has two indexed
  assignments on one source line; no failing candidates/k/ls/array snapshots were
  retained. The 16 saved direct frames precede the failing host/video segment.
  They are not a reproducer. No cause, clamp, drop or classifier fix is invented.

## OBS-003 bounded live profile (independent of native PL ABI)

The GOBS1 wire bytes above are unchanged. `observation_live.rs` provides a
per-record `Encoder`, borrowed `Reader`, and bounded `Fanout`; the old `Capture`
API and its history maps are offline only. `Encoder::new_nint1` installs at most
16 immutable descriptors once, requires one plane, an explicit source clock and
synthetic or hardware provenance, and rejects nint other than 1. It never decodes
its encoded capture. No payload normalization is performed by this codec.

Live order is strictly increasing **producer seq** within one capture. Duplicates,
reordering and counter rollback fail closed; offline replay remains available for
delayed/reordered records. Joining a running source establishes a baseline at the
first delivered seq: missing leading observations are unknown, not invented loss.
An epoch is opaque, not a numerical clock. Epoch zero is diagnostic/unattributed
and does not erase the last known binding. The last known epoch/descriptor binding
is checked; lifetime epoch non-reuse and immutable context binding remain producer
obligations. The bounded reader cannot audit every historical epoch without an
unbounded map. All live descriptors precede the first observation; new grids beyond
the installed table require a new capture in this first profile. Source restart or
hardware-counter reset requires a fresh capture UUID and new stream, not seq reset
in an existing connection. The controller supplies a fresh UUID for each source
lifetime; client reconnect alone does not create another observation/source.

Fanout uses Q preallocated whole-record slots (default 8, maximum payload slot
budget 16 MiB). Publishing uses `try_lock`: a copying reader can cause a whole
incoming observation to be dropped and counted, but cannot block acquisition.
Readers copy one complete slot before socket I/O. There are at most C senders
(default 2, limit 8), each with one reusable pending record and a scalar cursor.
While a socket is blocked, its pending record is finished before the cursor skips
over overwritten slots. No header/payload splicing or partial-record drop occurs.
A failed connection is closed, never resumed in the middle of a record. Forced
drain timeout is reported as an error and partial-disconnect counter; a truncated
receiver record is an error. Ring ordinals are internal and never replace source
seq. `contention_drops` and per-client `missed` explain distinct loss causes.
Slot reuse is not itself a loss for clients which already consumed that slot.

Userspace payload storage is bounded by Q*R + C*max(R,header) + encoder frame R +
source power 4*N, plus the descriptor/header tables and fixed threads. Python
`tools/observation/live.py` uses one 524328-byte input buffer, at most 16 descriptors
and scalar counters, returning a borrowed payload valid until the next read.
Rust incremental Reader has the same fixed input buffer bound. Kernel TCP buffers,
allocator/stack/runtime and process RSS are additional and measured separately.
Nothing retains all source seq, payloads or epochs. Reader CPU is deliberately
instrumental, without a claim of maximum-Fs FFT throughput or UI frame rate.

`gobs-synthetic` is a separate, explicitly opted-in transport exerciser built from
the same source stack for native Linux/Windows and ARM. It is not installed in
rootfs, auto-started, or used as a fallback by maia-httpd. It opens no DMA/IIO/RF
devices. Its source=1 descriptors, invented ticks/epochs and deterministic powers
are synthetic throughout. Context alternation and epoch=0 diagnostic tests do not
prove RF settling. TCP here is an instrumental transport, not a new native DMA
ABI or a host product migration. Existing `/waterfall` and real8 are untouched
pending acceptance of their agreed replacement.

```powershell
$py='C:\proj\goofy_v2\.venv\Scripts\python.exe'
# Portable exe comes from the exact CI run, not a local Rust installation:
$run=43129725401 # accepted independent streaming build, attempt 1
$exe="artifacts/$run-attempt-1/observation-live-windows/gobs-synthetic.exe"
$id=[guid]::NewGuid().ToString('N')
& $exe --synthetic --capture-id $id --bind 127.0.0.1:9009 --seconds 30 --rate 2000 --wait-clients 1
# In another terminal; all data remain explicitly synthetic:
& $py firmware/tools/observation/live.py --connect 127.0.0.1:9009 --expect-source synthetic --verify-synthetic --seconds 55
# Automated fast/slow clients, RSS/CPU and payload/label oracle, <=5 minutes:
& $py firmware/tools/observation/check_live.py $exe evidence/obs-003-another-local-live --seconds 30
```

Native integration is deliberately pending the i5 OBS-003-PL document and shared
native golden records, then successful P&R + compatible bit/bin. PL base is 1R1T A
+ PL-PREP ccbd2a58a271469e83d3ab84aa656376334195c1; current board remains PL-5b.
The adapter must decode that **single** ABI at the existing
`fpga.rs::get_spectrometer_buffers` / rxbuffer and `Spectrometer::run` boundary,
validate carried seq/context/first-sample tick with the matching payload, map
clean=0 to epoch=0, select an installed descriptor by carried context and call
`Encoder::observation` then `Fanout::publish`. It must not read current RF settings
to label pending buffers, duplicate nint=1 planes, reapply old K(Fs) to normalized
payloads, or introduce a second DMA owner. No native layouts/registers are guessed.
Before hardware acceptance, verify native vectors, payload units/bin order,
counter domain/reset semantics and the agreed temporary PL/ARM reload/rollback
procedure. No hardware test or real8 regression claim follows from synthetic TCP.

Independent streaming acceptance: run 43129725401, attempt 1, source head
279e1dffa63b867de8c551459cb963e65937f608 passed Linux/Windows native tests and
Nano ARM-only build. Each OS passed 11 Rust tests (including 1,000,000 observations)
and 18 Python tests. Local Pentium TCP test produced 60,000 synthetic observations
in 30 seconds, preserved each received payload/label, and reconciled all queue and
contention drops. Source warmed RSS was 3,637,248 bytes; readers approximately
15.3 MB, stable within 16 KiB. Python oracle CPU limits throughput; even the client
without artificial delay had visible losses. No maximum-Fs or hardware claim.
Artifacts: `observation-contract`, `observation-live-windows`, `maia-httpd-nano`.
Local full report: C:/proj/pluto_ag/evidence/OBS-003-ARM.md. Later reader-PID tooling
and documentation commits preserve the exact compiled source fingerprint.

## OBN1-r1 native adapter (software stage; hardware acceptance deferred)

The i5 handoff is now available: [authoritative ABI](OBN1-ABI-r1.md), PL commit
`370d03a02fface2a287d8d7cafb6cee7422217ee`, parent PL-PREP `ccbd2a58`.
Its 130 SHA256SUMS entries and bit-in-XSA equality were verified. Integrated P&R/
bitgen passed: WNS +0.057 ns, WHS +0.021 ns, TNS/THS 0, with the inherited timing/
CDC limitations documented by i5. These reports are not hardware acceptance.
Expected experimental bit SHA256 is
`311b49bc1d82646ad2f73d33ff475b304318b0e4b0d9049b83f31b087a6b19f9`,
bin `089e9227e72593c546821ca79711caf46246fecff074f741925b7c783ed6ad12`.
They are kept in the local handoff, not substituted into the firmware's PL-5b XSA.

`native_observation.rs` validates every header constant/reserved bit/exponent and
all 4096 bins. GOBS index k reads native index reverse12((k+2048)%4096), then
computes float32(M*2^(2E)/750668^2), ENBW 2.0050129965307675. There is no legacy
K(Fs), 4e6/Fs, extra nint division, sacrificed bin zero or duplicate peak plane.
The first live profile accepts static **raw** input only; DDC flags fail closed
until its immutable gain/grid provenance is implemented. Dirty records retain
their measured powers and epoch=0. A finite immutable epoch-to-descriptor table
selects by carried metadata, never by the currently staged token or IIO readback.
One optional diagnostic context transition uses the same RF grid; changing that
token does not measure RF settling.

`maia-httpd --native-observations CONFIG.json` opts into the experimental producer.
Without that flag the legacy application path remains selected. In native mode:

- `IpCore::take_native` checks OBN1 and the 8 x 32768 mapping, skips legacy DDC/
  integration initialization, and refuses DDC input. It never asserts SDR reset.
- The old Spectrometer task is not started; `/waterfall` returns 410. Existing GET
  APIs remain available, while mutating HTTP requests return 423 for this static
  source lifetime. Configure RF outside this mode before starting it. This is a
  restricted acceptance mode, not a new product configuration/control protocol.
- Each command arms 1..8 windows. Completion requires idle/no fault/reject and
  consistent selected/recorded/lost deltas, not IRQ count or pointer differences.
  Full 8-slot wrap and modulo32 counters are handled. All completed slots are
  invalidated through the existing rxbuffer ioctl and copied before rearm. The
  whole series is validated before any of its records is published. Corruption,
  context mismatch, counter rollback, fault or timeout aborts the source; there
  is no automatic reset/recovery or decoder fallback.
- Reusable storage includes one 8*32768-byte native copy batch plus the existing
  bounded output ring/encoder/power/pending buffers. CPU conversion and time
  between series can leave **unselected** FFTs; native selected-window loss,
  output contention loss and per-client ring skips are separate counters.
- The source requires an explicitly coordinated full system boot ID, matching
  static IIO Fs/LO and an external settling assertion. A per-boot create_new marker
  refuses blind restart; a nonblocking owner lock and another-maia process check
  supplement the required existing service/lease ownership procedure. Neither
  boot ID nor ABI ID attests the loaded bit hash or RF purity: delivery/boot proof
  must be checked independently. Fresh capture UUID after producer reset is an
  operator/controller obligation, not a value derived from receive time.
- Source runs are bounded to 240 seconds plus completion/drain. On stop it clears
  validity, never resets the AXI master. The per-boot marker is intentionally kept.
  A coordinated system reset/RAM boot is required for another source lifetime;
  restarting the stock ELF alone is not a valid PL/ARM rollback.

Example schema (IDs, Fs/LO and settling must come from the FUTURE verified setup;
the artifact contains an intentionally unusable dummy boot ID for offline checks):

```json
{
  "capture_id": "11111111-1111-4111-8111-111111111111",
  "coordinated_boot_id": "22222222-2222-4222-8222-222222222222",
  "externally_settled": true,
  "sample_rate": 61440000,
  "rx_lo_hz": 5190000000,
  "epoch": 7,
  "series_windows": 8,
  "run_seconds": 30,
  "bind": "0.0.0.0:9009",
  "diagnostic_after_series": 20,
  "diagnostic_next_epoch": 8
}
```

`maia-httpd --check-native-observations CONFIG.json` validates syntax/profile and
exits **before** FPGA/IIO/ownership access. It does not verify the boot/RF assertion
and emits no observations. CI executes this mode on the actual ARM ELF with
qemu-arm and the target sysroot, including an invalid-settling negative case.

Seven shared native fixtures include three i5 golden vectors and four actual HDL
simulation DMA writes, all driven by synthetic IQ. Their exact native bytes are
stored as patch-compatible hex with expected metadata and SHA256 of all normalized
float32 bins, derived from the i5 semantic JSON. Rust produces GOBS1; independent
Python checks every output byte via those payload hashes, epoch/seq/tick, ENBW,
single-plane nint=1 and visible seq gaps. Simulated records use source=1, with
distinct capture namespaces for native-vector and DMA-simulation families. They
are never relabelled hardware data. Transactional drain, full-ring wrap, lost
windows/delayed completion, bad payload and legacy/native HTTP policy are tested
without a board. Shared bounded TCP code is re-exercised on Linux and Windows.

The user explicitly deferred hardware testing because Pluto is disconnected.
Software acceptance does not establish PL-to-ARM runtime, cache/CDC/reset safety,
RF normalization, settling, real8 regression or ARM CPU/RSS. The next hardware
visit needs a compatible **A 1R1T** RAM-boot environment (kernel/DT/transports), the
verified temporary boot/rollback procedure, the matched new ELF and a static RF
test condition. Do not hot-load this bit into installed PL-5b/2R2T and call it a
matched pair. The i5 DMA can pre-issue AW while idle; SDR-only live reset is unsafe.
Keep TX off and existing lease/watchdog rules. Report/continuation:
C:/proj/pluto_ag/evidence/OBS-003-ARM.md and C:/proj/pluto_ag/CHAT_CONTEXT.md.

## Code rollback

For OBS-003, the retained firmware branch obs-002-contract is the saved point
`66e569206830c2b6d2631b17a4bc90db0c58cf6b`. With clean worktrees, switch firmware
back to that branch and run its `tools/obs-ci/sources.py prepare maia-sdr` to restore
the pinned Maia plus routing/OBS-002 stack. The OBS-003 branches/commits remain;
no reset/force/cleanup or hardware operation is needed. Dirty trees must be saved
before switching/preparing; the helper refuses them.
