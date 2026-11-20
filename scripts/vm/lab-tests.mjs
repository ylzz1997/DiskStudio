import test from 'node:test';
// Run explicitly with node --test; do not let the frontend runner collect this file.
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import { root, safePath, checkQcowHeader, buildArgs, disks, validateTestBlocks } from './lab.mjs';

fs.mkdirSync(path.join(root, 'tests'), { recursive: true });
const folder = fs.mkdtempSync(path.join(root, 'tests', 'guard-'));
test('VM paths reject devices, traversal, external files, hardlinks and links', () => {
  for (const name of ['../outside.img', '\\\\.\\PhysicalDrive0', '\\\\?\\GLOBALROOT\\Device\\Harddisk0', 'https://example.com/disk', 'test.qcow2:stream']) {
    assert.throws(() => safePath(path.resolve(folder, name), folder, false));
  }
  const original = path.join(folder, 'ordinary.bin');
  fs.writeFileSync(original, 'test');
  assert.equal(safePath(original, folder), original);
  const linked = path.join(folder, 'hardlink.bin');
  fs.linkSync(original, linked);
  assert.throws(() => safePath(linked, folder), /hardlinked/);
  const outside = fs.mkdtempSync(path.join(root, 'tests', 'outside-'));
  const junction = path.join(folder, 'junction');
  fs.symlinkSync(outside, junction, 'junction');
  assert.throws(() => safePath(path.join(junction, 'a'), folder, false), /Links/);
  fs.unlinkSync(junction);
  fs.unlinkSync(linked);
});
test('QCOW2 cannot redirect access through backing or external data files', () => {
  const filename = path.join(folder, 'header.qcow2');
  const header = Buffer.alloc(104);
  header.writeUInt32BE(0x514649fb, 0); header.writeUInt32BE(3, 4);
  header.writeBigUInt64BE(16n * 1024n ** 3n, 24);
  fs.writeFileSync(filename, header);
  assert.equal(checkQcowHeader(filename, folder), 16 * 1024 ** 3);
  header.writeBigUInt64BE(128n, 8); fs.writeFileSync(filename, header);
  assert.throws(() => checkQcowHeader(filename, folder), /backing/);
  header.writeBigUInt64BE(0n, 8); header.writeBigUInt64BE(4n, 72); fs.writeFileSync(filename, header);
  assert.throws(() => checkQcowHeader(filename, folder), /External data/);
});
test('test profile uses only project files, no NIC or host integration', () => {
  const args = buildArgs();
  assert.equal(args[args.indexOf('-accel') + 1], 'tcg');
  assert.equal(args[args.indexOf('-uuid') + 1], 'e20adb9f-f665-432e-9a62-dd5d897b06a0');
  assert.ok(args.includes('type=1,manufacturer=DiskStudio,product=DiskStudio-Test,serial=DS-VM-0001'));
  assert.ok(!args.includes('-no-reboot'), 'Normal TCG boot must allow guest restarts');
  assert.ok(buildArgs({ accelerator: 'whpx' }).includes('-no-reboot'), 'WHPX must avoid the observed warm-reset failure');
  assert.equal(args[args.indexOf('-nic') + 1], 'none');
  assert.ok(args.includes('tcp:127.0.0.1:49365,server=on,wait=off'));
  const blocks = args.flatMap((value, i) => value === '-blockdev' ? [JSON.parse(args[i + 1])] : []);
  for (const disk of disks) {
    const block = blocks.find(b => b['node-name'] === disk.id);
    assert.equal(block.driver, 'qcow2');
    assert.equal(block.file.driver, 'file');
    assert.equal(block.file.filename, path.join(root, 'windows', disk.name));
  }
  assert.doesNotMatch(args.join(' '), /PhysicalDrive|GLOBALROOT|usb-host|virtfs|smb=|hostfwd|clipboard|spice|tpmdev/i);
  assert.throws(() => buildArgs({ accelerator: 'anything' }), /Unsupported/);
});
test('Windows installer sees one OS disk and read-only optical media', () => {
  const args = buildArgs({ setup: true });
  const blocks = args.flatMap((value, i) => value === '-blockdev' ? [JSON.parse(args[i + 1])] : []);
  assert.deepEqual(blocks.filter(b => b.driver === 'qcow2').map(b => b['node-name']), ['os']);
  assert.equal(blocks.find(b => b['node-name'] === 'installer')['read-only'], true);
  assert.doesNotMatch(args.join(' '), /DS-TEST/);
});
test('4Kn profile keeps the OS and substitutes only the isolated NVMe pair', () => {
  const args = buildArgs({ fourKn: true });
  const blocks = args.flatMap((value, i) => value === '-blockdev' ? [JSON.parse(args[i + 1])] : []);
  const paths = blocks.filter(b => b.driver === 'qcow2').map(b => b.file.filename);
  assert.deepEqual(paths, ['system.qcow2', 'test-a-4kn.qcow2', 'test-b-4kn.qcow2'].map(file => path.join(root, 'windows', file)));
  const nvme = args.filter(arg => arg.startsWith('nvme,'));
  assert.deepEqual(nvme, disks.filter(d => d.bus === 'nvme').map(d =>
    `nvme,drive=${d.id},serial=${d.serial},id=${d.id}_4kn,logical_block_size=4096,physical_block_size=4096`));
  assert.equal(args[args.indexOf('-nic') + 1], 'none');
  assert.throws(() => buildArgs({ fourKn: true, setup: true }), /4Kn/);
  assert.throws(() => buildArgs({ fourKn: true, network: true }), /4Kn/);
});
test('clone and restore fixtures use independent NVMe images and start paused', () => {
  assert.throws(() => buildArgs({ fourKn: true, fixture: '../other' }), /separate 4Kn/);
  for (const fixture of ['whole-clone', 'btrfs-clone', 'mixed-clone', 'btrfs-restore', 'archive-restore']) {
    assert.throws(() => buildArgs({ fixture }), /separate 4Kn/);
    assert.throws(() => buildArgs({ fourKn: true, fixture, sectorFault: true }), /separate 4Kn/);
    const args = buildArgs({ fourKn: true, fixture });
    const blocks = args.flatMap((value, i) => value === '-blockdev' ? [JSON.parse(args[i + 1])] : []);
    assert.ok(args.includes('-S'));
    assert.equal(args[args.indexOf('-nic') + 1], 'none');
    assert.deepEqual(blocks.filter(b => b.driver === 'qcow2').map(b => b.file.filename),
      ['system.qcow2', `test-a-${fixture}-4kn.qcow2`, `test-b-${fixture}-4kn.qcow2`].map(file => path.join(root, 'windows', file)));
    assert.equal(blocks.find(b => b['node-name'] === 'os').discard, undefined);
    for (const block of blocks.filter(b => ['test_a', 'test_b'].includes(b['node-name']))) {
      assert.equal(block.file.driver, 'file');
      assert.equal(block.discard, 'unmap');
      assert.equal(block['detect-zeroes'], 'unmap');
    }
  }
});
test('running-test preflight rejects replaced disks, wrong capacities and extra devices', () => {
  const args = buildArgs();
  const blocks = args.flatMap((value, i) => {
    if (value !== '-blockdev') return [];
    const block = JSON.parse(args[i + 1]);
    const disk = disks.find(d => d.id === block['node-name']);
    return [{ removable: !disk, inserted: { 'node-name': block['node-name'],
      drv: block.driver, file: block.file.filename, ro: !disk,
      image: { 'virtual-size': disk ? disk.size * 1024 ** 3 : 0 } } }];
  });
  assert.doesNotThrow(() => validateTestBlocks(blocks));
  assert.throws(() => validateTestBlocks(blocks, true));
  for (const change of [
    b => b.push({ removable: false, inserted: { file: '\\\\.\\PhysicalDrive0' } }),
    b => { b[1].inserted.file = b[0].inserted.file; },
    b => { b[1].inserted.image['virtual-size'] += 1024; },
    b => { b[1].inserted.ro = true; },
  ]) {
    const changed = structuredClone(blocks);
    change(changed);
    assert.throws(() => validateTestBlocks(changed));
  }
});
test('sector-copy fault wraps only the fixed Test A file and starts paused', () => {
  assert.throws(() => buildArgs({ sectorFault: true }), /4Kn/);
  assert.throws(() => buildArgs({ fourKn: true, sectorFault: true, accelerator: 'whpx' }), /TCG/);
  const args = buildArgs({ fourKn: true, sectorFault: true });
  assert.ok(args.includes('-S'));
  const backends = args.flatMap((value, index) => value === '-blockdev' ? [JSON.parse(args[index + 1])] : []);
  const a = backends.find(value => value['node-name'] === 'test_a');
  assert.equal(a.driver, 'blkdebug');
  assert.deepEqual(a['inject-error'], [{ event: 'none', iotype: 'read', sector: 2232323 * 8, errno: 5 }]);
  assert.equal(a.image.driver, 'qcow2');
  assert.equal(a.image.file.filename, path.join(root, 'windows', 'test-a-4kn.qcow2'));
  const blocks = backends.map(backend => {
    const disk = disks.find(candidate => candidate.id === backend['node-name']);
    const image = { 'virtual-size': disk ? disk.size * 1024 ** 3 : 0 };
    const file = backend.file?.filename;
    if (backend === a) image['backing-image'] = { format: 'qcow2', filename: a.image.file.filename };
    return { removable: !disk, inserted: { drv: backend.driver, file, ro: !disk, image,
      'node-name': backend['node-name'], backing_file_depth: backend === a ? 1 : 0 } };
  });
  assert.doesNotThrow(() => validateTestBlocks(blocks, true, true));
  assert.throws(() => validateTestBlocks(blocks, true));
  blocks.find(block => block.inserted['node-name'] === 'test_a').inserted.image['backing-image'].filename =
    path.join(root, 'windows', 'test-b-4kn.qcow2');
  assert.throws(() => validateTestBlocks(blocks, true, true));
});
test('actual QCOW2 snapshot restores written test sectors', () => {
  const img = path.join(root, 'tools/qemu/qemu-img.exe');
  const io = path.join(root, 'tools/qemu/qemu-io.exe');
  const filename = path.join(folder, 'rollback.qcow2');
  const run = (exe, args) => {
    const result = spawnSync(exe, args, { encoding: 'utf8', windowsHide: true, timeout: 30000 });
    assert.equal(result.status, 0, `${result.error ?? ''}\n${result.stderr}\n${result.stdout}`);
  };
  run(img, ['create', '-f', 'qcow2', filename, '8M']);
  checkQcowHeader(filename, folder);
  run(img, ['snapshot', '-c', 'blank', filename]);
  run(io, ['-f', 'qcow2', '-c', 'write -P 0x5a 0 4096', filename]);
  run(io, ['-f', 'qcow2', '-r', '-c', 'read -P 0x5a 0 4096', filename]);
  run(img, ['snapshot', '-a', 'blank', filename]);
  run(io, ['-f', 'qcow2', '-r', '-c', 'read -P 0x00 0 4096', filename]);
});
