import fs from 'node:fs';
import path from 'node:path';
import crypto from 'node:crypto';
import net from 'node:net';
import { spawn, spawnSync } from 'node:child_process';
import { project, root, safePath, checkQcowHeader } from './lab.mjs';
import { verifyUbuntu } from './ubuntu-qmp.mjs';
const base = path.join(root, 'ubuntu');
const uuid = '25c39913-ac45-4445-aa54-bdd0dbca7001';
const os = path.join(base, 'system.qcow2');
const btrfsImages = ['btrfs-snapshot-image', 'btrfs-retained-image', 'btrfs-checksum-images', 'btrfs-members-images', 'btrfs-parity-images', 'btrfs-allocation-image'];
const mdRaid6 = 'virtual-raid-md6-4kn';
const mdRaidMetadata = 'virtual-raid-md-metadata-4kn';
const mdRaidZones = 'virtual-raid-md-zones-4kn';
const mdRaid10 = 'virtual-raid-md10-4kn';
const mdFixtures = [mdRaid6, mdRaidMetadata, mdRaidZones, mdRaid10, 'virtual-raid-md10-advanced-4kn'];
const imageFixtures = [...btrfsImages, 'lvm-images-4kn', 'lvm-raid-matrix-4kn', 'lvm-thin-snapshots-4kn', 'lvm-cow-persistent-4kn', 'lvm-cross-deleted-4kn', 'lvm-raid10-layouts-4kn', ...mdFixtures];
const phases = ['populate', 'verify', 'ext-populate', 'ext-verify', 'btrfs-verify', 'btrfs-move-verify', 'btrfs-sha256-verify', ...imageFixtures];
const alive = pid => { try { process.kill(pid, 0); return true; } catch { return false; } };
function stopped() {
  for (const file of [path.join(root, 'run.json'), path.join(base, 'run.json')]) {
    if (fs.existsSync(file) && alive(JSON.parse(fs.readFileSync(safePath(file), 'utf8')).pid)) throw Error('Shut down both VMs before changing or reattaching disk files');
  }
}
function tool(name, args) {
  const result = spawnSync(safePath(path.join(root, 'tools/qemu', name)), args, { windowsHide: true, encoding: 'utf8', timeout: 120000 });
  if (result.error || result.status !== 0) throw Error(result.error?.message || result.stderr);
  return result.stdout;
}
async function main() {
  if (process.argv[2] === 'stop') {
    const socket = net.createConnection({ host: '127.0.0.1', port: 49366 });
    let buffer = '', step = 0;
    await new Promise((resolve, reject) => {
      socket.setTimeout(8000, () => { socket.destroy(); reject(Error('QMP timeout')); });
      socket.on('error', reject);
      socket.on('data', chunk => {
        buffer += chunk;
        let index;
        while ((index = buffer.indexOf('\n')) >= 0) {
          const line = JSON.parse(buffer.slice(0, index)); buffer = buffer.slice(index + 1);
          if (line.event) continue;
          if (line.error) { socket.destroy(); reject(Error(JSON.stringify(line.error))); return; }
          if (step === 0 && line.QMP) socket.write(JSON.stringify({ execute: 'qmp_capabilities' }) + '\n');
          else if (step === 1) socket.write(JSON.stringify({ execute: 'query-name' }) + '\n');
          else if (step === 2 && line.return?.name === 'DiskStudio-Ubuntu') socket.write(JSON.stringify({ execute: 'query-uuid' }) + '\n');
          else if (step === 3 && line.return?.UUID === uuid) socket.write(JSON.stringify({ execute: 'quit' }) + '\n');
          else if (step === 4) { socket.end(); resolve(); }
          else { socket.destroy(); reject(Error('Unexpected QMP identity')); }
          step++;
        }
      });
    });
    return;
  }
  const command = process.argv[2];
  if (command?.startsWith('seed-') && phases.includes(command.slice(5))) {
    const state = path.join(base,'run.json');
    if (fs.existsSync(state) && alive(JSON.parse(fs.readFileSync(safePath(state),'utf8')).pid)) throw Error('Ubuntu must be stopped before preparing seed media');
    const phase = command.slice(5);
    const stage = safePath(path.join(base, `seed-${phase}`), root, false);
    fs.mkdirSync(stage);
    const mdFixture = mdFixtures.includes(phase);
    const scriptName = mdFixture ? 'ubuntu-md6.py' : phase.startsWith('lvm-') ? 'ubuntu-lvm.py' : phase.startsWith('btrfs-') ? 'ubuntu-btrfs.py' : phase.startsWith('ext-') ? 'ubuntu-ext.py' : 'ubuntu-ext4.py';
    const script = fs.readFileSync(path.join(project, 'scripts/vm', scriptName)).toString('base64');
    const seedRevision = mdFixture ? crypto.createHash('sha256').update(script).digest('hex').slice(0, 12) : '1';
    fs.writeFileSync(path.join(stage, 'meta-data'), `instance-id: diskstudio-ext4-${phase}-${seedRevision}\nlocal-hostname: diskstudio-ubuntu\n`);
    fs.writeFileSync(path.join(stage, 'user-data'), `#cloud-config
users: []
ssh_pwauth: false
disable_root: true
write_files:
  - path: /opt/diskstudio-ext4-checks.py
    permissions: '0700'
    encoding: b64
    content: ${script}
runcmd:
  - [sh, -c, 'python3 /opt/diskstudio-ext4-checks.py ${phase} > /dev/ttyS0 2>&1; sync']
power_state:
  mode: poweroff
  delay: now
  timeout: 30
`);
    const result = spawnSync('pwsh', ['-NoProfile', '-File', path.join(project,'scripts/vm/make-ubuntu-seed.ps1'), '-Phase', phase], {windowsHide:true,encoding:'utf8'});
    if (result.status !== 0) throw Error(result.stderr);
    console.log(`Immutable Ubuntu ${phase} seed prepared`);
    return;
  }
  stopped();
  if (process.argv[2] === 'init') {
    const source = safePath(path.join(base, 'noble-mirror.img'));
    const h = crypto.createHash('sha256');
    for await (const chunk of fs.createReadStream(source)) h.update(chunk);
    if (h.digest('hex') !== '04e8f2461a16a44e78a370284471f29616cdd9baf1893df25807b71f985b6c94') throw Error('Ubuntu image hash mismatch');
    checkQcowHeader(source);
    safePath(os, root, false);
    if (fs.existsSync(os)) throw Error('Ubuntu OS already exists');
    tool('qemu-img.exe', ['convert', '-f', 'qcow2', '-O', 'qcow2', source, os]);
    tool('qemu-img.exe', ['resize', '-f', 'qcow2', os, '16G']);
    const stage = safePath(path.join(base, 'seed'), root, false);
    fs.mkdirSync(stage);
    fs.writeFileSync(path.join(stage, 'meta-data'), 'instance-id: diskstudio-ext4-probe-1\nlocal-hostname: diskstudio-ubuntu\n');
    fs.writeFileSync(path.join(stage, 'user-data'), `#cloud-config
users: []
ssh_pwauth: false
disable_root: true
network:
  config: disabled
runcmd:
  - [sh, -c, 'echo DISKSTUDIO_UBUNTU_BEGIN > /dev/ttyS0; uname -a > /dev/ttyS0; cat /sys/class/dmi/id/product_uuid > /dev/ttyS0; e2fsck -V > /dev/ttyS0 2>&1; resize2fs -V > /dev/ttyS0 2>&1; python3 --version > /dev/ttyS0; lsblk -b -o NAME,SIZE,SERIAL,FSTYPE > /dev/ttyS0; echo DISKSTUDIO_UBUNTU_READY > /dev/ttyS0']
power_state:
  mode: poweroff
  delay: now
  timeout: 30
`);
    const result = spawnSync('pwsh', ['-NoProfile', '-File', path.join(project, 'scripts/vm/make-ubuntu-seed.ps1')], { windowsHide: true, encoding: 'utf8' });
    if (result.status !== 0) throw Error(result.stderr);
    console.log('Ubuntu OS and immutable offline NoCloud seed prepared');
    return;
  }
  if (command !== 'probe' && !phases.includes(command)) throw Error('Unknown Ubuntu verification phase');
  const attachesTests = command !== 'probe' && !imageFixtures.includes(command);
  if (checkQcowHeader(os) !== 16 * 1024 ** 3) throw Error('Unexpected Ubuntu OS capacity');
  const log = safePath(path.join(base, `${command}-${Date.now()}.log`), root, false);
  if (fs.existsSync(log)) throw Error('Preserve prior probe log before another run');
  const args = ['-name', 'DiskStudio-Ubuntu', '-uuid', uuid, '-L', safePath(path.join(root, 'tools/qemu/share')),
    '-machine', 'q35,vmport=off,hpet=off', '-accel', 'tcg,thread=single', '-icount', 'shift=3,sleep=off', '-cpu', 'max', '-smp', '2', '-m', '2048',
    '-S', '-nodefaults', '-no-user-config', '-display', 'none', '-monitor', 'none', '-parallel', 'none', '-nic', 'none',
    '-serial', `file:${log}`, '-qmp', 'tcp:127.0.0.1:49366,server=on,wait=off',
    '-drive', `file=${safePath(os)},format=qcow2,if=none,id=ubuntu_os`, '-device', 'virtio-blk-pci,drive=ubuntu_os,serial=DS-UBUNTU-OS-0001',
    '-drive', `file=${safePath(path.join(base, command === 'probe' ? 'seed.iso' : `seed-${command}.iso`))},format=raw,media=cdrom,readonly=on`];
  if (attachesTests) {
    for (const [name, serial, id] of [['test-a.qcow2','DS-TEST-A-0001','test_a'],['test-b.qcow2','DS-TEST-B-0001','test_b']]) {
      const fourKn = command.startsWith('btrfs-');
      const file = safePath(path.join(root, 'windows', fourKn ? name.replace('.qcow2', '-4kn.qcow2') : name));
      if (checkQcowHeader(file) !== 16*1024**3) throw Error('Unexpected test disk capacity');
      const geometry = fourKn ? ',logical_block_size=4096,physical_block_size=4096' : '';
      const options = {driver:'qcow2','node-name':id,file:{driver:'file',filename:file}};
      if (fourKn) {
        options['read-only'] = true;
        options.file['read-only'] = true;
      }
      args.push('-blockdev',JSON.stringify(options),'-device',`nvme,drive=${id},serial=${serial}${geometry}`);
    }
  }
  const child = spawn(safePath(path.join(root, 'tools/qemu/qemu-system-x86_64.exe')), args, { detached: true, windowsHide: true, stdio: 'ignore' });
  child.unref();
  fs.writeFileSync(path.join(base, 'run.json'), JSON.stringify({ pid: child.pid, uuid, args, started: new Date().toISOString() }, null, 2));
  // Keep the guest paused until the live QMP inventory is verified. A failure
  // leaves it paused for inspection; no automatic force-stop or mutation retry.
  for (let attempt = 0; ; attempt++) {
    try { await verifyUbuntu(true); break; }
    catch (error) {
      if (error.code !== 'ECONNREFUSED' || attempt >= 39) throw error;
      await new Promise(resolve => setTimeout(resolve, 250));
    }
  }
  console.log(`Ubuntu ${command} started (${child.pid}); ${attachesTests ? 'virtual OS and allowlisted test A/B' : 'only its virtual OS disk'} attached; log ${log}`);
}
main().catch(e => { console.error(e.message); process.exitCode = 1; });
