#!/usr/bin/env python3
"""Independent Linux acceptance only; refuse every disk except VM test A/B."""
import hashlib, json, os, pathlib, stat, subprocess, sys, traceback

GIB = 1024 ** 3
PHASE = sys.argv[1]
assert PHASE in ('populate', 'verify')
def run(*args):
    result = subprocess.run(args, check=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    output = result.stdout.decode('utf-8', errors='replace')
    print(output, end='', flush=True)
    return output
def digest(path):
    h = hashlib.sha256()
    with open(path, 'rb') as f:
        while data := f.read(1024 * 1024): h.update(data)
    return h.hexdigest()
def manifest(folder):
    result = {}
    for p in sorted(folder.rglob('*')):
        if 'lost+found' in p.parts: continue
        s = p.lstat()
        entry = {'mode': stat.S_IMODE(s.st_mode), 'uid': s.st_uid, 'gid': s.st_gid, 'type': stat.S_IFMT(s.st_mode)}
        if p.is_symlink(): entry['link'] = os.readlink(p)
        elif p.is_file():
            entry.update(size=s.st_size, sha256=digest(p), links=s.st_nlink)
            entry['xattrs'] = {name: os.getxattr(p, name).hex() for name in os.listxattr(p)}
        result[str(p.relative_to(folder))] = entry
    return result
def check():
    assert pathlib.Path('/sys/class/dmi/id/product_uuid').read_text().strip().lower() == '25c39913-ac45-4445-aa54-bdd0dbca7001'
    tree = json.loads(subprocess.check_output(['lsblk','-J','-b','-o','NAME,PATH,TYPE,SIZE,SERIAL,FSTYPE,LABEL,PARTUUID,PARTTYPE,UUID']))['blockdevices']
    disks = [d for d in tree if d['type'] == 'disk']
    assert len(disks) == 3, disks
    expected = {'DS-UBUNTU-OS-0001':16*GIB,'DS-TEST-A-0001':16*GIB,'DS-TEST-B-0001':16*GIB}
    assert {d['serial'].strip():d['size'] for d in disks} == expected
    root = subprocess.check_output(['findmnt','-n','-o','SOURCE','/'], text=True).strip()
    osdisk = next(d for d in disks if d['serial'].strip() == 'DS-UBUNTU-OS-0001')
    assert root in [p['path'] for p in osdisk.get('children', [])]
    for serial, label in [('DS-TEST-A-0001','DS-A资料'),('DS-TEST-B-0001','DS-B资料')]:
        disk = next(d for d in disks if d['serial'].strip() == serial)
        parts = [p for p in disk.get('children', []) if p.get('parttype') == '0fc63daf-8483-4772-8e79-3d69d8477de4']
        assert len(parts) == 1, parts
        p = parts[0]
        assert p['fstype'] == 'ext4' and p['label'] == label and p['size'] == (2 if PHASE == 'populate' else 3)*GIB, p
        assert p['path'] != root and p['path'].startswith('/dev/nvme')
        run('e2fsck','-f','-n',p['path'])
        folder = pathlib.Path('/mnt')/serial
        folder.mkdir(exist_ok=True)
        before = pathlib.Path('/var/lib')/(serial+'-ext4-manifest.json')
        run('mount','-t','ext4','-o','rw' if PHASE == 'populate' else 'ro,noload',p['path'],str(folder))
        try:
            if PHASE == 'populate':
                (folder/'目录').mkdir()
                (folder/'目录'/'说明.txt').write_text('DiskStudio ext4 跨系统测试\n'*4096, encoding='utf-8')
                with open(folder/'temporary-allocation.bin','wb') as f:
                    os.posix_fallocate(f.fileno(),0,1280*1024**2)
                block = bytes(range(256))*4096
                with open(folder/'large.bin','wb') as f:
                    for _ in range(160): f.write(block)
                # Exercise hard links, symlinks, sparse holes, Unicode, xattrs and modes.
                os.link(folder/'large.bin',folder/'hardlink.bin')
                os.symlink('目录/说明.txt',folder/'符号链接')
                with open(folder/'sparse.bin','wb') as f:
                    f.write(b'first-block'); f.seek(192*1024**2); f.write(b'last-block')
                os.chmod(folder/'large.bin',0o640)
                os.setxattr(folder/'large.bin','user.diskstudio','中文属性'.encode())
                os.chown(folder/'目录'/'说明.txt',1234,2345)
                # Force noncontiguous allocation, then free gaps before Windows shrink.
                for i in range(16):
                    with open(folder/f'fragment-{i}.bin','wb') as f:
                        for _ in range(4): f.write(hashlib.sha256(str(i).encode()).digest()*32768)
                for i in range(0,16,2): (folder/f'fragment-{i}.bin').unlink()
                (folder/'temporary-allocation.bin').unlink()
                run('sync')
                run('filefrag','-v',str(folder/'large.bin'))
                saved = {'uuid':p['uuid'],'partuuid':p['partuuid'],'files':manifest(folder)}
                before.write_text(json.dumps(saved,ensure_ascii=False,sort_keys=True),encoding='utf-8')
            else:
                saved = json.loads(before.read_text(encoding='utf-8'))
                assert saved['uuid'] == p['uuid'] and saved['partuuid'] == p['partuuid']
                assert saved['files'] == manifest(folder), 'File contents or metadata differ'
                assert (folder/'large.bin').stat().st_ino == (folder/'hardlink.bin').stat().st_ino
                assert (folder/'sparse.bin').stat().st_blocks*512 < 1024**2, 'Sparse holes lost'
            print('DISKSTUDIO_EXT4_'+PHASE.upper()+'_PASS '+serial, flush=True)
        finally:
            run('umount',str(folder))
        run('e2fsck','-f','-n',p['path'])
    print('DISKSTUDIO_EXT4_ALL_'+PHASE.upper()+'_PASS',flush=True)
try: check()
except BaseException:
    print('DISKSTUDIO_EXT4_FAILED',flush=True)
    traceback.print_exc()
    sys.exit(1)
