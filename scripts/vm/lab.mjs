import fs from 'node:fs';
import path from 'node:path';
import net from 'node:net';
import crypto from 'node:crypto';
import { spawn, spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';

// This is a development tool, never imported into the desktop application.
// All writable VM resources live under .vm. No arbitrary QEMU arguments or
// disk paths are accepted. Host devices are never enumerated or opened here.
export const project = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '../..');
export const root = path.join(project, '.vm');
export const disks = Object.freeze([
  { id: 'os', name: 'system.qcow2', size: 80, serial: 'DS-OS-0001', model: 'DiskStudio OS', bus: 'sata' },
  { id: 'test_a', name: 'test-a.qcow2', size: 16, serial: 'DS-TEST-A-0001', label: 'DiskStudio Test A', bus: 'nvme' },
  { id: 'test_b', name: 'test-b.qcow2', size: 16, serial: 'DS-TEST-B-0001', label: 'DiskStudio Test B', bus: 'nvme' },
]);
const fourKnDisks = Object.freeze(disks.map(disk => disk.bus === 'nvme' ? {
  ...disk, name: disk.name.replace('.qcow2', '-4kn.qcow2'), controller: `${disk.id}_4kn`,
} : disk));
const profileDisks = fourKn => fourKn ? fourKnDisks : disks;
const uuid = 'e20adb9f-f665-432e-9a62-dd5d897b06a0';
const vmName = 'DiskStudio-Test';
const qemu = path.join(root, 'tools/qemu/qemu-system-x86_64.exe');
const qemuWindow = path.join(root, 'tools/qemu/qemu-system-x86_64w.exe');
const img = path.join(root, 'tools/qemu/qemu-img.exe');
const bios = path.join(root, 'tools/qemu/share/bios-256k.bin');
const statePath = path.join(root, 'run.json');
const installIso = path.join(root, 'media/windows-server-2025-zh-cn.iso');
const payloadIso = path.join(root, 'media/diskstudio-tools.iso');
const port = 49365;

function readToolsManifest(name) {
  const filename = safePath(path.join(root, name));
  const value = JSON.parse(fs.readFileSync(filename, 'utf8').replace(/^\uFEFF/, ''));
  if (!/^diskstudio-tools-[0-9a-f-]{36}\.iso$/.test(value.file ?? '') || !/^[0-9a-f]{64}$/.test(value.sha256 ?? '')) {
    throw new Error('Invalid immutable tools ISO manifest');
  }
  return { ...value, filename: safePath(path.join(root, 'media', value.file)) };
}
function currentToolsIso() {
  return fs.existsSync(path.join(root, 'current-tools.json')) ? readToolsManifest('current-tools.json').filename : payloadIso;
}

