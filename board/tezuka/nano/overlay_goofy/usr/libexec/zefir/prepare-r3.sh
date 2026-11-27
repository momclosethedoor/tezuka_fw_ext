#!/bin/sh
set -eu
! pidof maia-httpd
p=/sys/bus/iio/devices/iio:device0
test "$(cat "$p/name")" = ad9361-phy
echo 1 > "$p/out_altvoltage1_TX_LO_powerdown"
echo -89.75 > "$p/out_voltage0_hardwaregain"
echo 61440000 > "$p/in_voltage_sampling_frequency"
echo 56000000 > "$p/in_voltage_rf_bandwidth"
echo manual > "$p/in_voltage0_gain_control_mode"
echo 40 > "$p/in_voltage0_hardwaregain"
echo 2400000000 > "$p/out_altvoltage0_RX_LO_frequency"
sleep 2
for f in in_voltage_sampling_frequency out_altvoltage0_RX_LO_frequency in_voltage_rf_bandwidth in_voltage0_gain_control_mode in_voltage0_hardwaregain out_altvoltage1_TX_LO_powerdown out_voltage0_hardwaregain; do echo "$f=$(cat "$p/$f")"; done
test "$(cat "$p/in_voltage_sampling_frequency")" = 61440000
test "$(cat "$p/out_altvoltage0_RX_LO_frequency")" = 2400000000
test "$(cat "$p/out_altvoltage1_TX_LO_powerdown")" = 1
# Fresh PL holds SDR reset. The sync CSR CDC cannot acknowledge in reset.
# Read only the AXI-domain identity first, then deassert the inherited reset
# once, with no admitted source/DMA. Never pulse/assert reset on this path.
test "$(devmem 0x7c460000)" = 0x6169616D
test "$(devmem 0x7c460008)" = 0x00000001
devmem 0x7c460008 32 0
# ID/capabilities are checked before any OBN2 spectrum control write.
test "$(devmem 0x7c460050)" = 0x354E424F
test "$(devmem 0x7c4600e4)" = 0x00000002
test "$(devmem 0x7c4601c0)" = 0x00000400
test "$(devmem 0x7c4601c4)" = 0x00008000
test "$(devmem 0x7c4601c8)" = 0x00001000
test "$(devmem 0x7c4601b8)" = 0x00000000
test "$(devmem 0x7c4601bc)" = 0x00000000
test "$(devmem 0x7c460064)" = 0x00000010
echo OBN5_R2_GEOMETRY_VERIFIED_NO_START_MEMORY_UNOWNED

# FIT-12 FLD1 disabled, owner PS, no fault before any measurement.
test "$(devmem 0x7c460224)" = 0x31444C46
test "$(devmem 0x7c4601e0)" = 0x00000000
test "$(devmem 0x7c460228)" = 0x00000000
echo FIT12_FLD1_DISABLED_SPI_PS

test "$(devmem 0x7c4600e0)" = 0x000001FF
test "$(devmem 0x7c460398)" = 0x000003FF
test "$(devmem 0x7c46039c)" = 0x000003FF
