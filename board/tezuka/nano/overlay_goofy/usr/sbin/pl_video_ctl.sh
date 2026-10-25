#!/bin/sh
# Управление блоком FM-дискриминатора в PL maia (docs/hw/VIDEO_REAL8.md) — срез PL-2, окончательный механизм:
# в PL-4 файл идёт в оверлей прошивки (путей /tmp внутри нет; каталог состояния —
# PL_VIDEO_RUN, по умолчанию /run/pl_video). busybox sh + mosquitto_sub/mosquitto_pub + devmem.
# PL-4a (оверлей goofy nano, E:\pl\docs\FW_BUILD.md) — три добавки к версии goofy_v2 (ref/board):
#   аренда: PL_VIDEO_ALLOW=<файл> (задаёт S64goofy-video) — mode ≠ 0 только пока файл есть (его держит
#     goofy_lease.sh); файла нет — команды mode ≠ 0 исполняются как mode 0, заданный mode ≠ 0 сбрасывается
#     в 0 на ближайшем тике; без переменной — как раньше;
#   счётчик насыщений (0x3C [15:0]) сбрасывается (sat_clear) при каждой смене mode / g / полей 0x40 — и
#     командой, и принудительно по аренде; reset_sat — как раньше;
#   в состоянии: sat_clears (число сбросов sat_clear), lease_block (1 — mode ≠ 0 запрещён арендой).
#
# Команда: топик cmd/video/pl (без retain), JSON {"mode":0|1|2,"g":0..3,"reset_sat":true} или
#   простые поля "mode=1 g=1 reset_sat=1"; отсутствующее поле — без изменений.
# Регистр 0x7C460038: [1:0] mode, [3:2] g, [4] sat_clear (импульс). Держится в 0, пока sdr_reset
#   (0x7C460008 бит 0) = 1; maia-httpd снимает сброс при старте.
# Блок v4+ (PL-5/PL-5b, VIDEO_REAL8.md §3): поля команды deemph, std, decim, color, marks (0/1), pol (0..3),
#   in_rate (0/1, только v5), clear (импульс counters_clear) -> 0x40 video_control (биты 0-6, 8; сброс 0x18).
#   На блоке < v4 адреса 0x40+ — алиасы 0x00+: туда НЕ пишем (поля игнорируются). В команде с обоими
#   регистрами сначала 0x40, затем 0x38 (сегмент видео начинается сразу с нужными настройками). Порядок
#   смены in_rate вместе с DDC (§11) — забота хоста: он шлёт шаги отдельными командами.
# Сторож: 0x38 читается раз в PERIOD_MS (≈ 200 мс); 0x38 ≠ заданному при снятом sdr_reset — запись
#   заданного и +1 к счётчику восстановлений (окно неверного состояния ≤ PERIOD_MS + время devmem).
# Состояние: retained state/video/pl (JSON, одна строка) — при изменении и не реже раза в секунду:
#   present, block, fixes (v3: 0x3C [19:16], иначе 0), desired{mode,g}, actual{mode,g,raw}, sat, sdr_reset,
#   v4+: video{desired, actual, status, marks, info} — сырые слова 0x40/0x44/0x48/0x4C (разбор — на хосте),
#   restores, last_restore_ms,
#   last_ok_ms (последнее чтение, где actual = desired, перед восстановлением), up_ms (uptime платы), seq.
#   Last Will того же соединения: {"alive":false}.
# Не наш битстрим (0x3C: [31:24] ≠ 0xFD или версия 0) — present:false, регистры не пишутся.
# Стоимость: devmem ≈ 8 мс CPU на вызов (замер 2026-10-24): 0x38 каждые 200 мс, 0x3C раз в секунду,
#   0x08 — только при расхождении -> ≈ 6 вызовов/с; замер за 20 с: 6.2 % одного ядра A9 (devmem 5 %, sh 1.2 %).
#   v4+: + 0x40 раз в секунду, 0x44 и 0x48 раз в секунду только в mode 1 -> ≤ 9 вызовов/с (≈ 9 %).
#   Сброс банка (sdr_reset) обнуляет 0x38 и 0x40 вместе: в mode 1 его ловит сторож 0x38 (200 мс) и
#   восстанавливает оба регистра; в mode 0 расхождение 0x40 ловится раз в секунду.

