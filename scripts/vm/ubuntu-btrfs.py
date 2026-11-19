#!/usr/bin/env python3
"""Verify Btrfs fixtures or generate ordinary file images in tmpfs."""

import base64
import gzip
import hashlib
import json
import os
import pathlib
import shutil
import struct
import subprocess
import sys
import tempfile

GIB = 1024 ** 3
MIB = 1024 ** 2
CHECKSUM_FILES = {
    'checksum-inline.txt': b'DiskStudio Btrfs checksum fixture\n',
    'nested/checksum-compressed.bin': bytes(range(251)) * 993 + b'checksum tail\n',
}


def output(*args):
    return subprocess.check_output(args, text=True).strip()


def checked_disks(with_test_disks=True):
    uuid = pathlib.Path('/sys/class/dmi/id/product_uuid').read_text().strip().lower()
    if uuid != '25c39913-ac45-4445-aa54-bdd0dbca7001':
        raise RuntimeError('Unexpected Ubuntu VM UUID')
    tree = json.loads(output(
        'lsblk', '-J', '-b', '-o',
        'NAME,PATH,TYPE,SIZE,SERIAL,LOG-SEC,PHY-SEC,FSTYPE,LABEL,PARTTYPE,PARTUUID,UUID',
    ))['blockdevices']
    disks = {}
    for item in tree:
        if item['type'] == 'disk':
            serial = item['serial'].strip()
            if serial in disks:
                raise RuntimeError('Duplicate VM disk serial')
            disks[serial] = item
    test_serials = ('DS-TEST-A-0001', 'DS-TEST-B-0001') if with_test_disks else ()
    if set(disks) != {'DS-UBUNTU-OS-0001', *test_serials}:
        raise RuntimeError('Unexpected Ubuntu disk inventory')
    if any(item['size'] != 16 * GIB for item in disks.values()):
        raise RuntimeError('Unexpected Ubuntu disk capacity')
    for serial in test_serials:
        disk = disks[serial]
        if (disk['log-sec'], disk['phy-sec']) != (4096, 4096):
            raise RuntimeError('Test disks must be the isolated 4Kn pair')
    root = output('findmnt', '-n', '-o', 'SOURCE', '/')
    os_parts = disks['DS-UBUNTU-OS-0001'].get('children', [])
    if root not in {part['path'] for part in os_parts}:
        raise RuntimeError('Ubuntu root is not on its own disk')
    return disks


def check(phase):
    disks = checked_disks()
    label, mib = {
        'VERIFY': ('DSBTRFS4KN', 768),
        'MOVE_VERIFY': ('DSBTRFSMOVE', 192),
        'SHA256_VERIFY': ('DSCSUM-sha256', 192),
    }[phase]
    matches = [part for part in disks['DS-TEST-A-0001'].get('children', [])
               if part.get('label') == label]
    if len(matches) != 1:
        raise RuntimeError('Expected one matching Btrfs fixture on A')
    fixture = matches[0]
    size = mib * MIB
    if fixture['size'] != size or fixture['fstype'] != 'btrfs' or (
        fixture['parttype'] != '0fc63daf-8483-4772-8e79-3d69d8477de4'
        or not fixture['uuid'] or not fixture['partuuid']
        or not fixture['path'].startswith('/dev/nvme')
    ):
        raise RuntimeError('Unexpected Btrfs fixture type or size')
    subprocess.run(['btrfs', 'check', '--readonly', '--check-data-csum', fixture['path']], check=True)
    folder = pathlib.Path('/mnt/diskstudio-btrfs')
    folder.mkdir(exist_ok=True)
    subprocess.run(['mount', '-t', 'btrfs', '-o', 'ro,nologreplay', fixture['path'], str(folder)], check=True)
    try:
        if phase == 'MOVE_VERIFY':
            files = sorted(folder.iterdir(), key=lambda item: os.fsencode(item.name))
            if len(files) != 51 or any(item.is_symlink() or not item.is_file() for item in files):
                raise RuntimeError('Expected 51 regular files after relocating stripes')
            total = 0
            digest = hashlib.sha256()
            for item in files:
                name = os.fsencode(item.name)
                data = item.read_bytes()
                digest.update(len(name).to_bytes(8, 'little'))
                digest.update(name)
                digest.update(len(data).to_bytes(8, 'little'))
                digest.update(data)
                total += len(data)
            if total != 27_430_912:
                raise RuntimeError('Unexpected content length after relocating stripes')
            print(f'Btrfs relocated fixture content SHA-256 {digest.hexdigest()}', flush=True)
        elif phase == 'SHA256_VERIFY':
            info = output('btrfs', 'inspect-internal', 'dump-super', fixture['path'])
            if not any(line.split()[:2] == ['csum_type', '2'] for line in info.splitlines()):
                raise RuntimeError('Expected a SHA-256 Btrfs superblock')
            if sorted(str(path.relative_to(folder)) for path in folder.rglob('*')) != sorted([*CHECKSUM_FILES, 'nested']):
                raise RuntimeError('Btrfs shrink changed the file tree')
            for name, expected in CHECKSUM_FILES.items():
                data = (folder / name).read_bytes()
                if data != expected:
                    raise RuntimeError(f'Btrfs shrink changed {name}')
                print(f'{name} {len(data)} {hashlib.sha256(data).hexdigest()}', flush=True)
        elif any(folder.iterdir()):
            raise RuntimeError('Expected an empty Btrfs UI fixture')
    finally:
        subprocess.run(['umount', str(folder)], check=True)
    print(f'DISKSTUDIO_BTRFS_4KN_{phase}_PASS', flush=True)


