# VMDK fixtures and supported scope

These are synthetic ordinary files, not copies of any user/VM/physical disk. No running VM was contacted. Generation scripts write new files exclusively and refuse existing output names. Assets are under `.vm/vmdk-fixtures`, with small compressed regression copies here.

`scripts/create-vmdk-fixtures.mjs` uses the existing QEMU 10.2.0 `qemu-img.exe` offline to convert an 8 MiB synthetic RAW MBR with two EBRs. Deterministic nonzero data near the beginning/end exercises actual compressed decoding. All three variants passed QEMU `check` and complete logical byte `compare` against RAW:

| Fixture | Container bytes | Header | Layout |
| --- | ---: | --- | --- |
| `vmdk-qemu-sparse.gz` | 393216 | v1, flags 3 | monolithicSparse, redundant GD/GT |
| `vmdk-qemu-zero.gz` | 393216 | v2, flags 7 | monolithicSparse, zero-grain flag enabled |
| `vmdk-qemu-stream.gz` | 134656 | v3, flags 0x30003 | streamOptimized, compressed grains with front GD/GT |
| `vmdk-footer.gz` | 139776 | v3, flags 0x30001 | Repacked stream with end GT/GD/footer/EOS markers |

The installed QEMU version produces front-index compressed files, without footer/EOS metadata markers. `scripts/create-vmdk-footer-fixture.mjs` repackages those independently produced compressed grains according to the documented footer layout; QEMU independently checks and compares the resulting complete logical contents. This is a synthetic footer fixture, not a VMware-created corpus. `vmdk-zlib-grain.bin` is an actual compressed grain extracted from the QEMU output; Rust decoding must equal the RAW first 64 KiB.

## Accepted subset

- Hosted sparse magic and headers v1/v2/v3 with a supported flag allowlist; clean close state and valid newline detector when present.
- One embedded descriptor with independent `parentCID=ffffffff`, matching capacity, and one RW/RDONLY SPARSE extent. Embedded basenames are descriptive metadata only, never paths to open. Parent/other external references, device paths, multiple extents, encryption and unknown structural fields are rejected.
- `monolithicSparse` and `streamOptimized`, 512 B logical sectors. Only GPT/MBR/EBR partition metadata is displayed; filesystem contents/usage are not inferred.
- Primary/redundant directory and grain-table agreement, metadata/data range and overlap checks, nonsequential grain order, sparse/explicit-zero grains.
- Compressed front indexes and standard footer indexes. Footer layouts require matching header geometry, typed metadata markers and EOS. Zlib blocks are decoded through bounded Rust byte-only IPC with exact length, full input consumption, checksum and stream-end checks.
- All captured headers, descriptors, indexes, footer markers and accessed compressed-grain headers are reread before acceptance. This is not an immutable snapshot or full-content verification.

## Bounds and remaining exclusions

Logical capacity 4 MiB–8 TiB; grain size power of two 8 KiB–256 MiB, compressed grain size at most 1 MiB with integral logical grain count; 512 entries per grain table; descriptor at most 64 KiB; one complete map at most 32 MiB; captured metadata at most 66 MiB; physical reads at most 160 MiB/100000 calls; logical reads at most 1 MiB per request/4 MiB total; cumulative decompression at most 64 MiB and cache at most 8 MiB. The decompressor allocates only the expected grain plus one overrun-detection byte.

Directory/table checks do not prove data integrity. Only compressed grains needed for partition metadata are decoded and checked. Front-index files do not have an EOS-based completeness proof. There is no repair, mounting, full-file scan, creation/conversion or content export. Flat/split/VMFS/SEsparse/raw-device-map/parent-chain/encrypted forms and 4Kn remain unsupported. Compatibility with VMware-generated and large real-world corpora is not yet established. The 8 TiB sparse memory test is a geometry/bounds check, not a throughput test.

## Regression evidence

10 reader tests cover all four fixtures, exact head/tail/EBR bytes, reordered grains, multi-directory boundaries, zero/sparse data, 8 TiB sparse geometry, dirty/unsupported headers, unsafe numbers, descriptor dependencies and aliases, conflicting redundancy, overlaps, truncated reads, footer markers, compressed LBA/length/checksum, cache reuse, metadata changes and logical read limits. Three IPC tests cover exact decoded data, byte-only decompression arguments, rejected parents, native sector mismatch and decoder failures. The Rust core format acceptance test now includes VMDK 512 B GPT and rejects 4Kn. Three Rust decompressor tests cover independent QEMU bytes, bounds, checksum, truncation, concatenation, trailing bytes and expansion beyond declared length.

Initial test assertion compared a Uint8Array with a Node Buffer by type-sensitive deep equality; changed to exact linear byte comparison. Final full-suite results are recorded in `.vm/vmdk-status.json`.

SHA-256 of compressed regression files:

| File | SHA-256 |
| --- | --- |
| `vmdk-raw.gz` | `84249846b9351d897b36c868f72927d0f2a74bff3dba00eb773b5599cb68e157` |
| `vmdk-qemu-sparse.gz` | `13b582370db30f2640fc39b5726bed005725f67529f5601a64b974ce938d9975` |
| `vmdk-qemu-zero.gz` | `b7121e775d4ada0c539e1494850cb1dcd5cad9fa1c0fd7a8705e5642c7cc5794` |
| `vmdk-qemu-stream.gz` | `1cb81cf420351d17755e16c40d34355bc6f068e0d5a6e93cc894e4fc2e1bb711` |
| `vmdk-footer.gz` | `8fdeb7fad66ebe46d1b27ff838ebfcc7c7bdcf4b1e786b9b45878a48f8d48525` |
| `vmdk-zlib-grain.bin` | `d5a8fd914d3c672d1973f0354243f3bd81d616aab9920c00b5663635d49c8a8f` |

Format references: [QEMU VMDK implementation](https://github.com/qemu/qemu/blob/master/block/vmdk.c), [VMware Virtual Disk Format 1.1 technical note, preserved mirror](https://www.efda.gov.et/wp-content/uploads/2019/03/vmdk_specs.pdf). The old VMware original and Broadcom replacement were unavailable during research; the mirrored document is VMware-authored, not the host's specification.

Later extension: external flat/split base descriptors are now implemented through explicit companion selection; see [VMDK-BUNDLE-FIXTURES.md](VMDK-BUNDLE-FIXTURES.md). The original standalone evidence above remains historical.
