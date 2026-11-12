# OBN1 native ABI · revision 1 · OBS-003-PL

Frozen early interface, 2026-11-12. Applies only to the matching experimental
PL/ARM pair, based on accepted 1R1T A + PL-PREP ccbd2a58. OBS-002 reference:
firmware 66e569206830c2b6d2631b17a4bc90db0c58cf6b, OBSERVATION-V1.
Any subsequent layout change requires an explicit new ABI revision/ID and
replacement early handoff. This is not GOBS1 on the FPGA.

## DMA record

Existing spectrometer AXI3 port, DDR base **0x16000000**, **8 slots** of 32768 bytes
(the maia_iio_lite configuration), 4096 little-endian u64 per slot. No DT/kernel
or DDR reserved-memory change. One slot is one complete nonoverlapping FFT
window, nint=1, one power plane. No bin is sacrificed.

For every word j in 0..4095:

- bits 0..36: unsigned power mantissa M (37 bits).
- bits 37..39: exponent E, range 0..4.
- bits 40..47: zero.
- bits 48..63: header halfword H[j] for j<32; zero for all other j.

Concatenate H[0]..H[31] as little-endian u16 to obtain the 64-byte header:

| Byte | Type | Meaning |
|---:|---|---|
| 0 | u32 | 0x314e424f (ASCII OBN1) |
| 4 | u16 | revision=1 |
| 6 | u16 | header bytes=64 |
| 8 | u64 | producer seq |
| 16 | u64 | effective RF epoch (0 whenever clean=0) |
| 24 | u64 | first contributing FFT-input sample sync tick |
| 32 | u32 | N=4096 |
| 36 | u32 | actual nint=1 |
| 40 | u32 | source counter Hz=62500000 |
| 44 | u32 | flags: bit0 clean, bit1 FFT input uses DDC; other bits zero |
| 48 | u32 | payload encoding=1 (M37E3, order/scale below) |
| 52 | u32 | native record bytes=32768 |
| 56 | u64 | reserved zero |

## Payload and conversion to OBSERVATION-V1

Retained FFT: signed16 IQ at the FFT input, N4096, R22, truncates [[0,1]]*6,
twiddle16, symmetric Blackman-Harris quantized to coefficients 0..511 and
divided by 512. FFT output signed22 has total butterfly division 64.
IQToFloatingPoint(22,18) yields R=arithmetic_shift(re,E), I=arithmetic_shift(im,E),
with the same E for both. Existing CpwrPeak computes M=R*R+I*I, no accumulation.
Native bins remain in FFT **bit-reversed** order; physical word j represents
unshifted Fourier bin bit_reverse_12(j). ARM reorders/fftshifts:
native word for GOBS bin k is bit_reverse_12((k+2048) mod 4096).

Let w[n]=round(511*BlackmanHarris_symmetric[n])/512 and S=sum(w[n]).
Linear full-scale-squared GOBS power is
`float32(ldexp(M,2*E) * 4096 / (32768^2 * S^2))`.
This uses a complex tone of full-scale amplitude 32768 FFT-input units, equal
to 2048 ADC units for the raw left-shifted-by-4 input. DDC gain must be described
in the ARM descriptor/provenance; no RF dBm calibration is implied. ARM supplies
sample rate and RF grid from its immutable epoch descriptor, never live readback.
ENBW=`4096*sum(w*w)/(S*S)`. ARM performs normalization and reordering exactly
once, not the legacy 4e6/Fs or K(Fs) conversion. No division by nint is needed.
All native bins are measured; dirty whole-window attribution is epoch=0,
not missing power or invented NaNs. GOBS provenance=hardware-stamped, reduction
mean (one FFT), legacy tag=MAX. ARM owns capture UUID and GOBS encoding.

## Input metadata and clocks

Seq starts at zero after coordinated PL reset and advances for **every** input
window before selection/loss; it does not restart on ARM series commands.
Tick is a free-running u64 in maia sync 62.5 MHz, reset to zero. Latch tick and
seq at the first accepted input sample after the raw/DDC/CDC mux. Epoch names
a visit/configuration generation, not a fastlock slot. Every change inside a
window (also during a sample pause), any low sample_valid, a source-mux change,
and the first priming window after reset force clean=0/epoch=0. A nonzero epoch
requires external validity and stable nonzero context for the entire window.
There is no claim to measured settling. Static RF configuration only.