def fixture_directory():
    checked_disks(with_test_disks=False)
    if output('findmnt', '-n', '-o', 'FSTYPE', '--target', '/dev/shm') != 'tmpfs':
        raise RuntimeError('Btrfs fixture requires a RAM-backed ordinary file')
    return tempfile.TemporaryDirectory(prefix='diskstudio-btrfs-', dir='/dev/shm')


def archive_fixture(image, marker, fields, files):
    archive = image.with_suffix('.raw.gz')
    with image.open('rb') as source, gzip.open(archive, 'wb', compresslevel=6) as compressed:
        digest = hashlib.file_digest(source, 'sha256').hexdigest()
        source.seek(0)
        shutil.copyfileobj(source, compressed, 1024 * 1024)
    evidence = {
        **fields,
        'imageSha256': digest,
        'gzipSha256': hashlib.sha256(archive.read_bytes()).hexdigest(),
        'gzipBytes': archive.stat().st_size,
        'files': {
            name: {'bytes': len(data), 'sha256': hashlib.sha256(data).hexdigest()}
            for name, data in files.items()
        },
    }
    prefix = f'DS_BTRFS_{marker}'
    print(prefix + '_MANIFEST ' + json.dumps(evidence, separators=(',', ':')), flush=True)
    with archive.open('rb') as chunks:
        for index, data in enumerate(iter(lambda: chunks.read(576), b'')):
            print(f'{prefix}_BLOCK {index:05d} {base64.b64encode(data).decode("ascii")}')
    print(f'DISKSTUDIO_BTRFS_{marker}_FIXTURE_PASS', flush=True)


