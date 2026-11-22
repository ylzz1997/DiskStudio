#!/usr/bin/env python3
"""Make an Ext4 deletion fixture with actual Linux commits in an ordinary RAM file."""

import base64
import gzip
import hashlib
import json
import os
import pathlib
import shutil
import subprocess
import sys
import tempfile

MIB = 1024 * 1024
SECTOR = 4096
VM_UUID = '25c39913-ac45-4445-aa54-bdd0dbca7001'


def output(*args):
    return subprocess.check_output(args, text=True).strip()


def check_vm():
    assert sys.argv[1:] == ['ext-journal-image']
    assert pathlib.Path('/sys/class/dmi/id/product_uuid').read_text().strip().lower() == VM_UUID
    tree = json.loads(output('lsblk', '-J', '-b', '-o', 'NAME,TYPE,PATH,SIZE,SERIAL'))['blockdevices']
    disks = [entry for entry in tree if entry['type'] == 'disk']
    assert len(disks) == 1
    disk = disks[0]
    assert disk['serial'].strip() == 'DS-UBUNTU-OS-0001' and disk['size'] == 16 * 1024 * MIB
    assert output('findmnt', '-n', '-o', 'SOURCE', '/') in {part['path'] for part in disk['children']}
    assert output('findmnt', '-n', '-o', 'FSTYPE', '--target', '/dev/shm') == 'tmpfs'


def witness(file):
    return {
        'inode': file.stat().st_ino,
        'bytes': file.stat().st_size,
        'sha256': hashlib.sha256(file.read_bytes()).hexdigest(),
    }


def sync():
    subprocess.run(['sync'], check=True)


def fixture(temp):
    image = temp / 'ext-journal.raw'
    mount = temp / 'mount'
    mount.mkdir()
    with image.open('xb') as handle:
        handle.truncate(32 * MIB)
    subprocess.run([
        'mke2fs', '-q', '-t', 'ext4', '-F', '-b', str(SECTOR), '-I', '256',
        '-J', 'size=4', '-O', '^orphan_file,^fast_commit', '-E', 'lazy_itable_init=0,lazy_journal_init=0',
        '-L', 'DS-EXT-JOURNAL', str(image),
    ], check=True)
    loop = output('losetup', '--find', '--show', '--sector-size', str(SECTOR), str(image))
    if not loop.startswith('/dev/loop'):
        raise RuntimeError('Expected an isolated loop device for the RAM fixture')
    try:
        assert output('losetup', '--noheadings', '--output', 'BACK-FILE', loop) == str(image)
        assert int(output('blockdev', '--getsize64', loop)) == image.stat().st_size
        assert int(output('blockdev', '--getss', loop)) == SECTOR
        subprocess.run(['mount', '-t', 'ext4', '-o', 'rw,nodiscard,journal_checksum', loop, str(mount)], check=True)
        try:
            nested = mount / 'nested'
            nested.mkdir()
            kept = mount / 'before-rename.txt'
            kept.write_bytes(b'Ext4 rename control, file still allocated.\n')
            sync()
            text = mount / 'removed.txt'
            text.write_bytes('Ext4 retained journal, 删除文本。\n'.encode())
            sparse = nested / 'fragment.bin'
            with sparse.open('xb') as stream:
                stream.write(b'first mapped extent\n' * 150)
                stream.seek(3 * SECTOR + 117)
                stream.write(bytes(range(251)) * 13)
                stream.flush()
                os.fsync(stream.fileno())
            sync()
            print(output('filefrag', '-v', str(sparse)), flush=True)
            removed = {str(file.relative_to(mount)): witness(file) for file in (text, sparse)}
            sync()
            text.unlink()
            sparse.unlink()
            live = mount / 'retained.txt'
            kept.rename(live)
            sync()
            assert not text.exists() and not sparse.exists()
            retained = witness(live)
        finally:
            subprocess.run(['umount', str(mount)], check=True)
    finally:
        subprocess.run(['losetup', '--detach', loop], check=True)
    subprocess.run(['e2fsck', '-f', '-n', str(image)], check=True)
    print(output('dumpe2fs', '-h', str(image)), flush=True)
    print(output('debugfs', '-R', 'logdump -O', str(image)), flush=True)
    compressed = temp / 'ext-journal.raw.gz'
    with image.open('rb') as source, gzip.open(compressed, 'wb', compresslevel=6) as dest:
        image_hash = hashlib.file_digest(source, 'sha256').hexdigest()
        source.seek(0)
        shutil.copyfileobj(source, dest, MIB)
    archive = compressed.read_bytes()
    assert len(archive) < MIB
    print('DS_EXT_JOURNAL_MANIFEST ' + json.dumps({
        'imageBytes': image.stat().st_size, 'imageSha256': image_hash,
        'gzipBytes': len(archive), 'gzipSha256': hashlib.sha256(archive).hexdigest(),
        'removed': removed, 'retained': retained,
    }, separators=(',', ':')), flush=True)
    for index, offset in enumerate(range(0, len(archive), 576)):
        block = base64.b64encode(archive[offset:offset + 576]).decode('ascii')
        print(f'DS_EXT_JOURNAL_BLOCK {index:05d} {block}')
    print('DISKSTUDIO_EXT_JOURNAL_FIXTURE_PASS', flush=True)


check_vm()
with tempfile.TemporaryDirectory(prefix='diskstudio-ext-journal-', dir='/dev/shm') as temp:
    fixture(pathlib.Path(temp))