const same = (a, b) => path.resolve(a).toLowerCase() === path.resolve(b).toLowerCase();
export function safePath(filename, base = root, mustExist = true) {
  const full = path.resolve(filename);
  const rel = path.relative(base, full);
  if (!rel || rel.startsWith('..') || path.isAbsolute(rel) || /[,:]/.test(rel)) {
    throw new Error(`Path must stay inside the VM directory: ${filename}`);
  }
  let current = path.resolve(base);
  for (const part of ['', ...rel.split(path.sep)]) {
    if (part) current = path.join(current, part);
    if (!fs.existsSync(current)) {
      if (mustExist) throw new Error(`Missing VM file: ${current}`);
      continue;
    }
    const stat = fs.lstatSync(current);
    if (stat.isSymbolicLink() || !same(fs.realpathSync(current), current)) {
      throw new Error(`Links/junctions are not allowed in VM paths: ${current}`);
    }
    if (!stat.isDirectory() && (!stat.isFile() || stat.nlink !== 1)) {
      throw new Error(`Only ordinary, non-hardlinked VM files are allowed: ${current}`);
    }
  }
  return full;
}
function mkdir(relative) {
  const dir = safePath(path.join(root, relative), root, false);
  fs.mkdirSync(dir, { recursive: true });
  return dir;
}
function tool(exe, args, timeout = 120000) {
  safePath(exe);
  const result = spawnSync(exe, args, { encoding: 'utf8', windowsHide: true, timeout });
  if (result.error || result.status !== 0) {
    throw new Error(`${path.basename(exe)} failed: ${result.error?.message ?? result.stderr ?? result.stdout}`);
  }
  return result.stdout.trim();
}
function writeJson(filename, data, exclusive = false) {
  safePath(filename, root, false);
  fs.writeFileSync(filename, JSON.stringify(data, null, 2) + '\n', { flag: exclusive ? 'wx' : 'w' });
}
function readState() {
  if (!fs.existsSync(statePath)) return null;
  safePath(statePath);
  return JSON.parse(fs.readFileSync(statePath, 'utf8'));
}
function isAlive(pid) {
  if (!Number.isSafeInteger(pid) || pid < 1) return false;
  try { process.kill(pid, 0); return true; } catch { return false; }
}
function assertStopped() {
  if (isAlive(readState()?.pid)) throw new Error('VM is running. Shut down the guest before changing images/checkpoints.');
  const ubuntuState = path.join(root, 'ubuntu/run.json');
  if (fs.existsSync(ubuntuState) && isAlive(JSON.parse(fs.readFileSync(safePath(ubuntuState), 'utf8')).pid)) {
    throw new Error('Ubuntu verification VM is running. Shut it down before attaching or changing the shared test images.');
  }
}
function diskPath(disk) { return path.join(root, 'windows', disk.name); }

export function checkQcowHeader(filename, base = root) {
  safePath(filename, base);
  const fd = fs.openSync(filename, 'r');
  const header = Buffer.alloc(104);
  try { if (fs.readSync(fd, header, 0, header.length, 0) < 104) throw new Error('Truncated QCOW2 header'); }
  finally { fs.closeSync(fd); }
  if (header.readUInt32BE(0) !== 0x514649fb || ![2, 3].includes(header.readUInt32BE(4))) {
    throw new Error('Expected a QCOW2 file');
  }
  if (header.readBigUInt64BE(8) !== 0n || header.readUInt32BE(16) !== 0) {
    throw new Error('External backing files are not allowed');
  }
  if (header.readUInt32BE(4) === 3 && (header.readBigUInt64BE(72) & 4n) !== 0n) {
    throw new Error('External data files are not allowed');
  }
  return Number(header.readBigUInt64BE(24));
}
function checkDisk(disk) {
  const filename = diskPath(disk);
  const size = checkQcowHeader(filename);
  if (size !== disk.size * 1024 ** 3) throw new Error(`Unexpected capacity: ${disk.name}`);
  return filename;
}
function checkIso(filename) {
  safePath(filename);
  const fd = fs.openSync(filename, 'r');
  const magic = Buffer.alloc(5);
  try { fs.readSync(fd, magic, 0, 5, 32769); } finally { fs.closeSync(fd); }
  if (magic.toString() !== 'CD001') throw new Error(`Invalid ISO 9660 media: ${filename}`);
}

