# OBS-001-CI

ARM base: goofy-pl4 / 5162a799e7d22db7ec417473058e87be36bd8408, PL-5b.
PL development separately uses accepted FW2-R1-1R1T variant A on i5; no XSA/DT migration here.
Workspace: C:/proj/pluto_ag/{firmware,maia-sdr,artifacts,evidence}.
No local Rust, Buildroot or ARM compiler is required.

## Source cycle (PowerShell, workspace root)

Use Python from C:/proj/goofy_v2/.venv/Scripts/python.exe (abbreviated below).

```powershell
python firmware/tools/obs-ci/sources.py prepare maia-sdr
# Edit maia-sdr/maia-httpd. Stage any newly created source files there.
python firmware/tools/obs-ci/sources.py export maia-sdr --patch 0002-local.patch
# Commit recipe, ordered package patches and source.json together in firmware.
# For further edits, prepare again, then export a new numbered patch.
python firmware/tools/obs-ci/sources.py verify maia-sdr
git -C firmware push origin obs-001-ci
gh workflow run main.yml -R momclosethedoor/tezuka_fw_ext --ref obs-001-ci -f boards=nano -f release=false -f mode=arm-only
# Record run ID, run_attempt and head_sha before download.
gh run view RUN_ID -R momclosethedoor/tezuka_fw_ext --json headSha,attempt,status,conclusion,url
gh run download RUN_ID -R momclosethedoor/tezuka_fw_ext --dir artifacts/RUN_ID-attempt-N
```

Preparation refuses dirty trees. The local obs-patch-base commit is only a developer
baseline; CI fetches the pinned upstream archive and applies package patches in
lexical order. source.json fingerprints exact maia-httpd file bytes, including Cargo.lock,
using explicit POSIX path string sorting (identical on Windows/Linux);
CI rejects divergence. Export is restricted to maia-httpd, no product change is
needed to validate this delivery chain. Git must use core.autocrlf=false.

## CI contract

One existing workflow, Nano/release=false. Use mode=arm-only for edit/test cycles;
mode=full also builds and verifies the firmware bundle. Full and ARM-only have
separate concurrency groups, so dispatching ARM-only does not cancel a full run.
Both export the ELF immediately after compilation. ARM-only skips the expensive
full-image runner disk cleanup. Download/ccache/tree caches survive later build
failures; full output is saved only after successful verification. ARM-only verifies target staging;
only full mode certifies equality with the packed rootfs and FIT.
 Rust 1.94.0, cargo --locked,
Buildroot TARGET_CROSS and STAGING_DIR. Downloads/ccache/Buildroot caches remain.
The exact output key includes package patches and board inputs (including XSA).
Cross-mode restoration permits only the identical input fingerprint: full make can
finish a valid partial ARM cache, and ARM can reuse full dependencies. Verification
script/doc changes do not invalidate the build output cache.
Maia and board-fpga dirclean remove stale extraction/patch stamps on every run;
Maia must print a real Compiling line. First run may require a full dependency build.

maia-httpd-nano contains the ELF, SHA256SUMS, ELF inspection, manifest and compiler log.
tezuka-nano retains the normal firmware bundle plus the same manifest.
Linux gates compare the ELF to the xz within packed rootfs; resolve loader, shared
libraries and symbol versions; compare all FIT components; enforce PL-5b hash and
stock flash budget. No Vivado runs. Never flash these outputs in this slice.

## Hardware procedure

Use existing tools/tz_board.py, its pinned host key/serial policy and maia_ws_probe.py
from goofy_v2. Keep private connection settings and snapshots outside this repository.
Record /api, relevant settings, process cmdline, executable hash, init/inittab,
dmesg and memory first. Keep the original /usr/bin/maia-httpd untouched.
Copy the candidate to a unique /tmp directory and verify its SHA256 on the board.
Stop S60maia-httpd, wait for its PID to disappear, then start the candidate from
/root with the same listen/TLS arguments. Never run two DMA owners. Leave the
lease/video watchdogs intact. Arrange a timed rollback before stopping the service.
After at most five minutes (normally 30–60 seconds), kill and wait for the temporary
PID, start S60maia-httpd, restore saved settings and confirm fresh spectra/host rows.
The local tested orchestration is `evidence/hardware.py` (outside source trees):

```powershell
# Arguments are the exact downloaded artifact, expected run ID, head SHA, attempt.
C:/proj/goofy_v2/.venv/Scripts/python.exe -X utf8 evidence/hardware.py artifacts/RUN/maia-httpd-nano/maia-httpd RUN_ID HEAD_SHA 1
```

Its board transaction uses these commands, with a unique temporary directory D;
it arms a detached 150-second rollback before stopping the stock service:

```sh
/etc/init.d/S60maia-httpd stop
# Wait until pidof maia-httpd is empty, otherwise abort.
cd /root
"$D/maia-httpd" --listen 0.0.0.0:80 --ssl-cert /mnt/jffs2/maia-httpd.crt --ssl-key /mnt/jffs2/maia-httpd.key --ca-cert /mnt/jffs2/maia-sdr-ca.crt >"$D/candidate.log" 2>&1 &
echo $! >"$D/pid"
/usr/sbin/goofy_tx_off obs-001-candidate 10
# Rollback, also performed by the detached guard on host/SSH failure:
kill "$(cat "$D/pid")"
# Wait until that PID exits before restarting the stock owner.
/etc/init.d/S60maia-httpd start
/usr/sbin/goofy_tx_off obs-001-restore 10
# Restore saved RX/spectrometer settings through the existing API; check fresh rows.
```

