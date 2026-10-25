#!/bin/sh
# Аренда TX (оверлей goofy nano, срез PL-4a; контракт — E:\pl\docs\LEASE.md). busybox sh + mosquitto_sub/pub.
#
# Хост раз в 1 с публикует goofy/lease/hb (QoS 0, БЕЗ retain): {"host":"<id>","seq":<n>}; {"seq":<n>,"release":true}
#   — снять аренду сразу. Сообщение без числового seq не считается. Retained-сообщения отбрасываются (-R).
# Аренда жива, пока последнее сердцебиение не старше LEASE_MS (5000 мс по часам платы).
# Включено, пока есть файл-флаг /etc/goofy/lease_enforce (в оверлее есть; rm — до перезагрузки выкл.).
# Аренды нет и флаг есть — раз в секунду:
#   TX: обратное чтение TX_LO_powerdown и out_voltage{0,1}_hardwaregain; не погашен -> погасить
#       (powerdown = 1, ослабление максимальное) и прочитать снова (goofy_tx_off из /usr/lib/goofy/tx.sh);
#   видео: файла $RUN/lease_ok нет -> сторож pl_video_ctl.sh (PL_VIDEO_ALLOW) держит 0x38 mode 0; если сторож
#       не запущен — mode 0 в 0x7C460038 пишем сами (g сохраняется, sat_clear).
# Аренда есть или флага нет: $RUN/lease_ok есть, TX не трогаем (только читаем для состояния).
# Состояние: retained state/goofy/lease раз в секунду и при смене; Last Will {"alive":false}.
# Записи: TX-атрибуты ad9361-phy; 0x7C460038 — только без сторожа видео.

. /usr/lib/goofy/tx.sh

RUN=${GOOFY_RUN:-/run/goofy}
BROKER=${GOOFY_BROKER:-127.0.0.1}
FLAG=${GOOFY_LEASE_FLAG:-/etc/goofy/lease_enforce}
LEASE_MS=5000
T_HB=goofy/lease/hb
T_STATE=state/goofy/lease
OK_FILE=$RUN/lease_ok
V_PID=/run/pl_video/pl_video_ctl.pid
R_ID=0x7C460000
R_CTL=0x7C460038
R_STAT=0x7C46003C

mkdir -p "$RUN" || exit 1
rm -f "$OK_FILE"  # до первого решения аренды нет (отказ в безопасную сторону)
echo $$ >"$RUN/goofy_lease.pid"
HB_FIFO=$RUN/hb.fifo
PUB_FIFO=$RUN/pub.fifo
rm -f "$HB_FIFO" "$PUB_FIFO"
mkfifo "$HB_FIFO" "$PUB_FIFO" || exit 1
exec 3<>"$HB_FIFO" 4<>"$PUB_FIFO"
trap '' PIPE

( exec 3<&- 4<&-; while :; do mosquitto_sub -h "$BROKER" -i goofy_lease_sub -R -t "$T_HB" 2>/dev/null; sleep 1; done ) >"$HB_FIFO" &
SUB_PID=$!
( exec 3<&- 4<&-; while :; do mosquitto_pub -h "$BROKER" -i goofy_lease_pub -r -l -t "$T_STATE" \
	--will-topic "$T_STATE" --will-retain --will-payload '{"alive":false}' 2>/dev/null; sleep 1; done ) <"$PUB_FIFO" &
PUB_PID=$!

cleanup() {
	rm -f "$OK_FILE"
	echo '{"alive":false}' >&4
	sleep 1
	kill "$SUB_PID" "$PUB_PID" 2>/dev/null
	pkill -f "mosquitto_sub -h $BROKER -i goofy_lease_sub" 2>/dev/null
	pkill -f "mosquitto_pub -h $BROKER -i goofy_lease_pub" 2>/dev/null
	rm -f "$HB_FIFO" "$PUB_FIFO" "$RUN/goofy_lease.pid"
	exit 0
}
trap cleanup TERM INT HUP

