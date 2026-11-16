# VHD fixtures

The original standalone-reader scope below is historical. Selected VHD/AVHD
parent chains are now documented in [VHD-CHAIN-FIXTURES.md](VHD-CHAIN-FIXTURES.md).

`vhd-qemu-{fixed,dynamic,raw}.gz` are compressed ordinary synthetic fixture files,
not VM disks or user data. `scripts/create-vhd-fixtures.mjs` builds an 8 MiB memory
buffer with an MBR extended partition and two EBRs, then uses the already installed
QEMU 10.2.0 `qemu-img convert -f raw -O vpc -o subformat=...,force_size=on` offline.
The fixed file is 8,389,120 bytes; the dynamic file is 4,197,888 bytes. Both expose
the same 8,388,608 logical bytes. No VM is started/contacted, and nothing is mounted.
Uncompressed UI fixtures are in `.vm/vhd-fixtures/`; generation refuses overwrites.

`vhd-memory.ts` is a separate minimal memory writer for malformed-input and mapping
tests. It deliberately reverses physical allocation order, and can vary block size,
sector presence bits, and unallocated blocks. It is not included in the app bundle.

References: [Microsoft VHD specification](https://www.microsoft.com/en-us/download/details.aspx?id=23850),
[QEMU VHD implementation](https://github.com/qemu/qemu/blob/master/block/vpc.c).

Supported import subset: VHD v1 fixed and standalone dynamic, 512-byte sectors,
4 MiB–2040 GiB logical size, power-of-two blocks 512 B–256 MiB, BAT at most 4 MiB.
Footer copies must match; checksums, allocation extents and metadata non-overlap
must pass. Reject differencing parents, saved-state disks, unsupported fields,
old 511-byte footers, and ambiguous legacy CHS capacity. No creation, conversion,
mounting, filesystem access or repair is provided by the product. VHDX has a
separate limited reader documented in `VHDX-FIXTURES.md`; other containers remain pending.

Only logical head/tail windows and requested EBR sectors are sent to Rust. File
paths never cross IPC. Metadata/used bitmaps are reread before acceptance; this is
not a snapshot or full content checksum. Import an offline copy. Resource ceilings:
4 MiB logical reads, 4 MiB cached bitmaps, 32 MiB total file reads, 20,000 reads.
