#!/usr/bin/env bash
set -eu
project=$(cd "$(dirname "$0")/../.." && pwd)
export PATH="/usr/bin:$project/.vm/tools/w64devkit/bin:$PATH"
source_dir=$(cygpath -u "$1")
cd "$source_dir"
./configure --build=x86_64-w64-mingw32 --host=x86_64-w64-mingw32 \
  --disable-nls --disable-elf-shlibs --disable-fsck --disable-uuidd \
  --disable-e2initrd-helper --disable-fuse2fs --disable-defrag --disable-tdb \
  CFLAGS="-O2 -D_WIN32_WINNT=0x0601" > configure-win.log 2>&1
# configure substitutes RDYNAMIC even on mingw; patch generated makefiles too.
node "$project/scripts/ext4/patch.mjs" "$(cygpath -w "$source_dir")"
make -j8 'SHELL=C:/Program Files/Git/usr/bin/sh.exe' top-deps > build-win.log 2>&1
for part in et uuid blkid ext2fs support e2p; do
  make -j8 'SHELL=C:/Program Files/Git/usr/bin/sh.exe' -C "lib/$part" all >> build-win.log 2>&1
done
for task in 'misc mke2fs' 'resize resize2fs' 'e2fsck e2fsck'; do
  set -- $task
  make -j8 'SHELL=C:/Program Files/Git/usr/bin/sh.exe' -C "$1" "$2" >> build-win.log 2>&1
done
