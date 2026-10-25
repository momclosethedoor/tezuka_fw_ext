#!/bin/sh
# Обёртка оверлея goofy nano (PL-4a) над watchconsoletx.sh Tezuka v0.3.24 (копия без изменений —
# /usr/libexec/goofy/tezuka/watchconsoletx.sh). При включённой аренде TX (/etc/goofy/lease_enforce) не запускается:
# консольный TX / DATV не используются, а скрипт сам пишет TX_LO_powerdown = 0. Запускают S95bgcript и
# api_controller.sh (cmd/rx/loopback).
if [ -e /etc/goofy/lease_enforce ]; then
	logger -t goofy "watchconsoletx.sh: аренда TX включена — не запускается" 2>/dev/null
	exit 0
fi
exec /usr/libexec/goofy/tezuka/watchconsoletx.sh "$@"
