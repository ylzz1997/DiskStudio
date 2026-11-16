#!/usr/bin/env python3
"""Independent Ext2/3 acceptance; exact VM UUID + serial/capacity gates first."""
import hashlib, json, os, pathlib, stat, subprocess, sys, traceback

GIB = 1024**3
PHASE = sys.argv[1]
assert PHASE in ('ext-populate', 'ext-verify')
POPULATE = PHASE == 'ext-populate'

def run(*args):
    result = subprocess.run(args, check=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    output = result.stdout.decode('utf-8', errors='replace')
    print(output, end='', flush=True)
    return output

def digest(path):
    h = hashlib.sha256()
    with open(path, 'rb') as f:
        while data := f.read(1024*1024): h.update(data)
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
    assert len(disks) == 3
    assert {d['serial'].strip():d['size'] for d in disks} == {'DS-UBUNTU-OS-0001':16*GIB,'DS-TEST-A-0001':16*GIB,'DS-TEST-B-0001':16*GIB}
    root = subprocess.check_output(['findmnt','-n','-o','SOURCE','/'], text=True).strip()
    osdisk = next(d for d in disks if d['serial'].strip() == 'DS-UBUNTU-OS-0001')
    assert root in [p['path'] for p in osdisk.get('children', [])]
    for letter in ('A', 'B'):
        disk = next(d for d in disks if d['serial'].strip() == 'DS-TEST-'+letter+'-0001')
        for fs, block_size in [('ext2',1024 if letter == 'A' else 4096),('ext3',2048 if letter == 'A' else 4096)]:
            label = 'DS'+letter+'-'+fs
            matches = [p for p in disk.get('children', []) if p.get('label') == label]
            assert len(matches) == 1, matches
            p = matches[0]
            assert p['fstype'] == fs and p['parttype'] == '0fc63daf-8483-4772-8e79-3d69d8477de4'
            assert p['size'] == (GIB if POPULATE else 3*GIB//2)
            assert p['path'].startswith('/dev/nvme') and p['path'] != root
            headers = run('dumpe2fs','-h',p['path'])
            reported_block = next(line.split(':',1)[1].strip() for line in headers.splitlines() if line.startswith('Block size:'))
            assert int(reported_block) == block_size
            features = next(line.split(':',1)[1].split() for line in headers.splitlines() if line.startswith('Filesystem features:'))
            assert ('has_journal' in features) == (fs == 'ext3') and 'extent' not in features and '64bit' not in features
            run('e2fsck','-f','-n',p['path'])
            folder = pathlib.Path('/mnt')/label
            folder.mkdir(exist_ok=True)
            baseline = pathlib.Path('/var/lib')/(label+'-manifest.json')
            if POPULATE: assert not baseline.exists(), 'Do not repeat population'
            run('mount','-t',fs,'-o','rw' if POPULATE else ('ro,noload' if fs == 'ext3' else 'ro'),p['path'],str(folder))
            try:
                if POPULATE:
                    assert {c.name for c in folder.iterdir()} <= {'lost+found'}, 'Expected empty fixture'
                    (folder/'目录').mkdir()
                    (folder/'目录'/'说明.txt').write_text('DiskStudio Ext2/3 跨系统测试\n'*4096,encoding='utf-8')
                    block = bytes(range(256))*4096
                    # Force later files beyond the future 384 MiB partition end.
                    with open(folder/'temporary.bin','wb') as f:
                        for _ in range(640): f.write(block)
                    with open(folder/'large.bin','wb') as f:
                        for _ in range(80): f.write(block)
                    os.link(folder/'large.bin',folder/'hardlink.bin')
                    os.symlink('目录/说明.txt',folder/'符号链接')
                    with open(folder/'sparse.bin','wb') as f:
                        f.write(b'first'); f.seek(160*1024**2); f.write(b'last')
                    os.chmod(folder/'large.bin',0o640)
                    os.setxattr(folder/'large.bin','user.diskstudio','中文属性'.encode())
                    os.chown(folder/'目录'/'说明.txt',1234,2345)
                    for i in range(16):
                        with open(folder/f'fragment-{i}.bin','wb') as f:
                            for _ in range(4): f.write(hashlib.sha256(str(i).encode()).digest()*32768)
                    for i in range(0,16,2): (folder/f'fragment-{i}.bin').unlink()
                    (folder/'temporary.bin').unlink()
                    run('sync')
                    run('filefrag','-v',str(folder/'large.bin'))
                    baseline.write_text(json.dumps({'uuid':p['uuid'],'partuuid':p['partuuid'],'files':manifest(folder)},ensure_ascii=False,sort_keys=True),encoding='utf-8')
                else:
                    saved = json.loads(baseline.read_text(encoding='utf-8'))
                    assert saved['uuid'] == p['uuid'] and saved['partuuid'] == p['partuuid']
                    assert saved['files'] == manifest(folder), 'Contents or metadata changed'
                    assert (folder/'large.bin').stat().st_ino == (folder/'hardlink.bin').stat().st_ino
                    assert (folder/'sparse.bin').stat().st_blocks*512 < 1024**2, 'Sparse holes lost'
                print('DISKSTUDIO_'+PHASE.upper().replace('-','_')+'_PASS '+label,flush=True)
            finally: run('umount',str(folder))
            run('e2fsck','-f','-n',p['path'])
    print('DISKSTUDIO_'+PHASE.upper().replace('-','_')+'_ALL_PASS',flush=True)

try: check()
except BaseException:
    print('DISKSTUDIO_EXT_FAILED',flush=True)
    traceback.print_exc()
    sys.exit(1)