export function buildArgs({ accelerator = 'tcg', setup = false, headless = false, paused = false, network = false, fourKn = false } = {}) {
  if (!['whpx', 'tcg'].includes(accelerator)) throw new Error('Unsupported accelerator');
  if (fourKn && (setup || network)) throw new Error('4Kn acceptance uses the installed offline guest');
  const args = [
    '-name', vmName, '-uuid', uuid,
    '-smbios', 'type=1,manufacturer=DiskStudio,product=DiskStudio-Test,serial=DS-VM-0001',
    '-L', path.join(root, 'tools/qemu/share'),
    '-machine', 'q35,vmport=off', '-accel', accelerator === 'whpx' ? 'whpx,kernel-irqchip=off' : accelerator,
    '-cpu', accelerator === 'whpx' ? 'Nehalem,-x2apic' : 'max',
    '-boot', 'menu=on', '-rtc', 'base=localtime',
    '-smp', accelerator === 'whpx' ? '1' : '4', '-m', '8192', '-nodefaults', '-no-user-config',
    '-display', headless ? 'none' : 'gtk,gl=off', '-device', 'VGA,xres=1600,yres=1000',
    '-device', 'qemu-xhci', '-device', 'usb-tablet',
    '-monitor', 'none', '-serial', 'none', '-parallel', 'none',
    '-qmp', `tcp:127.0.0.1:${port},server=on,wait=off`,
    '-bios', bios,
  ];
  for (const disk of profileDisks(fourKn)) {
    // During Windows Setup, expose only the OS disk. Test disks are attached later.
    if (setup && disk.id !== 'os') continue;
    args.push('-blockdev', JSON.stringify({ driver: 'qcow2', 'node-name': disk.id, file: { driver: 'file', filename: diskPath(disk) } }));
    args.push('-device', disk.bus === 'nvme'
      ? `nvme,drive=${disk.id},serial=${disk.serial}${fourKn ? `,id=${disk.controller},logical_block_size=4096,physical_block_size=4096` : ''}`
      : `ide-hd,bus=ide.0,drive=${disk.id},serial=${disk.serial},model=${disk.model},bootindex=2`);
  }
  if (setup) {
    args.push('-blockdev', JSON.stringify({ driver: 'raw', 'node-name': 'installer', 'read-only': true, file: { driver: 'file', filename: installIso } }));
    args.push('-device', 'ide-cd,bus=ide.1,drive=installer,bootindex=1');
  }
  const toolsIso = currentToolsIso();
  if (fs.existsSync(toolsIso)) {
    args.push('-blockdev', JSON.stringify({ driver: 'raw', 'node-name': 'tools', 'read-only': true, file: { driver: 'file', filename: toolsIso } }));
    args.push('-device', 'ide-cd,bus=ide.2,drive=tools');
  }
  // Default: no virtual NIC. Optional user-mode NAT has no forwarding/shares.
  args.push('-nic', network ? 'user,model=e1000e' : 'none');
  // WHPX 11.1 has a reset failure on this host. Keep experiments from attempting
  // a warm reset; TCG is the default and supports normal in-guest restarts.
  if (accelerator === 'whpx') args.push('-no-reboot');
  if (paused) args.push('-S');
  return args;
}

async function connectQmp(timeout = 8000) {
  const socket = net.createConnection({ host: '127.0.0.1', port });
  let buffer = '', sequence = 0;
  const pending = new Map();
  const greeting = new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error('QMP connection timed out')), timeout);
    socket.once('error', error => { clearTimeout(timer); reject(error); });
    socket.on('data', chunk => {
      buffer += chunk;
      for (let index; (index = buffer.indexOf('\n')) >= 0;) {
        const line = buffer.slice(0, index).trim(); buffer = buffer.slice(index + 1);
        if (!line) continue;
        let msg;
        try { msg = JSON.parse(line); } catch { continue; }
        if (msg.QMP) { clearTimeout(timer); resolve(); }
        const wait = pending.get(msg.id);
        if (wait) { pending.delete(msg.id); clearTimeout(wait.timer); msg.error ? wait.reject(new Error(JSON.stringify(msg.error))) : wait.resolve(msg.return); }
      }
    });
    socket.on('close', () => {
      clearTimeout(timer);
      reject(new Error('QMP closed before greeting'));
      for (const wait of pending.values()) { clearTimeout(wait.timer); wait.reject(new Error('QMP closed')); }
      pending.clear();
    });
  });
  const execute = (command, args = {}) => new Promise((resolve, reject) => {
    const id = ++sequence;
    const timer = setTimeout(() => { pending.delete(id); reject(new Error(`QMP timeout: ${command}`)); }, timeout);
    pending.set(id, { resolve, reject, timer });
    socket.write(JSON.stringify({ execute: command, arguments: args, id }) + '\n');
  });
  try {
    await greeting;
    await execute('qmp_capabilities');
    if ((await execute('query-name')).name !== vmName || (await execute('query-uuid')).UUID !== uuid) {
      throw new Error('QMP target is not the DiskStudio test VM');
    }
    return { execute, close: () => socket.destroy() };
  } catch (error) { socket.destroy(); throw error; }
}
const delay = ms => new Promise(resolve => setTimeout(resolve, ms));
async function hashFile(filename) {
  const hash = crypto.createHash('sha256');
  for await (const bytes of fs.createReadStream(filename)) hash.update(bytes);
  return hash.digest('hex');
}
async function waitQmp(child) {
  let lastError;
  for (let attempt = 0; attempt < 40; attempt++) {
    if (!isAlive(child.pid)) throw new Error('QEMU exited. See .vm/logs/qemu.err.log');
    try { return await connectQmp(1000); } catch (error) { lastError = error; }
    await delay(250);
  }
  throw lastError;
}
async function withLock(action) {
  mkdir('locks');
  const filename = safePath(path.join(root, 'locks/manage.lock'), root, false);
  const fd = fs.openSync(filename, 'wx');
  fs.writeFileSync(fd, String(process.pid));
  try { return await action(); }
  finally { fs.closeSync(fd); fs.unlinkSync(filename); }
}