RUN=${PL_VIDEO_RUN:-/run/pl_video}
BROKER=${PL_VIDEO_BROKER:-127.0.0.1}
PERIOD_MS=${PL_VIDEO_PERIOD_MS:-200}
ALLOW=${PL_VIDEO_ALLOW:-}
T_CMD=cmd/video/pl
T_STATE=state/video/pl
R_ID=0x7C460000
R_RESET=0x7C460008
R_CTL=0x7C460038
R_STAT=0x7C46003C
R_VCTL=0x7C460040
R_VSTAT=0x7C460044
R_VMARKS=0x7C460048
R_VINFO=0x7C46004C
V_MASK=383  # биты 0-6 и 8 регистра 0x40 (7 — импульс counters_clear, читается 0)

mkdir -p "$RUN" || exit 1
echo $$ > "$RUN/pl_video_ctl.pid"
CMD_FIFO="$RUN/cmd.fifo"
PUB_FIFO="$RUN/pub.fifo"
rm -f "$CMD_FIFO" "$PUB_FIFO"
mkfifo "$CMD_FIFO" "$PUB_FIFO" || exit 1
# <> — открытие без ожидания второй стороны; обе стороны переживают перезапуск клиентов MQTT.
exec 3<>"$CMD_FIFO" 4<>"$PUB_FIFO"
trap '' PIPE

( exec 3<&- 4<&-; while :; do mosquitto_sub -h "$BROKER" -i pl_video_sub -t "$T_CMD" 2>/dev/null; sleep 1; done ) >"$CMD_FIFO" &
SUB_PID=$!
( exec 3<&- 4<&-; while :; do mosquitto_pub -h "$BROKER" -i pl_video_pub -r -l -t "$T_STATE" \
	--will-topic "$T_STATE" --will-retain --will-payload '{"alive":false}' 2>/dev/null; sleep 1; done ) <"$PUB_FIFO" &
PUB_PID=$!

cleanup() {
	echo '{"alive":false}' >&4
	sleep 1
	kill "$SUB_PID" "$PUB_PID" 2>/dev/null
	pkill -f "mosquitto_sub -h $BROKER -i pl_video_sub" 2>/dev/null
	pkill -f "mosquitto_pub -h $BROKER -i pl_video_pub" 2>/dev/null
	rm -f "$CMD_FIFO" "$PUB_FIFO" "$RUN/pl_video_ctl.pid"
	exit 0
}
trap cleanup TERM INT HUP

# Время платы в мс без fork: /proc/uptime «секунды.сотые».
now_ms() {
	read -r _up _rest </proc/uptime
	NOW=$(( ${_up%.*} * 1000 + 1${_up#*.}0 - 1000 ))
}

# Число после "key": или key= в строке $LINE -> $V (пусто — поля нет). Без fork.
field() {
	V=""
	case "$LINE" in
	*\"$1\"*) _t=${LINE#*\"$1\"}; _t=${_t#*:} ;;
	*$1=*) _t=${LINE#*$1=} ;;
	*) return ;;
	esac
	_t=${_t#"${_t%%[! ]*}"}
	case "$_t" in
	true*) V=1 ;;
	false*) V=0 ;;
	[0-9]*) V=${_t%%[!0-9]*} ;;
	esac
}

D_MODE=0 D_G=0
D_V=24 A_V=-1 V_ST=-1 V_MARKS=-1 V_INFO=-1  # 0x40: сброс 0x18 (цвет, метки); -1 — не читали
A_RAW=0 A_MODE=0 A_G=0
SAT=0 BLOCK=0 FIXES=0 PRESENT=0 SDR_RESET=0
RESTORES=0 LAST_RESTORE=0 LAST_OK=0
SEQ=0 LAST_PUB=0 DIRTY=1 NEXT_STAT=0
SAT_CLEARS=0 BLOCKED=0

# Аренда: код 0 — mode ≠ 0 разрешён (переменной нет или файл есть). BLOCKED — для состояния.
allowed() {
	if [ -z "$ALLOW" ] || [ -e "$ALLOW" ]; then BLOCKED=0; return 0; fi
	BLOCKED=1; return 1
}

