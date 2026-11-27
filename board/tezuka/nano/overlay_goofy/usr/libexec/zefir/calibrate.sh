set -eu
p=/sys/bus/iio/devices/iio:device0
d=/sys/kernel/debug/iio/iio:device0
test "$(cat "$p/out_altvoltage1_TX_LO_powerdown")" = 1
echo 0 > "$d/adi,rx-fastlock-pincontrol-enable"
echo 0 > "$d/adi,tx-fastlock-pincontrol-enable"
echo 2400000000 > "$p/out_altvoltage0_RX_LO_frequency"
sleep 0.1
actual=$(cat "$p/out_altvoltage0_RX_LO_frequency"); delta=$((actual-2400000000)); test "$delta" -ge -10; test "$delta" -le 10
echo 0 > "$p/out_altvoltage0_RX_LO_fastlock_store"
echo 0 > "$p/out_altvoltage0_RX_LO_fastlock_save"
cat "$p/out_altvoltage0_RX_LO_fastlock_save"
echo 2412000000 > "$p/out_altvoltage0_RX_LO_frequency"
sleep 0.1
actual=$(cat "$p/out_altvoltage0_RX_LO_frequency"); delta=$((actual-2412000000)); test "$delta" -ge -10; test "$delta" -le 10
echo 0 > "$p/out_altvoltage0_RX_LO_fastlock_store"
echo 0 > "$p/out_altvoltage0_RX_LO_fastlock_save"
cat "$p/out_altvoltage0_RX_LO_fastlock_save"
echo 2424000000 > "$p/out_altvoltage0_RX_LO_frequency"
sleep 0.1
actual=$(cat "$p/out_altvoltage0_RX_LO_frequency"); delta=$((actual-2424000000)); test "$delta" -ge -10; test "$delta" -le 10
echo 0 > "$p/out_altvoltage0_RX_LO_fastlock_store"
echo 0 > "$p/out_altvoltage0_RX_LO_fastlock_save"
cat "$p/out_altvoltage0_RX_LO_fastlock_save"
echo 2436000000 > "$p/out_altvoltage0_RX_LO_frequency"
sleep 0.1
actual=$(cat "$p/out_altvoltage0_RX_LO_frequency"); delta=$((actual-2436000000)); test "$delta" -ge -10; test "$delta" -le 10
echo 0 > "$p/out_altvoltage0_RX_LO_fastlock_store"
echo 0 > "$p/out_altvoltage0_RX_LO_fastlock_save"
cat "$p/out_altvoltage0_RX_LO_fastlock_save"
echo 2448000000 > "$p/out_altvoltage0_RX_LO_frequency"
sleep 0.1
actual=$(cat "$p/out_altvoltage0_RX_LO_frequency"); delta=$((actual-2448000000)); test "$delta" -ge -10; test "$delta" -le 10
echo 0 > "$p/out_altvoltage0_RX_LO_fastlock_store"
echo 0 > "$p/out_altvoltage0_RX_LO_fastlock_save"
cat "$p/out_altvoltage0_RX_LO_fastlock_save"
echo 2460000000 > "$p/out_altvoltage0_RX_LO_frequency"
sleep 0.1
actual=$(cat "$p/out_altvoltage0_RX_LO_frequency"); delta=$((actual-2460000000)); test "$delta" -ge -10; test "$delta" -le 10
echo 0 > "$p/out_altvoltage0_RX_LO_fastlock_store"
echo 0 > "$p/out_altvoltage0_RX_LO_fastlock_save"
cat "$p/out_altvoltage0_RX_LO_fastlock_save"
echo 2472000000 > "$p/out_altvoltage0_RX_LO_frequency"
sleep 0.1
actual=$(cat "$p/out_altvoltage0_RX_LO_frequency"); delta=$((actual-2472000000)); test "$delta" -ge -10; test "$delta" -le 10
echo 0 > "$p/out_altvoltage0_RX_LO_fastlock_store"
echo 0 > "$p/out_altvoltage0_RX_LO_fastlock_save"
cat "$p/out_altvoltage0_RX_LO_fastlock_save"
echo 2484000000 > "$p/out_altvoltage0_RX_LO_frequency"
sleep 0.1
actual=$(cat "$p/out_altvoltage0_RX_LO_frequency"); delta=$((actual-2484000000)); test "$delta" -ge -10; test "$delta" -le 10
echo 0 > "$p/out_altvoltage0_RX_LO_fastlock_store"
echo 0 > "$p/out_altvoltage0_RX_LO_fastlock_save"
cat "$p/out_altvoltage0_RX_LO_fastlock_save"
