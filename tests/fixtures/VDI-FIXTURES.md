# VDI fixtures

`vdi-qemu-fixed.gz` and `vdi-qemu-dynamic.gz` contain ordinary synthetic files.
`scripts/create-vdi-fixtures.mjs` uses the existing 8 MiB MBR/EBR source
`vhd-qemu-raw.gz` and QEMU 10.2.0 offline conversion (`-O vdi -o static=on/off`).
It refuses overwrites and does not start/contact a VM, mount or open a device.
Uncompressed files stay in `.vm/vdi-fixtures` for native UI checks.

| Fixture | Container bytes | Allocated blocks | SHA-256 of compressed file |
| --- | ---: | ---: | --- |
| fixed | 8,389,632 | 8 | `19caf12247cf0cc79458cb8ba8279b498f5e17f7d9dae7f04ead109e1f1bf0a3` |
| dynamic | 3,146,752 | 3 | `41a94a5e736f656325ea55a6f361b32ac85ab6e280dcf83432ee48556a9dbc30` |

Both expose exactly 8,388,608 logical bytes. Independent `qemu-img check` and
`compare -f raw -F vdi` passed. These are QEMU-created VDI 1.1 files, not a
VirtualBox-created corpus. `vdi-memory.ts` independently builds small byte arrays
for mutations, reversed allocation order, sparse/free/zero entries, 1.1+ geometry,
per-block extra data and partial logical tails. None is included in the app bundle.

The product imports standalone VDI 1.1 and 1.1+ fixed/dynamic images, with 384/400
byte headers and 512 B sectors. It checks type, flags, UUID presence, capacity,
header/map/data separation, complete allocated extents, unique slot ownership
and agreement of the allocated count. Physical blocks need not follow logical
order. Per-block prefixes are skipped. Free and zero entries return zero; free
entries add a detail note. Extra trailing file bytes produce a visible warning
and are never used as logical disk contents.

VDI metadata has no checksum, so structural validity does not establish content
integrity. Header and full map are reread before acceptance to detect change;
this is not a snapshot or full-content digest. Paths never enter IPC, no parent
lookup or automatic raw-device access is added, and the selected file stays
read-only. Only bounded logical head/tail/EBR bytes go to the existing inspector.

Current limits: logical size 4 MiB–8 TiB; blocks 512 B–256 MiB, power of two;
per-block prefix zero or a power of two from 512 B–1 MiB; map 32 MiB; cumulative
logical reads 4 MiB (1 MiB/request); cumulative file slices 80 MiB/20,000 calls.
An 8 TiB all-free disk is tested with a 32 MiB map, without a TiB allocation.
The full map limit can restrict logical capacity for smaller block sizes.

Old VDI versions, unknown header extensions/features, undo/differencing chains,
4Kn, incomplete fixed allocation and non-sector-aligned capacities are rejected.
No creation, conversion, mounting, repair or partition-content access is supplied
by this import feature. Those parity goals remain separate and unfinished.

References consulted for the format, without copying implementation code:
[Oracle VDI structures](https://github.com/mirror/vbox/blob/master/src/VBox/Storage/VDICore.h),
[Oracle VDI implementation](https://github.com/mirror/vbox/blob/master/src/VBox/Storage/VDI.cpp),
[QEMU VDI implementation](https://github.com/qemu/qemu/blob/master/block/vdi.c).