read_ctl() {
	A_RAW=$(( $(devmem $R_CTL 32) ))
	A_MODE=$(( A_RAW & 3 )); A_G=$(( (A_RAW >> 2) & 3 ))
}
read_stat() {
	_v=$(( $(devmem $R_STAT 32) ))
	if [ $(( (_v >> 24) & 255 )) -eq 253 ] && [ $(( (_v >> 20) & 15 )) -ge 1 ]; then
		PRESENT=1; BLOCK=$(( (_v >> 20) & 15 ))
		# v3: [19:16] — флаги фиксов, [15:0] — счётчик; v1/v2: счётчик [19:0] (VIDEO_REAL8.md §2)
		if [ $BLOCK -ge 3 ]; then FIXES=$(( (_v >> 16) & 15 )); SAT=$(( _v & 65535 ))
		else FIXES=0; SAT=$(( _v & 1048575 )); fi
	else
		PRESENT=0; BLOCK=0; FIXES=0; SAT=0
	fi
}
write_ctl() {  # $1 — доп. биты (16 = sat_clear)
	devmem $R_CTL 32 $(( D_MODE | (D_G << 2) | ${1:-0} ))
	read_ctl
}
read_vctl() {
	[ $BLOCK -ge 4 ] || return
	A_V=$(( $(devmem $R_VCTL 32) & V_MASK ))
}
write_vctl() {  # $1 — доп. биты (128 = counters_clear)
	[ $BLOCK -ge 4 ] || return
	devmem $R_VCTL 32 $(( D_V | ${1:-0} ))
	read_vctl
}
read_vstat() {  # 0x44, 0x48 — только в mode 1 (overrun и метки считаются только там); 0x4C — константа
	[ $BLOCK -ge 4 ] || return
	[ $V_INFO -ge 0 ] || V_INFO=$(( $(devmem $R_VINFO 32) ))
	if [ $A_MODE -eq 1 ]; then
		V_ST=$(( $(devmem $R_VSTAT 32) )); V_MARKS=$(( $(devmem $R_VMARKS 32) ))
	else
		V_ST=-1 V_MARKS=-1  # вне видео 0x44/0x48 не читаем — прежнее значение не выдаём за текущее
	fi
}
publish() {
	SEQ=$((SEQ + 1))
	if [ $PRESENT = 1 ]; then
		_vj=""
		[ $BLOCK -ge 4 ] && _vj=",\"video\":{\"desired\":$D_V,\"actual\":$A_V,\"status\":$V_ST,\"marks\":$V_MARKS,\"info\":$V_INFO}"
		echo "{\"alive\":true,\"present\":true,\"block\":$BLOCK,\"fixes\":$FIXES,\"desired\":{\"mode\":$D_MODE,\"g\":$D_G},\"actual\":{\"mode\":$A_MODE,\"g\":$A_G,\"raw\":$A_RAW},\"sat\":$SAT,\"sdr_reset\":$SDR_RESET,\"restores\":$RESTORES,\"sat_clears\":$SAT_CLEARS,\"lease_block\":$BLOCKED,\"last_restore_ms\":$LAST_RESTORE,\"last_ok_ms\":$LAST_OK,\"period_ms\":$PERIOD_MS,\"up_ms\":$NOW,\"seq\":$SEQ$_vj}" >&4
	else
		echo "{\"alive\":true,\"present\":false,\"reason\":\"нет блока (0x3C не 0xFDvxxxxx)\",\"desired\":{\"mode\":$D_MODE,\"g\":$D_G},\"up_ms\":$NOW,\"seq\":$SEQ}" >&4
	fi
	LAST_PUB=$NOW; DIRTY=0
}