Never enable TX for this test. A WebSocket handshake alone proves neither fresh
reception nor RF measurement accuracy. Record unsupported video preconditions.

## Continuation

OBS-003 independent streaming work is on `obs-003-arm`, based on OBS-002 commit
66e569206830c2b6d2631b17a4bc90db0c58cf6b. Per-record codec, bounded fanout,
explicit synthetic TCP exerciser and headless live reader are documented in
[OBSERVATION-V1.md](OBSERVATION-V1.md). Local execution report and exact build
provenance: C:/proj/pluto_ag/evidence/OBS-003-ARM.md. OBS-003 as a whole stays open
until i5 supplies the native ABI/golden records and successful compatible P&R
bit/bin, followed by the agreed PL/ARM hardware acceptance and restoration.
The user will forward that handoff when available; no substitute ABI or PL-5b
metadata is invented. No board access is required for the independent tests.

Independent part passed run 43129725401, attempt 1, head
279e1dffa63b867de8c551459cb963e65937f608: Linux/Windows native tests, TCP backpressure,
Pentium bounded-memory test and Nano ARM-only delivery. Artifacts downloaded under
C:/proj/pluto_ag/artifacts/43129725401-attempt-1/. Maia ELF SHA256
9c4a209af450d55bc0a55242304757d924a05bc9aa9a952d497dd93468637263.
No native DMA adapter or hardware acceptance is claimed. Later tooling/docs commits
do not change the compiled Maia tree; the source manifest remains authoritative.

OBS-002 contract/replay is accepted; canonical format and OBS-003 hook points:
[OBSERVATION-V1.md](OBSERVATION-V1.md). Rust/Python shared-vector checks and ARM-only
run 43118803431 (attempt 1, head 75f0acee03469bd009a25512bc228981ad7e417b) passed.
Local report: C:/proj/pluto_ag/evidence/OBS-002-CONTRACT.md. No live endpoint or
hardware was changed in OBS-002. Remaining OBS-003 work must carry real acquisition seq,
RF visit/map generation, first-sample clock and completed count through FFT/DMA,
then connect the bounded live producer/reader adapters. PL-5b does not supply those fields.
The host tracker IndexError 66/64 remains open; failing state was not captured.

## Acceptance scope after lead clarification

The existing PL-5b firmware/run 40190150385 is accepted as the firmware baseline.
Only ARM-only delivery and hardware smoke are required for this execution; no new
full image is needed. Full run 43100358402 was cancelled as instructed after the
first ARM run 43104265437 exposed a cross-platform source fingerprint bug.
The corrected local tree matches the exact upstream tar plus GNU patch, 49 files.
Initial CRLF files and platform-dependent Path sorting were both corrected.
Failed deliveries preserve an explicitly unverified diagnostic ELF and source map;
a successfully compiled ARM dependency tree can be cached despite verification
failure because Maia is always removed/re-extracted/re-patched on the next run.

## Accepted execution, 2026-11-12

ARM-only delivery and hardware API/RX/row/real8 transport smoke passed. Successful
run: https://github.com/momclosethedoor/tezuka_fw_ext/actions/runs/43108518497,
attempt 1, head 1265146212550a5c9a8100f898ff27c53d9e6f78. Artifact maia-httpd-nano
(ID 13895333237), ELF 6,225,744 bytes, SHA256
7367e94eeeb11adecf364ecf0ac2352d965589b4ea7e73cb8409f7b7c4343ff7.
Source fingerprint 8470d0e05ee6bc27307e779907f14356fe6dc2440a5f6e5faf2922ef717864e9,
49 exact files. GCC 15.1.0 / Bootlin 2025.08-1; Rust 1.94.0 --locked.
Runner job: 855 seconds; Rust release: 4m23s. All four dependency/output cache saves
succeeded. A warm-cache repeat was not run after the scope reduction.

On Pluto, /proc/PID/exe hash matched the artifact and API reported git 1265146.
74 fresh direct frames; 16 distinct spectral payloads excluding the tag. Host rows
75 -> 674, 593 unique WS messages, zero time/tag losses. Real8: 881 blocks/15.03s,
zero block losses, no overrun. RSS 7,760 -> 7,812 KiB; virtual size 230,384 KiB.
Kernel log unchanged. Two peer resets occurred when test clients disconnected.
No loader/library or new DMA/IIO errors. Video picture/RF accuracy and long-term
memory were not accepted: demod remained searching, clipping was saturated as in
the baseline. A host tracker IndexError (index 66 vs array length 64; failing index/array not captured) occurred once;
one stock-ELF control did not reproduce it. Cause remains open; classifier code
was not changed and full classifier regression is not claimed.

Stock ELF restored (SHA256 6e13d631f6e485bb11038799ff12d2c5130abc3220d23b6e42b2f95bec840596),
RX/DDC/spectrometer/settings matched, fresh host rows 116 -> 714 after rollback.
TX remained off, lease/video watchdogs stayed enabled, temporary Pluto files were
removed. No flash or PL/DT/kernel/env changes. XSA remains byte-identical to 5162a79
(SHA256 6175da5e91bf77747348449bcc3301e89c0c4daf18d1917855ab981038ce48f5).
The old full image/run 40190150385 remains the accepted firmware baseline; no claim
is made that its embedded ELF equals this new ARM ELF. Full-mode checks exist but
were not accepted on a completed full run in this slice.

Three source-fingerprint regression tests pass on Windows; they are included in
future CI runs. Later commits add tests/docs without changing the compiled Maia
source or recipe. Full local evidence/report: C:/proj/pluto_ag/evidence/OBS-001-CI.md;
private host/connection data and raw captures remain outside this repository.
Current continuation and the retained host tracker finding are linked above.
