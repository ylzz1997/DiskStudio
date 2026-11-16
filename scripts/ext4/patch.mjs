import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
const project = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '../..');
const source = process.argv[2] ? path.resolve(process.argv[2]) : path.join(project, '.vm/ext4-source/e2fsprogs-1.47.4');
if (!source.startsWith(path.join(project, '.vm/ext4-source') + path.sep)) throw Error('Source must stay inside the isolated build directory');
function edit(name, transform) {
  const filename = path.join(source, name);
  const original = fs.readFileSync(filename, 'utf8');
  const result = transform(original);
  if (result !== original) fs.writeFileSync(filename, result);
}
fs.copyFileSync(path.join(project, 'scripts/ext4/bounded_io.c'), path.join(source, 'lib/ext2fs/windows_io.c'));
for (const file of ['lib/ext2fs/Makefile.in', 'lib/ext2fs/Makefile']) {
  if (fs.existsSync(path.join(source, file))) edit(file, s => s.replace(/^[ \t]*(getsize|getsectsize|ismounted)\.o \\\r?\n/gm, '').replace(/\\\r?\n(?:\r?\n)+(?=\t)/g, '\\\n'));
}
for (const file of ['resize/main.c', 'e2fsck/unix.c', 'e2fsck/journal.c']) {
  edit(file, s => s.replaceAll('unix_io_manager', 'default_io_manager'));
}
// Native Windows has signal(), but no POSIX sigaction or SIGUSR signals.
edit('e2fsck/unix.c', s => s.replaceAll('#ifdef HAVE_SIGNAL_H', '#if defined(HAVE_SIGNAL_H) && !defined(_WIN32)'));
edit('e2fsck/unix.c', s => s.includes('#define NAME_MAX 255') ? s : s.replace('#include "config.h"', '#include "config.h"\n#ifndef NAME_MAX\n#define NAME_MAX 255\n#endif'));
for (const file of ['e2fsck/ehandler.c', 'e2fsck/message.c', 'e2fsck/problem.c', 'e2fsck/util.c', 'resize/resource_track.c']) {
  edit(file, s => {
    for (const header of ['termios.h', 'sys/resource.h']) if (!s.includes(`#ifndef _WIN32\n#include <${header}>`)) s = s.replace(`#include <${header}>`, `#ifndef _WIN32\n#include <${header}>\n#endif`);
    return s;
  });
}
for (const file of ['resize/resource_track.c', 'e2fsck/util.c']) {
  edit(file, s => s.includes('ds_resource_marker') ? s : s.replace('#include "config.h"', '#include "config.h"\n#ifdef _WIN32\nstatic char ds_resource_marker;\n#define sbrk(unused) (&ds_resource_marker)\n#endif'));
}
edit('e2fsck/pass5.c', s => s.includes('#ifdef HAVE_SYS_IOCTL_H') ? s : s.replace('#include <sys/ioctl.h>', '#ifdef HAVE_SYS_IOCTL_H\n#include <sys/ioctl.h>\n#endif'));
for (const file of ['e2fsck/Makefile.in', 'e2fsck/Makefile']) if (fs.existsSync(path.join(source,file))) edit(file, s => s.replaceAll('-rdynamic', ''));
edit('lib/support/sort_r.h', s => s.replace('defined __MINGW32__ || ', ''));
edit('e2fsck/unix.c', s => s.replaceAll('"/dev/null"', '"NUL"'));
edit('misc/mke2fs.c', s => s.includes('diskstudio_utf8_label') ? s : s.replace('volume_label = optarg;', 'volume_label = diskstudio_utf8_label(optarg);').replace('static char *volume_label;', 'extern char *diskstudio_utf8_label(const char *);\nstatic char *volume_label;'));
// No daemon log files or POSIX crash handlers: the parent captures both pipes.
fs.writeFileSync(path.join(source, 'e2fsck/logfile.c'), '#include "config.h"\n#include "e2fsck.h"\nvoid set_up_logging(e2fsck_t ctx) { (void)ctx; }\n');
fs.writeFileSync(path.join(source, 'e2fsck/sigcatcher.c'), 'void sigcatcher_setup(void) {}\n');
console.log('Applied bounded Windows adapter and portability patches');
