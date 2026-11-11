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

## Code rollback

The retained firmware branch obs-001-ci is the saved point
`eb6e8062c970b5c40f8279ab192ee4512d2a2a77`. With clean worktrees, switch firmware
back to that branch and run its `tools/obs-ci/sources.py prepare maia-sdr` to restore
the pinned Maia plus routing-only stack. The OBS-002 branches/commits remain;
no reset/force/cleanup or hardware operation is needed. Dirty trees must be saved
before switching/preparing; the helper refuses them.