## Registers (existing maia AXI window, byte offsets)

Existing aliases 0x18/1c, video 0x38/3c/40..4c, product_id/_version and board
ports remain unchanged. Old spectrometer nint/peak/abort fields at 0x20 no
longer control observation processing. Its last_buffer field still reports the
last completed DMA slot. `clk_fastlock_out` is held low in this static-RF build.

| Offset | Access | Meaning |
|---:|---|---|
| 0x50 | R | ABI ID=0x314e424f; mandatory startup check |
| 0x54 | RW/pulse | bit0 sample_valid (reset0), bit1 arm pulse; other bits0 |
| 0x58 | RW | staged epoch low32 |
| 0x5c | RW | staged epoch high32 |
| 0x60 | RW | series selected-window count, 1..8 |
| 0x64 | R | bit0 series_busy, bit1 DMA busy, bit2 fault, bit3 command_rejected sticky |
| 0x68 | R | produced input windows, low32/modulo |
| 0x6c | R | selected input windows, low32/modulo |
| 0x70 | R | successfully recorded DMA windows, low32/modulo |
| 0x74 | R | selected windows lost, low32/modulo |
| 0x78 | R | last successfully recorded producer seq low32 |
| 0x7c | R | last successfully recorded producer seq high32 |

Arm snapshots the staged epoch atomically and grants a bounded series of the
next 1..8 input windows beginning at a first-sample boundary. Command is rejected
while a series is busy, on fault, or for count outside 1..8. Validity is an
FFT-input-domain control. Keep it low while configuring RF/DDC/mux, then assert
only after the externally chosen settling interval. Arm is one command per
series, never a per-FFT acknowledgment. RF context changes via arm can dirty a
currently running unselected window; they never relabel a previous result.

## Ownership, loss, completion and reset

Two banks replace the existing integrator ping-pong. At selected result start,
claim a free bank for the **whole** power frame; lock a completed bank until all
DMA write responses for that record return. If both banks are owned, drop the
whole selected observation and increment lost; its producer seq is not reused.
Unselected observations advance producer seq/produced but not selected/lost.
No hidden nint growth and no FFT backpressure. Pending selected windows are
visible as selected-recorded-lost (mod32, bounded by one series).

DMA slot becomes readable only after the final successful B response: then
recorded increments, last_buffer/last_seq update, and existing spectrometer IRQ
pulses. Any AXI error latches fault, prevents new starts/selections, and invalidates
the series; ARM must discard it and coordinate reset. AW/W stalls or delayed B
must not unlock/rewrite a bank or publish partial data. IRQ coalescing is not a
frame counter; use recorded deltas and slots. A series writes at most 8 slots,
then stops, preserving all its successful records until ARM explicitly rearms.
ARM must drain/copy the previous series before rearming; concurrent ring reads
and wrap overwrite are outside this bounded-series contract.

Coordinated PL/system reset cancels partial/pending transfers and resets seq,
tick/counters/ring pointer; discard all DDR contents until a new DMA completion.
Never reset PL while its AXI master has outstanding transactions unless the
interconnect is reset/drained together. ARM allocates a fresh capture ID after
any counter/PL reset or wrap. A process restart may attach only with authoritative
capture state; otherwise coordinate a new capture/reset. Re-arm alone does not
reset producer lifetime. Native seq/tick u64 wrap requires a new capture.

## Pairing and early handoff

ARM must read 0x50 and require exactly OBN1, validate the header constants and
the staged descriptor before interpreting any ring buffer. Legacy PL-5b/A have
no OBN1: refuse to label their buffers hardware observations. Legacy waterfall
code is incompatible with this payload and must not consume this DMA ring.
Use the separate bounded OBS adapter from Pentium; video/IQ paths remain as A.

`native_vectors.py` generates/decodes native revision1 records with semantic JSON
and normalized powers. Early `handoff/native-abi-r1.zip` is the layout authority,
not a board-ready image. Its SHA256SUMS and immutable revision identify the exact
handoff. Synthetic vectors validate bytes/normalization; later DMA simulation
records validate the actual producer. No direct publication or board action.