def history_fixture(retained):
    with fixture_directory() as temp:
        directory = pathlib.Path(temp)
        kind = 'retained' if retained else 'snapshot'
        image = directory / f'{kind}.raw'
        with image.open('xb') as stream:
            stream.truncate(256 * MIB)
        subprocess.run([
            'mkfs.btrfs', '--nodiscard', '--checksum', 'crc32c',
            '-L', 'DSBTRFSOLD' if retained else 'DSBTRFSSNAP',
            '-d', 'single', '-m', 'dup', '-s', '4096', '-n', '16384',
            str(image),
        ], check=True)
        mount = directory / 'mount'
        mount.mkdir()
        subprocess.run([
            'mount', '-t', 'btrfs', '-o', 'loop,noatime,nodiscard,compress-force=zstd:3',
            str(image), str(mount),
        ], check=True)
        prefix = 'retained' if retained else 'deleted'
        expected = {
            f'{prefix}-inline.txt': (
                b'DiskStudio Btrfs retained transaction fixture\n' if retained
                else b'DiskStudio Btrfs read-only snapshot fixture\n'
            ),
            f'nested/{prefix}-compressed.bin': (
                bytes(range(251)) * 993 + b'DiskStudio Btrfs compressed tail\n'
            ),
        }
        old_tree = {}
        try:
            src = mount / 'source'
            snapshot = mount / 'frozen'
            subprocess.run(['btrfs', 'subvolume', 'create', str(src)], check=True)
            (src / 'nested').mkdir()
            for name, data in expected.items():
                (src / name).write_bytes(data)
            (src / 'old-name.txt').write_bytes(b'Renamed in the live tree\n')
            (src / 'old-link.txt').write_bytes(b'One live hard link remains\n')
            os.link(src / 'old-link.txt', src / 'live-link.txt')
            (src / 'unchanged.txt').write_bytes(b'Present in both views\n')
            subprocess.run(['btrfs', 'filesystem', 'sync', str(mount)], check=True)
            if retained:
                subvolume = int(output('btrfs', 'inspect-internal', 'rootid', str(src)))
                with image.open('rb') as stream:
                    stream.seek(65536 + 0x48)
                    generation, bytenr = struct.unpack('<QQ', stream.read(16))
                if subvolume < 256 or generation == 0 or bytenr == 0:
                    raise RuntimeError('Missing committed pre-deletion root')
                old_tree = {'subvolume': subvolume, 'generation': generation, 'bytenr': bytenr}
            else:
                subprocess.run(['btrfs', 'subvolume', 'snapshot', '-r', str(src), str(snapshot)], check=True)
            for name in [*expected, 'old-link.txt']:
                (src / name).unlink()
            (src / 'old-name.txt').rename(src / 'new-name.txt')
            if retained:
                if output('btrfs', 'subvolume', 'list', '-s', str(mount)):
                    raise RuntimeError('Retained transaction fixture must have no snapshots')
            else:
                subprocess.run(['btrfs', 'filesystem', 'sync', str(mount)], check=True)
                if output('btrfs', 'property', 'get', '-ts', str(snapshot), 'ro') != 'ro=true':
                    raise RuntimeError('Expected a native read-only snapshot')
                if not all((snapshot / name).read_bytes() == data for name, data in expected.items()):
                    raise RuntimeError('Snapshot file bytes differ')
            if any((src / name).exists() for name in expected):
                raise RuntimeError('Deleted file is still present in the live subvolume')
            if not all((src / name).is_file() for name in ('new-name.txt', 'live-link.txt', 'unchanged.txt')):
                raise RuntimeError('Missing live rename and hard-link controls')
        finally:
            subprocess.run(['umount', str(mount)], check=True)
        subprocess.run(['btrfs', 'check', '--readonly', '--check-data-csum', str(image)], check=True)
        if old_tree:
            prior = directory / 'prior'
            prior.mkdir()
            subprocess.run([
                'btrfs', 'restore', '-t', str(old_tree['bytenr']), '-r', str(old_tree['subvolume']),
                str(image), str(prior),
            ], check=True)
            if not all((prior / name).read_bytes() == data for name, data in expected.items()):
                raise RuntimeError('Linux restore from the retained root returned unexpected bytes')
        archive_fixture(image, kind.upper(), {'kind': kind, 'oldTree': old_tree}, expected)


