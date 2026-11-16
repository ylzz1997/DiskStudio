# e2fsprogs 1.47.4 used by DiskStudio

The native Windows helpers are separate processes, embedded as resources in the
desktop executable. They are built from the unmodified upstream release archive
included here, plus the reproducible adapter/portability changes in
`scripts/ext4/patch.mjs` and `scripts/ext4/bounded_io.c`.

Upstream source: https://mirrors.edge.kernel.org/pub/linux/kernel/people/tytso/e2fsprogs/v1.47.4/e2fsprogs-1.47.4.tar.xz

SHA-256: `fd5bf388cbdbe006a3d3b318d983b2948382440acc85a87f1e7d108653e8db0b`.

`NOTICE` includes the upstream notices and GPL version 2 text. Upstream libraries
also use LGPL, BSD and MIT terms as described in that notice and source headers;
`libuuid-COPYING` is preserved separately. Keep the archive, all adapter source,
configuration, build scripts and notices together with any distributed helpers.

Rebuild with `pwsh -NoProfile -File scripts/ext4/build.ps1`. The pinned compiler is
w64devkit 2.11.0, SHA-256
`54c2bb9962e8a30c8270e34c5efc3d5fee579f53087fa7d435b95fbba2af7b2b`.
Downloads and intermediate builds stay in `.vm/`; generated executables and
configuration in `src-tauri/ext4-engine/` are compiled into DiskStudio.

The adapter accepts only the literal name `diskstudio`, an inherited file handle,
and a checked partition extent. It does not accept/open device paths. It enforces
read-only mode and extent boundaries, explicitly rejects discard/TRIM, handles
unaligned filesystem requests with sector-aligned read/modify/write, and exposes
EOF to interactive prompts. Native labels use hexadecimal UTF-8 arguments to
avoid Windows code-page conversion. POSIX daemon logging and signal handlers are
disabled; DiskStudio captures bounded output from both pipes.

Runtime extraction uses a unique temporary directory and verifies embedded bytes;
files are held read-only through execution and removed when the operation ends.