command() {
	[ $PRESENT = 1 ] || { DIRTY=1; return; }
	field mode; m=$V; field g; g=$V; field reset_sat; rs=$V
	_om=$D_MODE _og=$D_G _ov=$D_V
	if [ -n "$m" ] && [ "$m" -le 2 ]; then
		if [ "$m" -ne 0 ] && ! allowed; then
			logger -t pl_video_ctl "mode $m без аренды -> 0"; m=0
		fi
		D_MODE=$m
	fi
	if [ -n "$g" ] && [ "$g" -le 3 ]; then D_G=$g; fi
	if [ $BLOCK -ge 4 ]; then
		_v=$D_V _vc=0 _vt=0
		for _f in deemph:0 std:1 decim:2 color:3 marks:4 in_rate:8; do
			field ${_f%:*}; _b=${_f#*:}
			if [ "$V" = 0 ] || [ "$V" = 1 ]; then _v=$(( (_v & ~(1 << _b)) | (V << _b) )); _vt=1; fi
		done
		[ $BLOCK -ge 5 ] || _v=$(( _v & ~256 ))  # in_rate — только v5
		field pol; if [ -n "$V" ] && [ "$V" -le 3 ]; then _v=$(( (_v & ~96) | (V << 5) )); _vt=1; fi
		field clear; [ "$V" = 1 ] && { _vc=128; _vt=1; }
		# поля и counters_clear — двумя записями: clear в одной записи со сменой in_rate не снимает overrun
		# (флаг снова защёлкивается при переключении; замер 2026-10-25, HW_NOTES PL-5b)
		_ch=0; [ $_v -ne $D_V ] && _ch=1
		D_V=$_v
		if [ $_vt = 1 ]; then
			if [ $_vc = 128 ] && [ $_ch = 1 ]; then write_vctl 0; fi
			write_vctl $_vc
		fi
	fi
	# смена режима (mode, g, поля 0x40) — счётчик насыщений с нуля
	if [ $D_MODE -ne $_om ] || [ $D_G -ne $_og ] || [ $D_V -ne $_ov ]; then rs=1; fi
	if [ "$rs" = 1 ]; then write_ctl 16; SAT_CLEARS=$((SAT_CLEARS + 1)); else write_ctl 0; fi
	now_ms; LAST_OK=$NOW
	read_stat; read_vstat
	DIRTY=1
}

[ "$(devmem $R_ID 32)" = 0x6169616D ] || { echo "нет maia по $R_ID" >&2; PRESENT=0; }
now_ms; read_stat
[ $PRESENT = 1 ] && { read_ctl; read_vctl; [ $A_V -ge 0 ] && D_V=$A_V; read_vstat; }
NEXT=$NOW
while :; do
	now_ms
	_w=$(( NEXT - NOW ))
	if [ $_w -gt 0 ]; then
		# ожидание тика = ожидание команды (read -t дробный, без fork)
		[ $_w -gt 999 ] && _w=999
		if [ $_w -ge 100 ]; then _T=0.$_w; elif [ $_w -ge 10 ]; then _T=0.0$_w; else _T=0.00$_w; fi
		if read -r -t $_T LINE <&3; then
			command
			continue
		fi
		now_ms
	fi
	NEXT=$(( NOW + PERIOD_MS ))
	_sec=0
	if [ $NOW -ge $NEXT_STAT ]; then
		read_stat; NEXT_STAT=$(( NOW + 1000 )); _sec=1
	fi
	if [ $PRESENT = 1 ]; then
		if [ $D_MODE -ne 0 ] && ! allowed; then
			D_MODE=0; write_ctl 16; SAT_CLEARS=$((SAT_CLEARS + 1)); DIRTY=1
			logger -t pl_video_ctl "аренды нет: mode -> 0"
		fi
		read_ctl
		if [ $A_MODE -ne $D_MODE ] || [ $A_G -ne $D_G ]; then
			_sr=$(( $(devmem $R_RESET 32) & 1 ))
			[ $_sr = $SDR_RESET ] || DIRTY=1
			SDR_RESET=$_sr
			if [ $SDR_RESET = 0 ]; then
				write_vctl 0  # сброс банка обнулил и 0x40: сначала он, затем 0x38
				write_ctl 0
				if [ $A_MODE -eq $D_MODE ] && [ $A_G -eq $D_G ]; then
					RESTORES=$((RESTORES + 1)); LAST_RESTORE=$NOW; DIRTY=1
					logger -t pl_video_ctl "0x38 восстановлен (раз $RESTORES)"
				fi
			fi
		else
			[ $SDR_RESET = 1 ] && DIRTY=1
			SDR_RESET=0; LAST_OK=$NOW
		fi
		if [ $_sec = 1 ] && [ $BLOCK -ge 4 ]; then
			_av=$A_V; read_vctl; read_vstat; [ $A_V = $_av ] || DIRTY=1
			if [ $A_V -ne $D_V ] && [ $SDR_RESET = 0 ]; then
				write_vctl 0
				if [ $A_V -eq $D_V ]; then
					RESTORES=$((RESTORES + 1)); LAST_RESTORE=$NOW; DIRTY=1
					logger -t pl_video_ctl "0x40 восстановлен (раз $RESTORES)"
				fi
			fi
		fi
	fi
	if [ $DIRTY = 1 ] || [ $(( NOW - LAST_PUB )) -ge 1000 ]; then publish; fi
done