def checksum_fixtures():
    with fixture_directory() as temp:
        directory = pathlib.Path(temp)
        mount = directory / 'mount'
        mount.mkdir()
        for algorithm in ('xxhash', 'sha256', 'blake2'):
            image = directory / f'{algorithm}.raw'
            with image.open('xb') as stream:
                stream.truncate(256 * MIB)
            subprocess.run([
                'mkfs.btrfs', '--nodiscard', '--checksum', algorithm,
                '-L', f'DSCSUM-{algorithm}', '-d', 'single', '-m', 'dup',
                '-s', '4096', '-n', '16384', str(image),
            ], check=True)
            subprocess.run([
                'mount', '-t', 'btrfs', '-o', 'loop,noatime,nodiscard,compress-force=zstd:3',
                str(image), str(mount),
            ], check=True)
            try:
                (mount / 'nested').mkdir()
                for name, data in CHECKSUM_FILES.items():
                    (mount / name).write_bytes(data)
            finally:
                subprocess.run(['umount', str(mount)], check=True)
            subprocess.run(['btrfs', 'check', '--readonly', '--check-data-csum', str(image)], check=True)
            subprocess.run([
                'mount', '-t', 'btrfs', '-o', 'loop,ro,nologreplay', str(image), str(mount),
            ], check=True)
            try:
                if any((mount / name).read_bytes() != data for name, data in CHECKSUM_FILES.items()):
                    raise RuntimeError(f'Linux {algorithm} fixture contents differ')
            finally:
                subprocess.run(['umount', str(mount)], check=True)
            archive_fixture(image, f'CHECKSUM_{algorithm.upper()}', {'algorithm': algorithm}, CHECKSUM_FILES)
            image.with_suffix('.raw.gz').unlink()
            image.unlink()


def allocation_fixture():
    with fixture_directory() as temp:
        directory = pathlib.Path(temp)
        image = directory / 'allocation.raw'
        mount = directory / 'mount'
        mount.mkdir()
        with image.open('xb') as stream:
            stream.truncate(256 * MIB)
        data = bytes(range(251)) * 1919 + b'DiskStudio Btrfs allocated file\n'
        marker = bytes(range(64)) * 64
        expected = {
            'live/nested/contents.bin': data,
            'live/nested/reflink.bin': data,
            'live/sparse.bin': bytes(256 * 1024) + marker,
            'live/preallocated.bin': bytes(2 * MIB),
            'snapshot/nested/deleted.bin': data[::-1],
        }
        subprocess.run([
            'mkfs.btrfs', '--nodiscard', '--checksum', 'sha256', '-O', 'block-group-tree',
            '-L', 'DSBTRFS-ALLOCATION', '-d', 'single', '-m', 'dup',
            '-s', '4096', '-n', '16384', str(image),
        ], check=True)
        subprocess.run([
            'mount', '-t', 'btrfs', '-o', 'loop,noatime,nodiscard', str(image), str(mount),
        ], check=True)
        try:
            live = mount / 'live'
            subprocess.run(['btrfs', 'subvolume', 'create', str(live)], check=True)
            (live / 'nested').mkdir()
            (live / 'nested/contents.bin').write_bytes(data)
            (live / 'nested/deleted.bin').write_bytes(data[::-1])
            subprocess.run([
                'cp', '--reflink=always', str(live / 'nested/contents.bin'),
                str(live / 'nested/reflink.bin'),
            ], check=True)
            with (live / 'sparse.bin').open('xb') as stream:
                stream.seek(256 * 1024)
                stream.write(marker)
            subprocess.run(['fallocate', '-l', '2M', str(live / 'preallocated.bin')], check=True)
            subprocess.run([
                'btrfs', 'subvolume', 'snapshot', '-r', str(live), str(mount / 'snapshot'),
            ], check=True)
            (live / 'nested/deleted.bin').unlink()
        finally:
            subprocess.run(['umount', str(mount)], check=True)
        subprocess.run(['btrfs', 'check', '--readonly', '--check-data-csum', str(image)], check=True)
        subprocess.run([
            'mount', '-t', 'btrfs', '-o', 'loop,ro,nologreplay', str(image), str(mount),
        ], check=True)
        try:
            if (mount / 'live/nested/deleted.bin').exists() or any(
                (mount / name).read_bytes() != data for name, data in expected.items()
            ):
                raise RuntimeError('Linux Btrfs allocation fixture contents differ')
        finally:
            subprocess.run(['umount', str(mount)], check=True)
        archive_fixture(image, 'ALLOCATION', {'kind': 'allocation'}, expected)


