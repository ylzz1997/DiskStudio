# VHDX fixtures

These are ordinary synthetic files, not VM disks or user data. The generator is
`scripts/create-vhdx-fixtures.mjs`; uncompressed files remain in `.vm/vhdx-fixtures`.
It refuses existing output files and does not start/contact a VM, mount anything,
or open devices. All three logical images are 8,388,608 bytes with 1 MiB blocks.

| Compressed fixture | Container length | Logical layout | Independent check |
| --- | ---: | --- | --- |
| `vhdx-qemu-fixed.gz` | 19,922,944 | 512 B, MBR + two EBRs | QEMU check and byte compare |
| `vhdx-qemu-dynamic.gz` | 11,534,336 | 512 B, MBR + two EBRs | QEMU check and byte compare |
| `vhdx-qemu-gpt4k.gz` | 10,485,760 | 4096 B, GPT primary + backup | 7-Zip logical extraction and byte compare |

QEMU 10.2.0 `qemu-img convert -f raw -O vhdx` generated all containers. The two
512 B images use the existing `vhd-qemu-raw.gz` source. The generator builds the
4Kn source in memory (`vhdx-gpt4k-raw.gz`), then changes only the logical/physical
sector-size metadata values in its new QEMU output to 4096. QEMU's VHDX writer
does not expose a sector-size option and its reader rejects 4Kn. Therefore the
4Kn variant was extracted to memory with existing 7-Zip 26.06 (`x -so -tVHDX`)
and compared with all 8 MiB of the raw source. Do not describe this as QEMU 4Kn
validation or as a Windows/Hyper-V generated corpus.

Compressed file SHA-256 values:

- fixed: `dd915bd5900620bcca4a376d19dbdc1c7c5af05cead2d16191018f270b6c6cd0`
- dynamic: `035a0d34b1f26d9b6db0f639d222d39b7d45f83cec0478b6f6edf83935327f2e`
- gpt4k: `17e2c57198fcfb097c3fdeec4e33d138aeaa88b72956cd41a4380f1a9af7dc8e`
- gpt4k raw: `90b88cd6477ba91fc956f7e946d6f148a91aa54a648eebd7607f3462c6fdcf43`

`vhdx-data.ts` decompresses copies for memory tests and uses an independent
bitwise CRC-32C routine to construct mutations. Tests compare logical windows,
EBRs and block boundaries to raw bytes, check both header/region copies, large
sequence numbers, bitmap insertion boundaries without allocating GiB buffers,
zero policies, changed metadata, corrupt lengths/states/overlaps and budgets.

Supported product subset: standalone VHDX v1 fixed/dynamic, 512 B logical sectors
with GPT/MBR/EBR or 4096 B sectors with GPT. Logical size is 4 MiB–8 TiB, block size
1–256 MiB (power of two), used BAT at most 16 MiB. Both standard 512/4096 physical
sector values are accepted independently. The reader requires the BAT and metadata
regions even when QEMU sets their Required flag to zero; unknown required regions
or metadata are rejected. A single valid redundant header/region copy is accepted
with a visible warning. Equal valid header sequence numbers or inconsistent valid
region tables are rejected. No source repair is attempted.

Differencing chains, nonzero LogGuid/log replay, partial-present blocks, 4Kn MBR,
truncated physical tail blocks and other unsupported layouts are rejected. Empty,
undefined and unmapped logical blocks are zero-filled; the UI explains this when
such blocks are read. This view does not recover stale data. Metadata and BAT are
reread before acceptance; this is not a snapshot or whole-content checksum.
No product creation, conversion, mount, writeback or filesystem-content access.

Limits: 4 MiB total logical reads, 1 MiB per logical request, 40 MiB cumulative
file reads, 16 MiB per file slice, 4096 slices. Only bounded logical head/tail and
EBR bytes cross existing IPC; file paths and parent paths never do.

References: [MS-VHDX](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-vhdx/39d641c2-093c-4d4a-8c9d-bd4b9fc2ff31),
[QEMU VHDX implementation](https://github.com/qemu/qemu/blob/master/block/vhdx.c).
