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
const fixtureProfiles = ['whole-clone', 'btrfs-clone', 'btrfs-exact-clone', 'mixed-clone', 'btrfs-restore', 'archive-restore', 'capacity-restore', 'fat-restore', 'ext-copy', 'signature-scope', 'signature-formats', 'ldm'];
function profileDisks(fourKn, fixture) {
  const base = fourKn ? fourKnDisks : disks;
  return fixture ? base.map(disk => disk.bus === 'nvme' ? {
    ...disk, name: disk.name.replace(/(?:-4kn)?\.qcow2$/, `-${fixture}${fourKn ? '-4kn' : ''}.qcow2`),
  } : disk) : base;
}
const stateFixture = state => state.fixture ?? (state.wholeClone === true ? 'whole-clone' : undefined);
function validateFixtureProfile(fourKn, sectorFault, fixture) {
  if (fixture === 'ldm') {
    if (sectorFault) throw new Error('LDM fixtures require unmodified disk I/O');
    return;
  }
  if (fixture === 'ext-copy') {
    if (fourKn || sectorFault) throw new Error('Ext copy fixture requires its own separate 512-byte profile');
    return;
  }
  if (fixture !== undefined && (!fixtureProfiles.includes(fixture) || !fourKn || sectorFault)) {
    throw new Error('Clone fixture requires its own separate 4Kn profile');
  }
}
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
// One logical 4Kn sector in the known Test A data extent. blkdebug uses 512 B units.
const sectorCopyFaultLba = 2232323;
const surfaceRepairFaultLba = 3932163;
const faultBaseNode = 'test_a_sector_copy_source';

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