function init() {
  assertStopped();
  mkdir('windows'); mkdir('logs'); mkdir('checkpoints'); mkdir('media');
  safePath(qemu); safePath(img); safePath(bios);
  for (const disk of disks) {
    const filename = safePath(diskPath(disk), root, false);
    if (!fs.existsSync(filename)) tool(img, ['create', '-f', 'qcow2', filename, `${disk.size}G`]);
    checkDisk(disk);
  }
  writeJson(path.join(root, 'manifest.json'), {
    name: vmName, uuid, memoryMiB: 8192, vcpus: 4, firmware: 'seabios', accelerator: 'tcg', cpuModel: 'max',
    guest: 'Windows Server 2025 Standard Evaluation (Desktop Experience)',
    disks, hostDiskPassthrough: false, sharedFolders: false, defaultNetwork: 'none',
    app: 'One production application build. All partition-operation tests run only inside this isolated VM on its two disposable NVMe disks; the host is used for metadata and UI inspection only.',
  });
  console.log('Initialized .vm/windows: 80 GiB system + 2 x 16 GiB test disks (growable QCOW2).');
}

async function start(options) {
  assertStopped();
  safePath(qemu); safePath(bios);
  // Validate the fixed profile before creating its two independent images.
  const args = buildArgs(options);
  for (const disk of profileDisks(options.fourKn)) {
    if (disk.controller) {
      const filename = safePath(diskPath(disk), root, false);
      if (!fs.existsSync(filename)) tool(img, ['create', '-f', 'qcow2', filename, `${disk.size}G`]);
    }
    checkDisk(disk);
  }
  if (options.setup) checkIso(installIso);
  if (fs.existsSync(currentToolsIso())) checkIso(currentToolsIso());
  // Refuse to start if another service or VM owns our loopback QMP port.
  await new Promise((resolve, reject) => {
    const server = net.createServer();
    server.once('error', reject);
    server.listen(port, '127.0.0.1', () => server.close(resolve));
  });
  const out = fs.openSync(safePath(path.join(mkdir('logs'), 'qemu.out.log'), root, false), 'a');
  const err = fs.openSync(safePath(path.join(root, 'logs/qemu.err.log'), root, false), 'a');
  const started = new Date().toISOString();
  const marker = `\n--- ${started} ${JSON.stringify(options)} ---\n`;
  fs.writeSync(out, marker); fs.writeSync(err, marker);
  const executable = safePath(options.headless ? qemu : qemuWindow);
  const temp = mkdir('tmp');
  const child = spawn(executable, args, {
    cwd: root, detached: true, windowsHide: Boolean(options.headless), stdio: ['ignore', out, err],
    env: { ...process.env, TEMP: temp, TMP: temp, XDG_CACHE_HOME: mkdir('cache') },
  });
  child.on('error', () => {});
  fs.closeSync(out); fs.closeSync(err);
  writeJson(statePath, { pid: child.pid, uuid, started, ...options });
  child.unref();
  const qmp = await waitQmp(child);
  try {
    const status = await qmp.execute('query-status');
    const blocks = await qmp.execute('query-block');
    writeJson(path.join(root, 'last-boot.json'), { accelerator: options.accelerator ?? 'tcg', status, blocks });
    console.log(JSON.stringify({ pid: child.pid, status: status.status, disks: blocks.filter(b => b.inserted).map(b => ({ node: b.inserted['node-name'], file: b.inserted.file, readonly: b.inserted.ro })) }, null, 2));
  } finally { qmp.close(); }
  return child;
}