now_ms() {
	read -r _up _rest </proc/uptime
	NOW=$(( ${_up%.*} * 1000 + 1${_up#*.}0 - 1000 ))
}

# Число после "key": в $LINE -> $V (true/false -> 1/0; пусто — поля нет). Без fork.
field() {
	V=""
	case "$LINE" in *\"$1\"*) ;; *) return ;; esac
	_t=${LINE#*\"$1\"}; _t=${_t#*:}; _t=${_t#"${_t%%[! ]*}"}
	case "$_t" in
	true*) V=1 ;;
	false*) V=0 ;;
	[0-9]*) V=${_t%%[!0-9]*} ;;
	esac
}
# Строка после "key": в $LINE -> $S (только [A-Za-z0-9._-], до 32 символов, иначе "invalid").
sfield() {
	S=""
	case "$LINE" in *\"$1\"*) ;; *) return ;; esac
	_t=${LINE#*\"$1\"}; _t=${_t#*:}; _t=${_t#*\"}; S=${_t%%\"*}
	case "$S" in *[!A-Za-z0-9._-]*) S=invalid ;; esac
	[ ${#S} -le 32 ] || S=invalid
}

HB_MS=0 HOST="" SEQ=-1 HB_N=0 RELEASED=0
LEASE=0 EN=1 LOST_N=0 FORCED=0 FAILS=0 PSEQ=0 DIRTY=1 LAST_PUB=0
TX_STATE=unknown TX_PD="?" TX_G0="?" TX_G1="-"
PHY="" MIN_MDB=""

MAIA=0
[ "$(devmem $R_ID 32)" = 0x6169616D ] && [ $(( ($(devmem $R_STAT 32) >> 24) & 255 )) -eq 253 ] && MAIA=1

heartbeat() {
	field seq; [ -n "$V" ] || return
	_s=$V
	sfield host; HOST=$S
	SEQ=$_s; HB_N=$((HB_N + 1))
	field release
	if [ "$V" = 1 ]; then RELEASED=1; HB_MS=0; DIRTY=1
	else RELEASED=0; HB_MS=$NOW; fi
}

read_tx() {
	if [ -z "$PHY" ]; then goofy_find_iio || { TX_STATE=no_phy; return 1; }; goofy_tx_min_gain; fi
	goofy_tx_read
	if goofy_tx_is_off; then TX_STATE=off; else TX_STATE=on; fi
}

enforce_tx() {
	read_tx || return
	[ $TX_STATE = off ] && return
	_was="pd=$TX_PD g0=$TX_G0 g1=$TX_G1"
	if goofy_tx_off; then
		FORCED=$((FORCED + 1)); TX_STATE=off
		logger -t goofy_lease "TX без аренды ($_was) -> погашен (раз $FORCED)"
	else
		FAILS=$((FAILS + 1)); TX_STATE=fail
		logger -p user.err -t goofy_lease "TX без аренды ($_was) НЕ погашен: pd=$TX_PD g0=$TX_G0 g1=$TX_G1"
	fi
	DIRTY=1
}

enforce_video() {
	[ $MAIA = 1 ] || return
	_p=""
	read -r _p 2>/dev/null <"$V_PID"
	[ -n "$_p" ] && kill -0 "$_p" 2>/dev/null && return  # сторож видео сам держит mode 0 без lease_ok
	_c=$(( $(devmem $R_CTL 32) ))
	if [ $(( _c & 3 )) -ne 0 ]; then
		devmem $R_CTL 32 $(( (_c & 12) | 16 ))
		logger -t goofy_lease "сторож видео не запущен: 0x38 mode $(( _c & 3 )) -> 0"
	fi
}

tick() {
	if [ -e "$FLAG" ]; then EN=1; else EN=0; fi
	_l=0
	[ $HB_MS -gt 0 ] && [ $(( NOW - HB_MS )) -le $LEASE_MS ] && _l=1
	[ $_l = $LEASE ] || DIRTY=1
	LEASE=$_l
	if [ $EN = 0 ] || [ $LEASE = 1 ]; then
		if [ ! -e "$OK_FILE" ]; then
			: >"$OK_FILE"
			if [ $EN = 0 ]; then logger -t goofy_lease "аренда не требуется (нет $FLAG)"
			else logger -t goofy_lease "аренда есть (host $HOST): TX и видео разрешены"; fi
			DIRTY=1
		fi
		read_tx
	else
		if [ -e "$OK_FILE" ]; then
			rm -f "$OK_FILE"; LOST_N=$((LOST_N + 1))
			logger -t goofy_lease "аренды нет: TX гасится, видео mode 0 (раз $LOST_N)"
			DIRTY=1
		fi
		enforce_tx
		enforce_video
	fi
}

publish() {
	PSEQ=$((PSEQ + 1))
	_age=-1; [ $HB_MS -gt 0 ] && _age=$(( NOW - HB_MS ))
	echo "{\"alive\":true,\"enforce\":$EN,\"lease\":$LEASE,\"host\":\"$HOST\",\"seq\":$SEQ,\"age_ms\":$_age,\"lease_ms\":$LEASE_MS,\"released\":$RELEASED,\"heartbeats\":$HB_N,\"lost\":$LOST_N,\"tx\":\"$TX_STATE\",\"tx_lo_powerdown\":\"$TX_PD\",\"tx_gain0\":\"$TX_G0\",\"tx_gain1\":\"$TX_G1\",\"forced\":$FORCED,\"fails\":$FAILS,\"up_ms\":$NOW,\"pseq\":$PSEQ}" >&4
	LAST_PUB=$NOW; DIRTY=0
}

now_ms
NEXT=$NOW
while :; do
	now_ms
	_w=$(( NEXT - NOW ))
	if [ $_w -gt 0 ]; then
		[ $_w -gt 999 ] && _w=999
		if [ $_w -ge 100 ]; then _T=0.$_w; elif [ $_w -ge 10 ]; then _T=0.0$_w; else _T=0.00$_w; fi
		if read -r -t $_T LINE <&3; then
			now_ms; heartbeat
			continue
		fi
		now_ms
	fi
	NEXT=$(( NOW + 1000 ))
	tick
	if [ $DIRTY = 1 ] || [ $(( NOW - LAST_PUB )) -ge 1000 ]; then publish; fi
done
