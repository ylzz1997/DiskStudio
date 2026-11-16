# VHD / AVHD selected parent chains

The existing image picker now accepts `.avhd` as well as `.vhd`. A differencing
image requests its parent files in a separate multi-file picker. Only explicitly
selected `File` objects are read. Stored parent paths are never opened or searched.
Parents are matched by the footer UUID, not by filename or selection order. Each
link must match capacity and the parent's file modification time (whole seconds
since 2000, as recorded in the child header). There is no timestamp bypass. A copy
that loses its modification time is rejected even if its contents might be valid.

Supported: VHD v1 fixed/dynamic bases and differencing layers, 512-byte sectors,
4 MiB–2040 GiB logical disks, up to 16 layers including the selected child. Missing
blocks and clear sector-bitmap bits inherit from the parent; present zero sectors
override the parent. The UI exposes only the resulting partition map/table and
checks, not filesystem browsing, recovery, mounting, editing or conversion.

Header/footer/BAT checks, metadata/locator/data non-overlap and bounded reads apply
to every used layer. Windows W2ru/W2ku locators are accepted as inert metadata;
their reserved space is byte-counted, 512-byte aligned, and at most 64 KiB each.
Old sector-counted locator variants, Mac locators, malformed UTF-16 names, saved
states, old 511-byte footers and ambiguous legacy CHS sizes are rejected. Existing
standalone VHD limits still apply. This does not add VHDX, VMDK or VDI parent chains.

Shared limits: 16 MiB BATs, 32 MiB captured metadata, 16 MiB accessed bitmaps,
128 MiB file reads and 100,000 file reads. Each layer additionally allows 4 MiB
BAT/bitmap storage, 32 MiB file reads and 20,000 reads. The logical partition-table
read budget is 4 MiB, with at most 1 MiB per request. Used metadata and accessed
bitmaps are reread before accepting an import. UUID, timestamps and checksums do
not authenticate content or provide a point-in-time snapshot; use a complete
offline copy. No filesystem/device paths cross Tauri IPC.

## Independent fixtures

`scripts/create-vhd-chain-fixtures.ps1` invokes the pinned **DiscUtils.Vhd 0.16.13**
.NET generator under `scripts/vm/vhd-fixtures`. Packages, build output, CLI/cache
files and uncompressed fixtures are under `.vm`; compressed regression fixtures
are in this directory. The generator refuses existing output and fixture files.

An 8 MiB synthetic RAW with two EBRs is written to a dynamic base. Two children
override sectors at block boundaries and the end of the disk; an explicit zero
sector replaces nonzero parent data. The MBR and EBRs inherit through the chain.
Every VHD/AVHD file has modification time **2026-01-02T03:04:05.000Z**. DiscUtils
then independently decodes all 8 MiB from explicit, ordered read-only streams and
compares the result byte-for-byte with a separately patched expected RAW. The
stored `C:\SyntheticFixtures\...` parent paths are deliberately never accessed.
The application reader is separately checked against these expected bytes within
its partition-inspection budget. This is not a Hyper-V/Virtual PC real-image corpus.

| Uncompressed file | Bytes | SHA-256 |
| --- | ---: | --- |
| base.vhd | 8,393,216 | 39c5e64b2f273a1aad0a4d9b83565204b2bc46ea8c77208644bf662a58d42252 |
| middle.avhd | 4,198,912 | 067fe80dbf3df44a4b85b4069a8205e2e18dd0068164f7bb8004a73d66e8a507 |
| leaf.avhd | 6,296,576 | 656a29a6b753e12845c94df3a87b6c308e7fcc53a70d79fb3129c96499f9c16f |
| expected.raw | 8,388,608 | f4844d9b1fb0b68446eeaf75584b0b0101535e8a60303e08af259d288454f155 |

Nine chain-reader tests cover inheritance/zero overrides/partial sectors,
missing and renamed/same-name parents, UUID/timestamp/capacity failures,
cycles/depth, malformed/overlapping locators, changed metadata/bitmaps, fixed
bases and cumulative budgets. Two backend tests verify merged AVHD partition
bytes, filename-only IPC and rejection before IPC for missing/stale parents.
The Rust core's existing image tests also exercise the `.avhd` suffix.

The release native UI was checked with the leaf and both parents selected in
reverse order: two EBRs/two logical partitions, 8 MiB logical capacity, 18.01 MiB
combined file length and three layers. All detail notes were readable. All four
source files remain byte-identical to their gzip originals, and all three VHD
modification times remain unchanged. This host check used ordinary synthetic
project files only. VM device acceptance is recorded separately.

References: [Microsoft VHD specification](https://www.microsoft.com/en-us/download/details.aspx?id=23850),
[DiscUtils VHD implementation](https://github.com/DiscUtils/DiscUtils/tree/master/Library/DiscUtils.Vhd),
[VirtualBox VHD implementation](https://github.com/VirtualBox/virtualbox/blob/main/src/VBox/Storage/VHD.cpp).
VirtualBox documents the Windows byte-counted locator-space convention, which
differs from the sector wording in the older specification.