function checkpoint(name) {
  assertStopped();
  if (!/^[a-z0-9][a-z0-9_-]{0,47}$/i.test(name ?? '')) throw new Error('Checkpoint name: 1-48 letters, digits, _ or -');
  const manifest = path.join(root, 'checkpoints', `${name}.json`);
  if (fs.existsSync(manifest)) throw new Error('Checkpoint already exists; choose a new name');
  const tag = `ds-${crypto.randomUUID()}`;
  for (const disk of disks) checkDisk(disk);
  const completed = [];
  try {
    for (const disk of disks) { tool(img, ['snapshot', '-c', tag, diskPath(disk)]); completed.push(disk); }
    writeJson(manifest, { format: 2, name, tag, uuid, created: new Date().toISOString(), disks: disks.map(d => d.name), firmware: 'seabios' }, true);
  } catch (error) {
    // Leave any partial snapshot intact for diagnosis; never publish a partial checkpoint.
    throw new Error(`Checkpoint incomplete (${completed.length}/3 disks). No restorable manifest published. ${error.message}`);
  }
  console.log(`Checkpoint saved: ${name} (3 disks; guest must have shut down cleanly).`);
}

function restore(name) {
  assertStopped();
  if (!/^[a-z0-9][a-z0-9_-]{0,47}$/i.test(name ?? '')) throw new Error('Invalid checkpoint name');
  const file = safePath(path.join(root, 'checkpoints', `${name}.json`));
  const saved = JSON.parse(fs.readFileSync(file, 'utf8'));
  if (saved.format !== 2 || saved.uuid !== uuid || !/^ds-[0-9a-f-]{36}$/.test(saved.tag) || saved.disks.join('|') !== disks.map(d => d.name).join('|') || saved.firmware !== 'seabios') throw new Error('Invalid checkpoint manifest');
  for (const disk of disks) {
    checkDisk(disk);
    const info = JSON.parse(tool(img, ['info', '--output=json', '-f', 'qcow2', diskPath(disk)]));
    if (!info.snapshots?.some(s => s.name === saved.tag)) throw new Error(`Checkpoint missing on ${disk.name}`);
  }
  const before = `before-restore-${Date.now()}`;
  checkpoint(before);
  for (const disk of disks) tool(img, ['snapshot', '-a', saved.tag, diskPath(disk)]);
  console.log(`Restored ${name}. Previous state retained as ${before}.`);
}

async function doctor() {
  console.log(tool(qemu, ['--version']).split('\n')[0]);
  console.log(tool(qemu, ['-accel', 'help']));
  const tpm = spawnSync(qemu, ['-tpmdev', 'help'], { encoding: 'utf8', windowsHide: true });
  console.log(tpm.status === 0 ? tpm.stdout.trim() : 'TPM emulator: unavailable in this Windows QEMU build');
  for (const disk of disks) {
    if (!fs.existsSync(diskPath(disk))) continue;
    console.log(`${disk.name}: ${checkDisk(disk)} (${disk.size} GiB virtual, ${fs.statSync(diskPath(disk)).size} bytes file)`);
  }
  console.log(`Installer ISO: ${fs.existsSync(installIso) ? 'ready' : 'not downloaded yet'}`);
  console.log(`Tools ISO: ${fs.existsSync(currentToolsIso()) ? currentToolsIso() : 'not prepared yet'}`);
  console.log(`VM process: ${isAlive(readState()?.pid) ? 'running' : 'stopped'}`);
}

