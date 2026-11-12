# OBS-004 host row bridge

`gobs_rows.py` installs as `sdr/source/gobs_rows.py` in the existing host.
`rows.patch` adds only `RowHistory.append_precomputed` to `sdr/dsp/rows.py`.
No source URI, network thread, service or browser change is added.

Pentium original rows.py SHA256:
`0c70c3ca729df57687d656c28928ffc92626957f091104473eca40b3b957604f`.
Modified SHA256:
`69e23f408c155b54c985913ac9dc2fe87096b9b308a8f7f8a4c80e219a553fb8`.
Preserve a byte-exact backup and refuse applying over a different original.
Use `git -c core.autocrlf=false apply --check PATH/rows.patch`, then apply;
reverse the patch with the same setting and check the original hash for restore.
Do not replace an existing gobs_rows.py without inspecting its differences.
The original file, local guarded installer and tested patch roundtrip are kept
under `C:/proj/pluto_ag/evidence/obs-004/`; no host Git repository was created.

After a separately approved ARM build/hardware startup:

```powershell
C:/proj/goofy_v2/.venv/Scripts/python.exe -X utf8 firmware/tools/observation/live.py --connect 192.168.2.1:9009 --profile avg-peak --host-rows-root C:/proj/goofy_v2 --row-ring-rows 64 --seconds 120 --output evidence/obs-004/live-host-rows.json
```

This uses existing instrumental TCP reception and directly appends precomputed
rows, in that same loop. Borrowed payload is consumed before the next reader
read. No normalization/reintegration takes place in the bridge. Display bins are
10log10 of linear power, with the existing finite display floor for zero.
`measurements`/row head advance only on a new group; drawing never feeds data.
Existing bounded cursors account for slow-row-reader overwrites.

For direct integration, create `GobsRows(rows, reader.reader.session)` per capture
and call `accept(frame, descriptor, generation)` from the existing receive loop.
Generation must identify the immutable descriptor snapshot. Final integration
point is DeviceLink/Source selection in `sdr/source/link.py` and the row-producing
branch in `sdr/loop.py`, before `rows.feed_lin`/`rows.feed`; use this direct hook
instead of either legacy aggregation method. Publish matching `cls_desc` via
`rows.remember_descriptor` before handing rows to classifier cursors. The existing
`(d,e,g,analog)` snapshot requires source-specific descriptor mapping and a host
time anchor; this slice leaves that GUI/classifier connection explicit and undone.

Times here are source-clock seconds, with active duration nint*N/Fs, not dwell or
arrival timestamps. Row type5/storage widths restrict this minimal bridge to
nint<=65535, N<=32768, generation<=u32, nonzero epoch<=i64::MAX. Wider valid GOBS
records remain readable instrumentally but this bridge rejects them explicitly.
One mean+peak group is one measurement with true nint. New source/target requires
a new capture/bridge. The bridge retains only the last identity/signature;
identical last presentation is ignored, conflicting or older IDs are errors.

Targeted test, using the existing host venv and installed row module:

```powershell
C:/proj/goofy_v2/.venv/Scripts/python.exe -X utf8 firmware/tools/observation/host/test_gobs_rows.py --host-root C:/proj/goofy_v2 --rows C:/proj/goofy_v2/sdr/dsp/rows.py --real-dir evidence/obs-003-hw/compensated --output evidence/obs-004/installed-host-tests.json
```

Omit `--real-dir` on a machine without the private accepted RF records. That
case is skipped, never replaced with synthetic provenance. The CLI-hook test
uses an offline socket substitute, not a live device. Python reference reduction
of saved data tests the reader/host bridge; it does not execute ARM/Rust reduction.
