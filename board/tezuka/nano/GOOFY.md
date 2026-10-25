# goofy-pl4 — nano: bitstream PL-5b + overlay_goofy

Branch `goofy-pl4` of the fork, based strictly on tag `v0.3.24` (`a304a1f`). Full description (Russian):
`E:\pl\docs\FW_BUILD.md`, lease contract `E:\pl\docs\LEASE.md`, video block contract `E:\pl\docs\VIDEO_REAL8.md`.

Changes vs v0.3.24 (nano only; no other board, no common file is touched):
- `bitstream/tezuka.xsa` — maia-sdr 22d590c + patches 0001, 0003-0013 (PL-5b, FM video block v5);
  `system_top.bit.bin` sha256 `ac2b2a2c7c7588606222e5a26c0aff94767a42fe26ddd16c869478a08bc1c057`.
- `configs/nano_defconfig` — `overlay_goofy` appended last to `BR2_ROOTFS_OVERLAY`.
- `overlay_goofy/` (busybox sh only, no new packages):
  - `S01goofy-txoff`, `S62goofy-txoff-httpd`, `usr/sbin/goofy_tx_off`, `usr/lib/goofy/tx.sh` — TX off at boot
    (right after ad9361 probe and again after maia-httpd starts), readback, log `/tmp/goofy_boot.log`;
  - `etc/inittab` (common copy + 2 respawn lines) and `usr/sbin/goofy_svc` — both services below run under
    init respawn (started after rcS);
  - `usr/sbin/goofy_lease.sh`, `etc/goofy/lease_enforce` — TX lease over MQTT (`goofy/lease/hb`, 1 s);
    no lease for > 5 s -> TX forced off, video block mode 0; removing the flag file disables it;
  - `usr/sbin/pl_video_ctl.sh` — FM video block watchdog (`cmd/video/pl`, `state/video/pl`);
  - `root/watchconsoletx.sh`, `root/watchdatveasy.sh` — wrappers: not started while the lease flag exists,
    otherwise exec the unchanged originals copied to `usr/libexec/goofy/tezuka/`;
  - stubs overriding common files: `S61classifier` (classifier off), `S51wireguard` + `usr/bin/wg-quick`
    (WireGuard off: PreUp/PostUp of a config pushed over anonymous MQTT = root code execution),
    `S95bgcript` (no `/mnt/jffs2/backdoor.sh`), `S98autostart` (no `/mnt/jffs2/autorun.sh`),
    `lib/mdev/automounter.sh` (no `runme*` from USB storage).

Register writes from the overlay: TX attributes of ad9361-phy / cf-ad9361-dds-core-lpc and the FM block
registers `0x7C460038`, `0x7C460040` only.
