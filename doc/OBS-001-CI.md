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

Run-specific results are recorded in the workspace evidence/OBS-001-CI.md.
OBS-002 begins with observation contracts and the replay harness only after delivery
acceptance. No spectrum, waterfall, DMA, accumulation or product protocol changes here.

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
