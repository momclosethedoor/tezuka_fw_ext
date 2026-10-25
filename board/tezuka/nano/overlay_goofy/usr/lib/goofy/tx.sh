# Общие функции гашения TX ad9361 (оверлей goofy nano, срез PL-4a; E:\pl\docs\FW_BUILD.md).
# Подключается через «.»; busybox sh, в проверке раз в секунду без fork (read/echo — встроенные).
# Пишет только TX-атрибуты: ad9361-phy out_altvoltage1_TX_LO_powerdown, out_voltage{0,1}_hardwaregain;
# goofy_dds_off — cf-ad9361-dds-core-lpc out_altvoltage*_raw (только при загрузке).

GOOFY_LOG=${GOOFY_LOG:-/tmp/goofy_boot.log}

goofy_log() {
	read -r _u _rest </proc/uptime
	echo "[$_u] $*" >>"$GOOFY_LOG"
}

# PHY, DDS — каталоги iio ad9361-phy и cf-ad9361-dds-core-lpc; код 0, если PHY найден.
goofy_find_iio() {
	PHY="" DDS=""
	for _d in /sys/bus/iio/devices/iio:device*; do
		_n=""
		read -r _n 2>/dev/null <"$_d/name" || continue
		case "$_n" in
		ad9361-phy) PHY=$_d ;;
		cf-ad9361-dds-core-lpc) DDS=$_d ;;
		esac
	done
	[ -n "$PHY" ]
}

# Миллидецибелы из строки «-89.750000» -> $MDB (без fork; дробная часть — первые 3 цифры).
goofy_mdb() {
	case "$1" in -*) _s=-1 _v=${1#-} ;; *) _s=1 _v=$1 ;; esac
	_i=${_v%%.*}; _f=${_v#"$_i"}; _f=${_f#.}000; _f=${_f%"${_f#???}"}
	case "$_i" in ''|*[!0-9]*) MDB=""; return 1 ;; esac
	MDB=$(( _s * (_i * 1000 + 1$_f - 1000) ))
}

# MIN — максимальное ослабление TX из out_voltage0_hardwaregain_available «[min step max]» (иначе -89.75).
goofy_tx_min_gain() {
	MIN=-89.75
	_a=""
	read -r _a 2>/dev/null <"$PHY/out_voltage0_hardwaregain_available"
	_a=${_a#\[}; _a=${_a%% *}
	goofy_mdb "$_a" && [ "$MDB" -lt 0 ] && MIN=$_a
	goofy_mdb "$MIN"; MIN_MDB=$MDB
}

# Обратное чтение: TX_PD (TX_LO_powerdown), TX_G0/TX_G1 (дБ, «-» — канала нет).
goofy_tx_read() {
	TX_PD="?" TX_G0="?" TX_G1="-"
	read -r TX_PD 2>/dev/null <"$PHY/out_altvoltage1_TX_LO_powerdown"
	read -r TX_G0 _rest 2>/dev/null <"$PHY/out_voltage0_hardwaregain"
	[ -e "$PHY/out_voltage1_hardwaregain" ] && read -r TX_G1 _rest 2>/dev/null <"$PHY/out_voltage1_hardwaregain"
}

# Код 0 — TX погашен: TX_PD = 1 и ослабление каждого канала не меньше максимального − 0.5 дБ.
goofy_tx_is_off() {
	[ "$TX_PD" = 1 ] || return 1
	[ -n "$MIN_MDB" ] || goofy_tx_min_gain
	for _g in "$TX_G0" "$TX_G1"; do
		[ "$_g" = "-" ] && continue
		goofy_mdb "$_g" || return 1
		[ "$MDB" -le $(( MIN_MDB + 500 )) ] || return 1
	done
	return 0
}

# Погасить TX и прочитать обратно; код 0 — подтверждено.
goofy_tx_off() {
	goofy_tx_min_gain
	echo 1 2>/dev/null >"$PHY/out_altvoltage1_TX_LO_powerdown"
	for _c in 0 1; do
		[ -e "$PHY/out_voltage${_c}_hardwaregain" ] && echo "$MIN" 2>/dev/null >"$PHY/out_voltage${_c}_hardwaregain"
	done
	goofy_tx_read
	goofy_tx_is_off
}

# DDS-тоны выкл. (запись 0 в *_raw), DDS_RAW — обратное чтение первого; TX_BUF — buffer/enable DDS.
goofy_dds_off() {
	DDS_RAW="-" TX_BUF="-"
	[ -n "$DDS" ] || return 1
	for _f in "$DDS"/out_altvoltage*_raw; do
		[ -e "$_f" ] || continue
		echo 0 2>/dev/null >"$_f"
		[ "$DDS_RAW" = "-" ] && read -r DDS_RAW 2>/dev/null <"$_f"
	done
	read -r TX_BUF 2>/dev/null <"$DDS/buffer/enable"
	[ "$DDS_RAW" = 0 ]
}