async function refreshTools() {
  const state = readState();
  if (!isAlive(state?.pid)) throw new Error('Start the test VM before hot-swapping its tools CD');
  const next = readToolsManifest('pending-tools.json');
  checkIso(next.filename);
  if (await hashFile(next.filename) !== next.sha256) throw new Error('Tools ISO hash mismatch');
  const qmp = await connectQmp();
  try {
    const blocks = await qmp.execute('query-block');
    await validateProfile(qmp, blocks, state.fourKn === true);
    const optical = blocks.filter(b => b.removable && b.inserted?.ro === true && b.inserted.drv === 'raw');
    if (optical.length !== 1 || !same(optical[0].inserted.file, currentToolsIso()) || !optical[0].qdev?.startsWith('/machine/')) throw new Error('Unable to identify the tools CD');
    await qmp.execute('blockdev-change-medium', { id: optical[0].qdev, filename: next.filename, format: 'raw', 'read-only-mode': 'read-only', force: false });
    const after = (await qmp.execute('query-block')).find(b => b.qdev === optical[0].qdev);
    if (!after?.inserted?.ro || after.inserted.drv !== 'raw' || !same(after.inserted.file, next.filename)) throw new Error('Tools CD change was not verified; inspect before retrying');
    writeJson(path.join(root, 'current-tools.json'), { file: next.file, sha256: next.sha256 });
    console.log(`Read-only tools CD updated: ${next.file}. Copy DiskStudio.exe inside the guest; no reboot required.`);
  } finally { qmp.close(); }
}

// Test-only isolation check. The production application has no VM identity,
// disk serial allowlist or special build. This harness owns the lab boundary.
export function validateTestBlocks(blocks, fourKn = false) {
  if (blocks.length !== 4) throw new Error('Unexpected test VM block inventory');
  for (const disk of profileDisks(fourKn)) {
    const found = blocks.filter(b => b.inserted?.['node-name'] === disk.id);
    if (found.length !== 1 || found[0].removable || found[0].inserted.ro !== false ||
        found[0].inserted.drv !== 'qcow2' || !same(found[0].inserted.file, checkDisk(disk)) ||
        found[0].inserted.image?.['virtual-size'] !== disk.size * 1024 ** 3) {
      throw new Error('A test disk is missing, replaced, or has an unexpected capacity');
    }
  }
  const optical = blocks.filter(b => b.removable && b.inserted?.ro === true && b.inserted.drv === 'raw');
  if (optical.length !== 1 || !same(optical[0].inserted.file, currentToolsIso())) throw new Error('Unexpected optical media');
}

async function validateProfile(qmp, blocks, fourKn) {
  validateTestBlocks(blocks, fourKn);
  if (!fourKn) return;
  for (const disk of fourKnDisks.filter(d => d.controller)) {
    const controller = `/machine/peripheral/${disk.controller}`;
    const block = blocks.find(b => b.inserted?.['node-name'] === disk.id);
    if (block.qdev !== disk.controller && block.qdev !== controller && !block.qdev?.startsWith(`${controller}/`)) {
      throw new Error('4Kn block backend is attached to an unexpected controller');
    }
    for (const [property, expected] of [['logical_block_size', 4096], ['physical_block_size', 4096], ['serial', disk.serial]]) {
      const value = await qmp.execute('qom-get', { path: controller, property });
      if (value !== expected) throw new Error(`4Kn controller ${disk.controller} has an unexpected ${property}`);
    }
  }
}

