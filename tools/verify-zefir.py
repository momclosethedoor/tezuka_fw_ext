#!/usr/bin/env python3
"""Verify the flash artifacts produced by the Nano Buildroot job."""

import hashlib
import lzma
import struct
import sys
from pathlib import Path


def aligned(value):
    return (value + 3) & ~3


def fdt(data):
    magic, length, pos, strings, _, _, _, _, _, size = struct.unpack_from('>10I', data)
    assert magic == 0xd00dfeed and length == len(data)
    end = pos + size
    path = []
    properties = {}
    while pos < end:
        token, = struct.unpack_from('>I', data, pos)
        pos += 4
        if token == 1:
            stop = data.index(b'\0', pos)
            path.append(data[pos:stop].decode())
            pos = aligned(stop + 1)
        elif token == 2:
            path.pop()
        elif token == 3:
            length, offset = struct.unpack_from('>II', data, pos)
            pos += 8
            start = strings + offset
            name = data[start:data.index(b'\0', start)].decode()
            properties['/'.join(path) or '/', name] = data[pos:pos + length]
            pos = aligned(pos + length)
        elif token == 9:
            return properties
        elif token != 4:
            raise ValueError(f'Unknown FDT token {token}')
    raise ValueError('Missing FDT end token')


def frm(path, partition_size):
    data = path.read_bytes()
    body, checksum = data[:-33], data[-33:]
    assert len(data) <= partition_size, (len(data), partition_size)
    assert checksum == hashlib.md5(body).hexdigest().encode() + b'\n', path
    print(f'{path.name}: {len(data)} bytes, SHA256 {hashlib.sha256(data).hexdigest()}')
    return body


def rootfs_files(raw):
    result = {}
    pos = 0
    while pos + 110 <= len(raw):
        assert raw[pos:pos + 6] == b'070701'
        values = [int(raw[i:i + 8], 16) for i in range(pos + 6, pos + 110, 8)]
        start = pos + 110
        name = raw[start:start + values[11] - 1].decode().removeprefix('./')
        pos = aligned(start + values[11])
        if name == 'TRAILER!!!':
            return result
        if name.startswith(('usr/libexec/zefir/', 'etc/init.d/S60')) or name in (
            'usr/bin/maia-delivery', 'usr/sbin/goofy_lease.sh', 'opt/vfat.img',
        ):
            result[name] = (values[1], raw[pos:pos + values[6]])
        pos = aligned(pos + values[6])
    raise ValueError('Missing CPIO trailer')


def verify_label(image):
    start = 512 * struct.unpack_from('<I', image, 454)[0]
    sector = struct.unpack_from('<H', image, start + 11)[0]
    reserved = struct.unpack_from('<H', image, start + 14)[0]
    fat_size = struct.unpack_from('<H', image, start + 22)[0]
    root_size = 32 * struct.unpack_from('<H', image, start + 17)[0]
    root = start + (reserved + image[start + 16] * fat_size) * sector
    expected = b'ZEFIR      '
    assert image[start + 43:start + 54] == expected
    labels = [image[i:i + 11] for i in range(root, root + root_size, 32)
              if image[i + 11] == 8 and image[i] != 0xe5]
    assert labels == [expected], labels


def main(images, agent):
    data = frm(images / 'pluto.frm', 0x1e00000)
    tree = fdt(data)
    for node in ('fpga@1', 'linux_kernel@1', 'ramdisk@1'):
        assert tree[f'/images/{node}/hash@1', 'value'] == hashlib.md5(
            tree[f'/images/{node}', 'data']).digest()
    assert tree['/', 'magic'] == b'ITB PlutoSDR (ADALM-PLUTO)\0'
    for (node, name), value in tree.items():
        if node.startswith('/configurations/config@') and name == 'fpga':
            assert value == b'fpga@1\0'
    assert hashlib.sha256(tree['/images/fpga@1', 'data']).hexdigest() == (
        '8f44b0357cfce3ba1e1056e51b3c88651b56e8822639ef675f87fcb3f33750ec')
    dt = fdt(tree['/images/fdt@1', 'data'])
    assert dt['/memory', 'reg'] == bytes.fromhex('0000000020000000')
    partitions = {node.split('@')[-1]: struct.unpack('>II', value)
                  for (node, name), value in dt.items()
                  if '/partition@qspi-' in node and name == 'reg'}
    assert partitions == {
        'qspi-fsbl-uboot': (0, 0x100000),
        'qspi-uboot-env': (0x100000, 0x20000),
        'qspi-nvmfs': (0x120000, 0xe0000),
        'qspi-linux': (0x200000, 0x1e00000),
    }

    files = rootfs_files(lzma.decompress(tree['/images/ramdisk@1', 'data']))
    for path, built in (
        ('usr/bin/maia-delivery', 'maia-httpd'),
        ('usr/libexec/zefir/obs-ffm1-iio', 'obs-ffm1-iio'),
        ('usr/sbin/goofy_lease.sh', 'goofy_lease.sh'),
    ):
        mode, file_data = files[path]
        assert mode & 0o111 and file_data == (agent / built).read_bytes(), path
    for path in ('obs_dma.ko', 'obs_ad9361.ko', 'obs-fl-ctl'):
        assert files['usr/libexec/zefir/' + path][1].startswith(b'\x7fELF')
    init = files['etc/init.d/S60maia-httpd'][1]
    prepare = files['usr/libexec/zefir/prepare-r3.sh'][1]
    assert b'bench_split544: false' in init
    assert init.index(b'sh ./prepare-r3.sh') < init.index(b'devmem 0x7c460050')
    assert prepare.index(b'devmem 0x7c460008 32 0') < prepare.index(b'devmem 0x7c460050')
    assert b'ready_dir=/tmp/pl-uart-obs-session' in init
    readiness = init.index(b'printf \'%s\\n\' OBS_READY')
    assert init.index(b'test -c /dev/obs_fl_guard') < readiness
    assert readiness < init.index(b'mv "$phase" "$ready_dir/phase"') < init.index(b'exec /usr/bin/maia-delivery')
    assert b'video_packing: true' in init
    assert b'field_video: ($host + ":18009")' in init
    verify_label(files['opt/vfat.img'][1])

    boot = frm(images / 'boot.frm', 0x100000 + 0x20000 + 1024 + 33)
    assert len(boot[:-0x20000 - 1024]) <= 0x100000
    assert boot[:-0x20000 - 1024] == (images / 'boot.dfu').read_bytes()[:-16]
    print('Verified Nano FIT, OBN5 r2, ARM runtime, USB label and QSPI layout.')


if __name__ == '__main__':
    main(Path(sys.argv[1]), Path(sys.argv[2]))