export function buildArgs({ accelerator = 'tcg', setup = false, headless = false, paused = false, network = false, fourKn = false, sectorFault = false, repairFault = false, fixture } = {}) {
  if (!['whpx', 'tcg'].includes(accelerator)) throw new Error('Unsupported accelerator');
  if (fourKn && (setup || network)) throw new Error('4Kn acceptance uses the installed offline guest');
  if (fixture && (setup || network)) throw new Error('Dedicated fixtures use the installed offline guest');
  if (repairFault && !sectorFault) throw new Error('Repair fault requires the fixed sector fault profile');
  if (sectorFault && (!fourKn || accelerator !== 'tcg')) throw new Error('Sector fault fixture requires 4Kn TCG');
  validateFixtureProfile(fourKn, sectorFault, fixture);
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
  for (const disk of profileDisks(fourKn, fixture)) {
    // During Windows Setup, expose only the OS disk. Test disks are attached later.
    if (setup && disk.id !== 'os') continue;
    const image = { driver: 'qcow2', 'node-name': disk.id, file: { driver: 'file', filename: diskPath(disk) } };
    if (fixture && disk.bus === 'nvme') {
      image.discard = 'unmap';
      image['detect-zeroes'] = 'unmap';
    }
    const backend = sectorFault && disk.id === 'test_a' ? {
      driver: 'blkdebug', 'node-name': disk.id, image: { ...image, 'node-name': faultBaseNode },
      'inject-error': [{ event: 'none', iotype: 'read', sector: (repairFault ? surfaceRepairFaultLba : sectorCopyFaultLba) * 8, errno: 5 }],
    } : image;
    if (repairFault && disk.id === 'test_a') {
      // Replace the injected read error after a Test A guest write completes.
      // The verifier checks the exact sector, preserved neighbors, and QMP bytes.
      backend['inject-error'].push({
        event: 'pwritev_done', iotype: 'read', sector: surfaceRepairFaultLba * 8, errno: 0,
      });
    }
    args.push('-blockdev', JSON.stringify(backend));
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
  if (paused || sectorFault || fixture) args.push('-S');
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
  for (const disk of profileDisks(options.fourKn, options.fixture)) {
    if (disk.controller || (options.fixture && disk.bus === 'nvme')) {
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
    const blocks = await qmp.execute('query-block');
    if (options.sectorFault || options.fixture) {
      await validateProfile(qmp, blocks, options.fourKn, options.sectorFault, options.fixture);
      await qmp.execute('cont');
    }
    const status = await qmp.execute('query-status');
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
    await validateProfile(qmp, blocks, state.fourKn === true, state.sectorFault === true, stateFixture(state));
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
export function validateTestBlocks(blocks, fourKn = false, sectorFault = false, fixture) {
  if (blocks.length !== 4) throw new Error('Unexpected test VM block inventory');
  if (sectorFault && !fourKn) throw new Error('Sector fault fixture requires the 4Kn inventory');
  validateFixtureProfile(fourKn, sectorFault, fixture);
  for (const disk of profileDisks(fourKn, fixture)) {
    const found = blocks.filter(b => b.inserted?.['node-name'] === disk.id);
    const info = found[0]?.inserted;
    const injected = sectorFault && disk.id === 'test_a';
    const image = injected ? info?.image?.['backing-image'] : info?.image;
    const file = injected ? image?.filename : info?.file;
    if (found.length !== 1 || found[0].removable || info.ro !== false ||
        info.drv !== (injected ? 'blkdebug' : 'qcow2') || typeof file !== 'string' || !same(file, checkDisk(disk)) ||
        info.image?.['virtual-size'] !== disk.size * 1024 ** 3 ||
        (injected && (image.format !== 'qcow2' || info.backing_file_depth !== 1 || image['backing-image']))) {
      throw new Error('A test disk is missing, replaced, or has an unexpected capacity');
    }
  }
  const optical = blocks.filter(b => b.removable && b.inserted?.ro === true && b.inserted.drv === 'raw');
  if (optical.length !== 1 || !same(optical[0].inserted.file, currentToolsIso())) throw new Error('Unexpected optical media');
}

async function validateProfile(qmp, blocks, fourKn, sectorFault = false, fixture) {
  validateTestBlocks(blocks, fourKn, sectorFault, fixture);
  if (sectorFault) {
    const nodes = await qmp.execute('query-named-block-nodes');
    const top = nodes.find(node => node['node-name'] === 'test_a');
    const base = nodes.find(node => node['node-name'] === faultBaseNode);
    const fileNode = nodes.find(node => node['node-name'] === base?.children?.[0]?.['node-name']);
    const expected = diskPath(fourKnDisks.find(disk => disk.id === 'test_a'));
    if (top?.drv !== 'blkdebug' || top.children?.length !== 1 || top.children[0].child !== 'image' ||
        top.children[0]['node-name'] !== faultBaseNode || base?.drv !== 'qcow2' || base.children?.length !== 1 ||
        base.children[0].child !== 'file' || base.backing_file_depth !== 0 || fileNode?.drv !== 'file' ||
        fileNode.children?.length !== 0 || !same(base.file, expected) || !same(fileNode.file, expected)) {
      throw new Error('Sector fault fixture does not wrap the single allowed Test A file');
    }
  }
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
    await validateProfile(qmp, await qmp.execute('query-block'), state.fourKn === true, state.sectorFault === true, stateFixture(state));
    console.log('PASS: DiskStudio-Test identity, three isolated QCOW2 disks, capacities and read-only tools CD verified.');
    if (state.fourKn) console.log('Both disposable NVMe controllers expose 4096-byte logical and physical sectors.');
    if (state.sectorFault) console.log(`Read fault restricted to Test A 4Kn LBA ${state.repairFault ? surfaceRepairFaultLba : sectorCopyFaultLba}.`);
    if (state.repairFault) console.log('Surface repair fixture clears the injected error on Test A write completion.');
    if (stateFixture(state) === 'whole-clone') console.log('Whole-disk cloning uses its independent A/B pair.');
    if (stateFixture(state) === 'btrfs-clone') console.log('Btrfs capacity cloning uses its independent A/B pair.');
    if (stateFixture(state) === 'btrfs-exact-clone') console.log('Btrfs same-size cloning uses its independent 4Kn A/B pair.');
    if (stateFixture(state) === 'mixed-clone') console.log('Mixed-filesystem cloning uses its independent A/B pair.');
    if (stateFixture(state) === 'btrfs-restore') console.log('Btrfs backup restoration uses its independent A/B pair.');
    if (stateFixture(state) === 'archive-restore') console.log('Encrypted disk backup restoration uses its independent A/B pair.');
    if (stateFixture(state) === 'capacity-restore') console.log('Capacity and file-based restoration uses its independent A/B pair.');
    if (stateFixture(state) === 'fat-restore') console.log('FAT variant restoration uses its independent A/B pair.');
    if (stateFixture(state) === 'ext-copy') console.log('Ext copy and restoration uses its independent 512-byte A/B pair.');
    if (stateFixture(state) === 'signature-scope') console.log('Signature scan scope uses its independent 4Kn A/B pair.');
    if (stateFixture(state) === 'signature-formats') console.log('Signature formats use their independent 4Kn A/B pair.');
    console.log('Partition tests may target ONLY DS-TEST-A-0001 and DS-TEST-B-0001 (16 GiB each), never the 80 GiB OS disk.');
  } finally { qmp.close(); }
}

async function ioStats() {
  const state = readState();
  if (!isAlive(state?.pid)) throw new Error('Start the test VM first');
  const qmp = await connectQmp();
  try {
    await validateProfile(qmp, await qmp.execute('query-block'), state.fourKn === true, state.sectorFault === true, stateFixture(state));
    const stats = await qmp.execute('query-blockstats');
    console.log(stats.filter(s => disks.some(d => d.id === s['node-name'])).map(s => ({
      disk: s['node-name'], readBytes: s.stats.rd_bytes, writtenBytes: s.stats.wr_bytes,
      readOperations: s.stats.rd_operations, writeOperations: s.stats.wr_operations,
    })));
  } finally { qmp.close(); }
}

async function fixtureNumberKeys(value) {
  const state = readState();
  if (!isAlive(state?.pid) || state.fourKn !== true) throw new Error('Start the 4Kn test VM first');
  const qmp = await connectQmp();
  try {
    await validateProfile(qmp, await qmp.execute('query-block'), true, state.sectorFault === true, stateFixture(state));
    const send = async keys => {
      await qmp.execute('send-key', { keys: keys.map(data => ({ type: 'qcode', data })), 'hold-time': 100 });
      await new Promise(resolve => setTimeout(resolve, 160));
    };
    await send(['ctrl', 'a']);
    for (const digit of value) await send([digit]);
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

async function surfaceReadWindow(enabled, parallel = false) {
  const state = readState();
  const fourKn = state?.fourKn === true;
  if (!isAlive(state?.pid) || fourKn === parallel || state.network !== false || stateFixture(state)) {
    throw new Error(parallel ? 'Start the isolated standard 512-byte test profile first' : 'Start the isolated standard 4Kn test profile first');
  }
  const fault = state.sectorFault === true;
  const targets = new Map([['test_a', 128 * 1024]]);
  if (parallel) targets.set('test_b', 2 * 1024 ** 2);
  const fields = ['bps', 'bps_rd', 'bps_wr', 'iops', 'iops_rd', 'iops_wr', 'bps_max', 'bps_rd_max', 'bps_wr_max', 'iops_max', 'iops_rd_max', 'iops_wr_max', 'iops_size'];
  const rateOf = (device, field) => Number(device?.inserted?.[field] ?? 0);
  const qmp = await connectQmp();
  try {
    const before = await qmp.execute('query-block');
    await validateProfile(qmp, before, fourKn, fault);
    const sources = [...targets].map(([id, limit]) => {
      const source = before.find(b => b.inserted?.['node-name'] === id);
      const disk = (fourKn ? fourKnDisks : disks).find(d => d.id === id);
      if (!source.qdev || (source.qdev !== disk.controller && !source.qdev.startsWith('/machine/'))) {
        throw new Error(`Unable to bind the read limit to ${id}`);
      }
      return { id, limit, source, disk };
    });
    for (const { id, limit, source, disk } of sources) {
      if (!fourKn) {
        const controller = source.qdev.replace(/\/(?:nvme-ns[^/]*|legacy-ns)$/, '');
        for (const [property, expected] of [['serial', disk.serial], ['logical_block_size', 512], ['physical_block_size', 512]]) {
          if (await qmp.execute('qom-get', { path: controller, property }) !== expected) {
            throw new Error(`Unexpected ${id} ${property}`);
          }
        }
      }
      if (fields.some(field => ![0, field === 'bps_rd' ? limit : 0].includes(rateOf(source, field)))) {
        throw new Error(`${id} has another throttle configured`);
      }
    }
    for (const { id, limit, source } of sources) {
      await qmp.execute('block_set_io_throttle', {
        id: source.qdev, group: `diskstudio-${id}-surface`,
        bps: 0, bps_rd: enabled ? limit : 0, bps_wr: 0, iops: 0, iops_rd: 0, iops_wr: 0,
      });
    }
    const after = await qmp.execute('query-block');
    await validateProfile(qmp, after, fourKn, fault);
    for (const previous of before.filter(b => b.inserted)) {
      const id = previous.inserted['node-name'];
      const current = after.find(b => b.inserted?.['node-name'] === id);
      if (fields.some(field => rateOf(current, field) !== (targets.has(id) && field === 'bps_rd' ? (enabled ? targets.get(id) : 0) : rateOf(previous, field)))) {
        throw new Error('Surface scan read limits could not be verified');
      }
    }
    console.log(enabled
      ? (parallel ? 'Test A/B read limits: 128 KiB/s and 2 MiB/s.' : 'Test A read limit: 128 KiB/s.')
      : (parallel ? 'Test A/B read limits cleared.' : 'Test A read limit cleared.'));
  } finally { qmp.close(); }
}

async function main() {
  const [command, ...args] = process.argv.slice(2);
  if (command === 'reallocation-capacity-keys') {
    if (args.length !== 1 || !['32', '64'].includes(args[0])) throw new Error('Choose the 4Kn fixture capacity of 32 or 64 MiB');
    return fixtureNumberKeys(args[0]);
  }
  if (command === 'sector-copy-count-keys') {
    if (args.length !== 1 || !['2', '3'].includes(args[0])) throw new Error('Choose the 4Kn fixture sector count of 2 or 3');
    return fixtureNumberKeys(args[0]);
  }
  if (!['init', 'doctor', 'smoke', 'start', 'install', 'status', 'shutdown', 'poweroff', 'login-keys', 'checkpoint', 'restore', 'refresh-tools', 'preflight', 'io-stats', 'pause-vm', 'resume-vm', 'move-stop-window', 'move-stop-clear', 'surface-read-window', 'surface-read-clear', 'surface-parallel-window', 'surface-parallel-clear'].includes(command)) {
    console.log('Usage: node scripts/vm/lab.mjs init|doctor|smoke|start|install|status|shutdown|poweroff|login-keys|refresh-tools|preflight|io-stats|pause-vm|resume-vm|move-stop-window|move-stop-clear|checkpoint NAME|restore NAME');
    console.log('start/install: TCG by default. Optional --whpx (experimental; reboot exits), --network (user-mode NAT, no shares/forwarding)');
    console.log('start --4kn: use the separate offline pair of 4096-byte NVMe fixtures');
    console.log('start --4kn --sector-copy-fault: inject a read error at one fixed Test A LBA');
    console.log('start --4kn --surface-repair-fault: inject the fixed Test A read error until a guest write completes');
    console.log('start --4kn --whole-clone: use the dedicated whole-disk clone fixtures');
    console.log('start --4kn --btrfs-clone: use the dedicated Btrfs resize clone fixtures');
    console.log('start --4kn --mixed-clone: use the dedicated mixed-filesystem clone fixtures');
    console.log('start --4kn --btrfs-restore: use the dedicated Btrfs backup restore fixtures');
    console.log('start --4kn --archive-restore: use the dedicated encrypted disk backup restore fixtures');
    console.log('start --4kn --btrfs-exact-clone: use the copied, independently verified Btrfs restore pair');
    console.log('start --4kn --capacity-restore: use the dedicated capacity and file-based restore fixtures');
    console.log('start --4kn --fat-restore: use the dedicated FAT variant restore fixtures');
    console.log('start --ext-copy: use the dedicated 512-byte Ext copy and restore fixtures');
    console.log('start --ldm [--4kn]: use the dedicated Windows dynamic disk fixtures');
    console.log('start --4kn --signature-scope: use the dedicated 4Kn signature scan fixtures');
    console.log('start --4kn --signature-formats: use the dedicated 4Kn signature format fixtures');
    console.log('reallocation-capacity-keys 32|64: enter the 4Kn fixture capacity into the focused guest field');
    console.log('sector-copy-count-keys 2|3: enter the 4Kn fixture sector count into the focused guest field');
    console.log('surface-read-window|surface-read-clear: set or clear the fixed Test A 4Kn read limit');
    console.log('surface-parallel-window|surface-parallel-clear: set or clear the fixed Test A/B 512-byte read limits');
    process.exitCode = command ? 1 : 0; return;
  }
  const allowed = ['start', 'install', 'smoke'].includes(command) ? ['--tcg', '--whpx', '--network'] : [];
  if (command === 'start') allowed.push('--4kn', '--sector-copy-fault', '--surface-repair-fault', ...fixtureProfiles.map(name => `--${name}`));
  if (!['checkpoint', 'restore'].includes(command) && args.some(a => !allowed.includes(a))) throw new Error('Unsupported option; arbitrary paths/QEMU arguments are not accepted');
  if (['checkpoint', 'restore'].includes(command) && args.length !== 1) throw new Error('Provide exactly one checkpoint name');
  if (args.includes('--tcg') && args.includes('--whpx')) throw new Error('Choose only one accelerator');
  if (command === 'doctor') return doctor();
  if (command === 'preflight') return preflight();
  if (command === 'io-stats') return ioStats();
  if (['move-stop-window', 'move-stop-clear'].includes(command)) return moveStopWindow(command === 'move-stop-window');
  if (['surface-read-window', 'surface-read-clear'].includes(command)) return surfaceReadWindow(command === 'surface-read-window');
  if (['surface-parallel-window', 'surface-parallel-clear'].includes(command)) return surfaceReadWindow(command === 'surface-parallel-window', true);
  if (['pause-vm', 'resume-vm'].includes(command)) {
    const state = readState();
    if (!isAlive(state?.pid)) throw new Error('Start the test VM first');
    const qmp = await connectQmp();
    try {
      await validateProfile(qmp, await qmp.execute('query-block'), state.fourKn === true, state.sectorFault === true, stateFixture(state));
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
    const fixtures = fixtureProfiles.filter(name => args.includes(`--${name}`));
    if (fixtures.length > 1) throw new Error('Choose one clone fixture');
    const repairFault = args.includes('--surface-repair-fault');
    if (repairFault && args.includes('--sector-copy-fault')) throw new Error('Choose one sector fault mode');
    const options = { accelerator: args.includes('--whpx') ? 'whpx' : 'tcg', setup: command === 'install', network: args.includes('--network'), fourKn: args.includes('--4kn'), sectorFault: repairFault || args.includes('--sector-copy-fault'), repairFault, fixture: fixtures[0] };
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