def member_fixtures(data_profile='raid0', metadata_profile='raid1', count=2):
    parity = data_profile in ('raid5', 'raid6')
    with fixture_directory() as temp:
        directory = pathlib.Path(temp)
        images = [directory / f'member-{index}.raw' for index in range(1, count + 1)]
        devices = []
        mount = directory / 'mount'
        mount.mkdir()
        deleted = b'DiskStudio deleted from a multi-device Btrfs subvolume\n'
        end = 23 if parity else 11
        live = b''.join(bytes([index]) * (64 * 1024) for index in range(1, end))
        expected = {'live/nested/striped.bin': live, 'snapshot/deleted.txt': deleted}
        try:
            for image in images:
                with image.open('xb') as stream:
                    stream.truncate(256 * MIB)
                loop = output('losetup', '--find', '--show', str(image))
                if not loop.startswith('/dev/loop') or not loop[9:].isdigit():
                    raise RuntimeError('Expected an isolated loop fixture')
                devices.append(loop)
                status = json.loads(output('losetup', '--list', '--json', '-O', 'NAME,BACK-FILE', loop))['loopdevices']
                if [(item['name'], item['back-file']) for item in status] != [(loop, str(image))]:
                    raise RuntimeError('Unexpected loop backing file')
            subprocess.run([
                'mkfs.btrfs', '--nodiscard', '--checksum', 'sha256', '-L', f'DSBTRFS-{data_profile.upper()}',
                '-d', data_profile, '-m', metadata_profile, '-s', '4096', '-n', '16384', *devices,
            ], check=True)
            subprocess.run(['btrfs', 'device', 'scan', *devices], check=True)
            member_options = ','.join(f'device={device}' for device in devices[1:])
            subprocess.run([
                'mount', '-t', 'btrfs', '-o', f'{member_options},noatime,nodiscard',
                devices[0], str(mount),
            ], check=True)
            try:
                source = mount / 'live'
                subprocess.run(['btrfs', 'subvolume', 'create', str(source)], check=True)
                (source / 'nested').mkdir()
                (source / 'nested/striped.bin').write_bytes(live)
                (source / 'deleted.txt').write_bytes(deleted)
                subprocess.run(['btrfs', 'subvolume', 'snapshot', '-r', str(source), str(mount / 'snapshot')], check=True)
                (source / 'deleted.txt').unlink()
            finally:
                subprocess.run(['umount', str(mount)], check=True)
            subprocess.run(['btrfs', 'check', '--readonly', '--check-data-csum', devices[0]], check=True)
            subprocess.run([
                'mount', '-t', 'btrfs', '-o', f'{member_options},ro,nologreplay',
                devices[0], str(mount),
            ], check=True)
            try:
                if (mount / 'live/deleted.txt').exists() or any(
                    (mount / name).read_bytes() != data for name, data in expected.items()
                ):
                    raise RuntimeError('Linux multi-device file readback differs')
            finally:
                subprocess.run(['umount', str(mount)], check=True)
        finally:
            for loop in reversed(devices):
                subprocess.run(['losetup', '--detach', loop], check=True)
        for index, image in enumerate(images, 1):
            fields = {'profile': data_profile, 'member': index} if parity else {'member': index}
            marker = f'{data_profile.upper()}_{index}' if parity else f'MEMBER_{index}'
            archive_fixture(image, marker, fields, expected)


if sys.argv[1:] in (['btrfs-snapshot-image'], ['btrfs-retained-image']):
    history_fixture(sys.argv[1] == 'btrfs-retained-image')
elif sys.argv[1:] == ['btrfs-checksum-images']:
    checksum_fixtures()
elif sys.argv[1:] == ['btrfs-allocation-image']:
    allocation_fixture()
elif sys.argv[1:] == ['btrfs-members-images']:
    member_fixtures()
elif sys.argv[1:] == ['btrfs-parity-images']:
    member_fixtures('raid5', 'raid5', 2)
    member_fixtures('raid6', 'raid6', 4)
elif sys.argv[1:] in (['btrfs-verify'], ['btrfs-move-verify'], ['btrfs-sha256-verify']):
    check(sys.argv[1].removeprefix('btrfs-').upper().replace('-', '_'))
else:
    raise RuntimeError('Unexpected Ubuntu Btrfs verification phase')
