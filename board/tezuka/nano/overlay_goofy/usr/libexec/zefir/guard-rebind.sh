#!/bin/sh
set -eu
R=/usr/libexec/zefir
test "$(devmem 0x7c460064)" = 0x00000010
test "$(devmem 0x7c4601e0)" = 0x00000000
test "$(devmem 0x7c460228)" = 0x00000000
! pidof obs-dma-read
! pidof maia-httpd
test "$(cat /sys/bus/iio/devices/iio:device0/out_altvoltage1_TX_LO_powerdown)" = 1
for en in /sys/bus/iio/devices/iio:device*/buffer/enable; do test "$(cat "$en")" = 0; done
test "$(basename "$(readlink /sys/bus/spi/devices/spi0.0/driver)")" = ad9361
if test -f "$R/gpio-owner.pid"; then
 pid=$(cat "$R/gpio-owner.pid");test "$(cat /proc/$pid/comm)" = gpioset;kill -TERM "$pid";sleep 0.2
fi
echo 79020000.cf-ad9361-lpc > /sys/bus/platform/drivers/cf_axi_adc/unbind
echo 79024000.cf-ad9361-dds-core-lpc > /sys/bus/platform/drivers/cf_axi_dds/unbind
echo spi0.0 > /sys/bus/spi/drivers/ad9361/unbind
insmod "$R/obs_ad9361.ko"
test "$(basename "$(readlink /sys/bus/spi/devices/spi0.0/driver)")" = ad9361-obs006d
# Probe after insmod may already have rebound DDS. Only an exact expected
# driver is accepted; EBUSY is never ignored without checking the result.
ensure_bound() {
    device=$1; driver=$2
    link=/sys/bus/platform/devices/$device/driver
    expected=/sys/bus/platform/drivers/$driver
    if test -L "$link"; then
        test "$(readlink -f "$link")" = "$expected"
    else
        if ! echo "$device" > "$expected/bind"; then
            test "$(readlink -f "$link")" = "$expected"
        fi
    fi
    test "$(readlink -f "$link")" = "$expected"
}
ensure_bound 79024000.cf-ad9361-dds-core-lpc cf_axi_dds
ensure_bound 79020000.cf-ad9361-lpc cf_axi_adc
p=/sys/bus/iio/devices/iio:device0
test "$(cat "$p/name")" = ad9361-phy
echo 1 > "$p/out_altvoltage1_TX_LO_powerdown"
echo -89.75 > "$p/out_voltage0_hardwaregain"
echo 61440000 > "$p/in_voltage_sampling_frequency"
echo 56000000 > "$p/in_voltage_rf_bandwidth"
echo manual > "$p/in_voltage0_gain_control_mode"
echo 40 > "$p/in_voltage0_hardwaregain"
echo 5850000000 > "$p/out_altvoltage0_RX_LO_frequency"
test "$(cat "$p/in_voltage_sampling_frequency")" = 61440000
test "$(cat "$p/out_altvoltage1_TX_LO_powerdown")" = 1
"$R/obs-fl-ctl" status
dmesg | tail -20
echo GUARDED_DRIVER_BOUND_PS_OWNER_TX_OFF