async function preflight() {
  const state = readState();
  if (!isAlive(state?.pid)) throw new Error('Start the test VM first');
  const qmp = await connectQmp(); // Checks the fixed VM name and UUID.
  try {
    await validateProfile(qmp, await qmp.execute('query-block'), state.fourKn === true);
    console.log('PASS: DiskStudio-Test identity, three isolated QCOW2 disks, capacities and read-only tools CD verified.');
    if (state.fourKn) console.log('Both disposable NVMe controllers expose 4096-byte logical and physical sectors.');
    console.log('Partition tests may target ONLY DS-TEST-A-0001 and DS-TEST-B-0001 (16 GiB each), never the 80 GiB OS disk.');
  } finally { qmp.close(); }
}

async function ioStats() {
  const state = readState();
  if (!isAlive(state?.pid)) throw new Error('Start the test VM first');
  const qmp = await connectQmp();
  try {
    await validateProfile(qmp, await qmp.execute('query-block'), state.fourKn === true);
    const stats = await qmp.execute('query-blockstats');
    console.log(stats.filter(s => disks.some(d => d.id === s['node-name'])).map(s => ({
      disk: s['node-name'], readBytes: s.stats.rd_bytes, writtenBytes: s.stats.wr_bytes,
      readOperations: s.stats.rd_operations, writeOperations: s.stats.wr_operations,
    })));
  } finally { qmp.close(); }
}

async function moveStopWindow(enabled) {
  const state = readState();
  if (!isAlive(state?.pid)) throw new Error('Start the test VM first');
  const qmp = await connectQmp();
  try {
    const blocks = await qmp.execute('query-block');
    await validateProfile(qmp, blocks, state.fourKn === true);
    const block = blocks.find(b => b.inserted?.['node-name'] === 'test_a');
    const namedController = state.fourKn === true && block.qdev === fourKnDisks.find(d => d.id === 'test_a').controller;
    if (!namedController && !block.qdev?.startsWith('/machine/')) throw new Error('Unable to bind the throttle to Test A');
    const mibPerSecond = state.fourKn === true ? 4 : 8;
    const rate = enabled ? mibPerSecond * 1024 ** 2 : 0;
    await qmp.execute('block_set_io_throttle', {
      id: block.qdev, group: 'diskstudio-test-a-move',
      bps: 0, bps_rd: 0, bps_wr: rate, iops: 0, iops_rd: 0, iops_wr: 0,
    });
    const after = await qmp.execute('query-block');
    await validateProfile(qmp, after, state.fourKn === true);
    const writeRate = item => Number(item?.inserted?.bps_wr ?? 0);
    for (const previous of blocks) {
      const expected = previous.qdev === block.qdev ? rate : writeRate(previous);
      if (writeRate(after.find(b => b.qdev === previous.qdev)) !== expected) {
        throw new Error('Test A write throttle could not be verified');
      }
    }
    console.log(enabled ? `Test A write limit: ${mibPerSecond} MiB/s.` : 'Test A write limit cleared.');
  } finally { qmp.close(); }
}

