# Parallels structured HDD/HDS: scope and fixtures

Only ordinary synthetic project files are involved. There is no host/guest device I/O, mounting, parent-file lookup, running-VM contact or source-file writeback. The existing image-open menu accepts explicitly selected `.hdd`/`.hds` files; package directories and XML descriptors are not opened.

## Format and limitations

The reader implements the traditional `WithoutFreeSpace` and extended `WithouFreSpacExt` structured headers (type 2). Traditional BAT addresses are 512-byte sectors; extended addresses are clusters. Capacity, the entire bounded BAT, metadata/data separation, complete physical clusters, allocation uniqueness and closed state are validated. Sparse blocks yield zero. Legacy implicit data offset, explicit offsets, closed-2.1 status, non-power-of-two sector-multiple clusters, partial logical tails and zero trailing BAT entries are supported. Conversion/unknown flags, unsupported format extensions, dirty/unknown close states, nonzero legacy reserved capacity, invalid geometry, aliases and truncation are rejected. Unreferenced data blocks are ignored with a warning.

The binary extent does not prove independence from snapshots. Snapshot relationships may be stored externally in `DiskDescriptor.xml`. **Every import therefore retains a warning** in the report and details: this is a single-file interpretation, not proof of a complete snapshot. No external names are followed. There are no header/content checksums, and rechecking header/BAT bytes before acceptance is not a point-in-time snapshot or full data validation. Flat/bundled/split/encrypted forms, snapshot composition, extensions, 4Kn, content browsing, conversion, mounting and writes remain incomplete.

Limits: logical 4 MiB–8 TiB (legacy has only a 32-bit sector count); clusters 512 B–256 MiB; BAT at most 32 MiB; logical reads 1 MiB each/4 MiB total; file reads 80 MiB/20000 calls. Logical capacity is also constrained by the BAT limit. The 8 TiB sparse test checks metadata geometry and budgets, not large-file throughput.

References: [QEMU reader and writer](https://github.com/qemu/qemu/blob/master/block/parallels.c), [OpenVZ/Parallels format declarations](https://github.com/OpenVZ/ploop/blob/master/include/ploop1_image.h), [Parallels description of external snapshot relationships](https://kb.parallels.com/br/116508/). [DiskGenius lists Parallels among supported virtual disks](https://www.diskgenius.cn/help/vmdisk.php).

## Independent fixtures

`scripts/create-parallels-fixtures.mjs` converts the existing 8 MiB synthetic MBR/two-EBR RAW fixture with the installed QEMU 10.2.0. It creates extended headers with 1 MiB and 64 KiB clusters. A third fixture packs unchanged independent data clusters in reverse order after a sector-aligned legacy header/BAT and converts addresses to sectors. QEMU decodes and compares **all logical bytes** against RAW for all three files. No Parallels Desktop-produced corpus has been tested.

The Windows QEMU `check` command crashed with heap-corruption status `0xc0000374` on the generated extended sample under both installed 10.2.0 and 11.1.0 binaries. Their complete logical reads/compares succeeded. **QEMU structural check is not counted as passed.** 7-Zip 26.06 also could not open this format. Product corruption tests provide separate structural evidence. Logs preserve the failed checker and successful compare stages; no repair was attempted. `--resume` continues fixture generation only for ordinary single-link files, verifies exact existing outputs before preserving them, and never overwrites differing files.

| Fixture | Container bytes | SHA-256 of gzip regression copy |
| --- | ---: | --- |
| `parallels-extended.gz` | 5242880 | `fe8dc7845ddb0d7acba307173845d842d2c6461d800c0b2fcbebed7e88570c42` |
| `parallels-small.gz` | 393216 | `35e24b891bf1fdef9fa07b723a4371b81aa428246e586583e58b1f17cc2275b6` |
| `parallels-legacy.gz` | 4194816 | `7f4b588358771c4825e42e442b2cd63c4f7d377f9fe315195925ea2c2bf8aac4` |

Files for native UI verification are in `.vm/parallels-fixtures`. Nine reader tests cover exact decoded windows/EBRs, all supported layouts, sparse/empty and unreferenced blocks, metadata changes, corrupt geometry/maps, short reads and budgets. Three IPC tests verify logical bytes, basename-only arguments, retained dependency warnings, 4Kn/native-sector conflicts and rejection of changed metadata. The Rust core acceptance regression includes HDD/HDS 512 B GPT and 4Kn rejection. Stage results and actual native UI evidence are recorded in `.vm/parallels-status.json`.
