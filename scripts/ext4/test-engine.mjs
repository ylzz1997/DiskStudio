import fs from 'node:fs';
import path from 'node:path';
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { project, root, safePath } from '../vm/lab.mjs';
const MiB = 1024 ** 2;
const dir = safePath(path.join(root, 'ext4-tests'), root, false);
fs.mkdirSync(dir, { recursive: true });
const file = safePath(path.join(dir, `bounded-${Date.now()}.img`), root, false);
const fd = fs.openSync(file, 'wx+');
const guard = Buffer.alloc(MiB, 0xa7), maximum = 768 * MiB;
fs.ftruncateSync(fd, maximum + 2 * MiB);
fs.writeSync(fd, guard, 0, MiB, 0);
fs.writeSync(fd, guard, 0, MiB, maximum + MiB);
const base = path.join(project, '.vm/ext4-source/e2fsprogs-1.47.4');
function run(tool, args, length, write, expected = 0) {
  const sub = { mke2fs: 'misc', e2fsck: 'e2fsck', resize2fs: 'resize' }[tool];
  const r = spawnSync(path.join(base, sub, `${tool}.exe`), args, {
    stdio: [fd, 'pipe', 'pipe'], windowsHide: true, encoding: 'utf8', timeout: 120000, maxBuffer: 2 * MiB,
    env: { ...process.env, MKE2FS_CONFIG: path.join(project, 'scripts/ext4/mke2fs.conf'), E2FSCK_CONFIG: 'NUL', LC_ALL: 'C',
      DISKSTUDIO_EXT4_OFFSET: String(MiB), DISKSTUDIO_EXT4_LENGTH: String(length), DISKSTUDIO_EXT4_SECTOR: '512', DISKSTUDIO_EXT4_WRITE: write ? '1' : '0' },
  });
  console.log(tool, args.join(' '), r.status, (r.stdout || '') + (r.stderr || ''));
  assert.ifError(r.error);
  if (expected === 'fail') assert.notEqual(r.status, 0); else assert.equal(r.status, expected);
}
function guards() {
  const b = Buffer.alloc(MiB);
  fs.readSync(fd, b, 0, MiB, 0); assert.deepEqual(b, guard);
  fs.readSync(fd, b, 0, MiB, maximum + MiB); assert.deepEqual(b, guard);
  assert.equal(fs.fstatSync(fd).size, maximum + 2 * MiB);
}
try {
  run('mke2fs', ['-q', '-F', '-t', 'ext4', '-b', '4096', '-I', '256', '-L', Buffer.from('Linux资料').toString('hex'), '-E', 'lazy_itable_init=0,lazy_journal_init=0,nodiscard', 'diskstudio'], 512 * MiB, true);
  guards();
  const sb = Buffer.alloc(1024); fs.readSync(fd, sb, 0, 1024, MiB + 1024);
  assert.equal(sb.readUInt16LE(56), 0xef53);
  assert.ok(sb.readUInt32LE(92) & 4, 'journal enabled');
  assert.ok(sb.readUInt32LE(96) & 0x40, 'extents enabled');
  assert.ok(sb.readUInt32LE(100) & 0x400, 'metadata checksums enabled');
  assert.equal(sb.subarray(120,136).toString('utf8').replace(/\0.*$/, ''), 'Linux资料');
  run('e2fsck', ['-f', '-n', 'diskstudio'], 512 * MiB, false);
  run('resize2fs', ['-P', 'diskstudio'], 512 * MiB, false);
  run('e2fsck', ['-f', '-p', 'diskstudio'], 512 * MiB, true);
  run('resize2fs', ['diskstudio', '65536'], 512 * MiB, true);
  guards();
  run('e2fsck', ['-f', '-n', 'diskstudio'], 256 * MiB, false);
  run('resize2fs', ['diskstudio', '196608'], maximum, true);
  run('e2fsck', ['-f', '-n', 'diskstudio'], maximum, false);
  guards();
  run('mke2fs', ['-F', '-t', 'ext4', 'diskstudio'], maximum, false, 'fail');
  run('resize2fs', ['diskstudio', '262144'], maximum, true, 'fail');
  run('e2fsck', ['-f', '-n', '../invalid'], maximum, false, 'fail');
  guards();
  fs.writeFileSync(path.join(dir, 'last-result.json'), JSON.stringify({ file, passed: true, format: 'ext4', shrink: '512→256 MiB', grow: '256→768 MiB', checked: new Date().toISOString() }, null, 2));
  console.log('PASS: bounded native Windows format/check/shrink/grow; guard bytes and file length unchanged');
} finally { fs.closeSync(fd); }