async function main() {
  const [command, ...args] = process.argv.slice(2);
  if (!['init', 'doctor', 'smoke', 'start', 'install', 'status', 'shutdown', 'poweroff', 'login-keys', 'checkpoint', 'restore', 'refresh-tools', 'preflight', 'io-stats', 'pause-vm', 'resume-vm', 'move-stop-window', 'move-stop-clear'].includes(command)) {
    console.log('Usage: node scripts/vm/lab.mjs init|doctor|smoke|start|install|status|shutdown|poweroff|login-keys|refresh-tools|preflight|io-stats|pause-vm|resume-vm|move-stop-window|move-stop-clear|checkpoint NAME|restore NAME');
    console.log('start/install: TCG by default. Optional --whpx (experimental; reboot exits), --network (user-mode NAT, no shares/forwarding)');
    console.log('start --4kn: use the separate offline pair of 4096-byte NVMe fixtures');
    process.exitCode = command ? 1 : 0; return;
  }
  const allowed = ['start', 'install', 'smoke'].includes(command) ? ['--tcg', '--whpx', '--network'] : [];
  if (command === 'start') allowed.push('--4kn');
  if (!['checkpoint', 'restore'].includes(command) && args.some(a => !allowed.includes(a))) throw new Error('Unsupported option; arbitrary paths/QEMU arguments are not accepted');
  if (['checkpoint', 'restore'].includes(command) && args.length !== 1) throw new Error('Provide exactly one checkpoint name');
  if (args.includes('--tcg') && args.includes('--whpx')) throw new Error('Choose only one accelerator');
  if (command === 'doctor') return doctor();
  if (command === 'preflight') return preflight();
  if (command === 'io-stats') return ioStats();
  if (['move-stop-window', 'move-stop-clear'].includes(command)) return moveStopWindow(command === 'move-stop-window');
  if (['pause-vm', 'resume-vm'].includes(command)) {
    const state = readState();
    if (!isAlive(state?.pid)) throw new Error('Start the test VM first');
    const qmp = await connectQmp();
    try {
      await validateProfile(qmp, await qmp.execute('query-block'), state.fourKn === true);
      await qmp.execute(command === 'pause-vm' ? 'stop' : 'cont');
      console.log(await qmp.execute('query-status'));
    } finally { qmp.close(); }
    return;
  }
  if (['status', 'shutdown', 'poweroff', 'login-keys'].includes(command)) {
    if (!isAlive(readState()?.pid)) { console.log('VM is stopped.'); return; }
    const qmp = await connectQmp();
    try {
      if (command === 'login-keys') {
        // Manually invoked by the user. No passwords or other input are accepted.
        // QMP identity has already been checked; no host keyboard input is used.
        await qmp.execute('send-key', { keys: ['ctrl', 'alt', 'delete'].map(data => ({ type: 'qcode', data })), 'hold-time': 100 });
        console.log('Sent Ctrl+Alt+Delete to DiskStudio-Test. Enter your password manually in the VM window.');
      } else {
        console.log(await qmp.execute(command === 'status' ? 'query-status' : command === 'poweroff' ? 'quit' : 'system_powerdown'));
      }
    }
    finally { qmp.close(); }
    return;
  }
  return withLock(async () => {
    if (command === 'refresh-tools') return refreshTools();
    if (command === 'init') return init();
    if (command === 'checkpoint') return checkpoint(args[0]);
    if (command === 'restore') return restore(args[0]);
    const options = { accelerator: args.includes('--whpx') ? 'whpx' : 'tcg', setup: command === 'install', network: args.includes('--network'), fourKn: args.includes('--4kn') };
    if (options.accelerator === 'whpx') console.warn('Experimental WHPX: Windows startup/reset was unreliable on this host. Guest reboot exits QEMU; use start again.');
    if (command === 'smoke') {
      assertStopped();
      const before = await Promise.all(disks.map(d => hashFile(checkDisk(d))));
      const child = await start({ ...options, headless: true, paused: true });
      const qmp = await connectQmp();
      try {
        const blocks = await qmp.execute('query-block');
        for (const disk of disks) if (!blocks.some(b => b.inserted?.['node-name'] === disk.id)) throw new Error(`Missing test disk ${disk.id}`);
        await qmp.execute('quit');
      } finally { qmp.close(); }
      for (let i = 0; i < 40 && isAlive(child.pid); i++) await delay(250);
      if (isAlive(child.pid)) throw new Error('Smoke VM did not exit');
      const after = await Promise.all(disks.map(d => hashFile(checkDisk(d))));
      if (JSON.stringify(before) !== JSON.stringify(after)) throw new Error('Paused smoke check unexpectedly changed a disk file');
      console.log('PASS: accelerator starts, three file-backed disks present, clean exit, all disk hashes unchanged.');
    } else await start(options);
  });
}
if (process.argv[1] && same(process.argv[1], fileURLToPath(import.meta.url))) {
  main().catch(error => { console.error(error.message); process.exitCode = 1; });
}
