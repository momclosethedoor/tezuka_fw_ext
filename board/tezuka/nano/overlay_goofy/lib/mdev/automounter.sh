#!/bin/sh
# Копия board/tezuka/common/overlay_base/lib/mdev/automounter.sh (v0.3.24) для оверлея goofy nano (PL-4a):
# единственное отличие — скрипты runme* с USB-носителя не выполняются (только строка в syslog).

destdir=/media

my_umount()
{
	if grep -qs "^/dev/$1 " /proc/mounts ; then
		umount "${destdir}/$1";
		echo heartbeat > /sys/class/leds/led0:green/trigger
	fi

	[ -d "${destdir}/$1" ] && rmdir "${destdir}/$1"
}

do_mount()
{
	local errno
	local err

	errno=0
	for I in $(seq 5)
	do
		err=$(mount -t auto -o sync "/dev/$1" "${destdir}/$1" 2>&1)
		errno=$?

		# If we get a "Device or resource busy" error, retry again in a
		# little bit, otherwise just return immediately.
		if ! echo "${err}" | grep -q "Device or resource busy"
		then
			return ${errno}
		fi

		sleep .25
	done

	echo "${err}" >&2
	return ${errno}
}

my_mount()
{
	mkdir -p "${destdir}/$1" || exit 1

	if ! do_mount $1; then
		# failed to mount, clean up mountpoint
		rmdir "${destdir}/$1"
		exit 1
	fi

	echo default-on > /sys/class/leds/led0:green/trigger

	# goofy (PL-4a): runme* с носителя НЕ исполняются (было: . runme*.sh / runme* start)
	for i in ${destdir}/$1/runme??* ;do
		[ -f "$i" ] && logger -t goofy "automounter: $i есть, НЕ выполняется"
	done
}

case "${ACTION}" in
add|"")
	my_umount ${MDEV}
	my_mount ${MDEV}
	;;
remove)
	my_umount ${MDEV}
	;;
remove_all)
	for i in ${destdir}/??*
	do
		my_umount $(basename $i)
	done
esac
