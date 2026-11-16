# Ext filesystem fixtures

Unmodified compressed fixtures from `nicholasbishop/ext4-view-rs` at commit
`8facbf4c2986b7e2dbdef92514d696f1559a2b90`, the source commit of ext4-view 1.0.0.
Upstream: https://github.com/nicholasbishop/ext4-view-rs/tree/8facbf4c2986b7e2dbdef92514d696f1559a2b90/test_data
Copyright 2023 Google LLC. MIT license retained in `ext-testfs-LICENSE.txt`.

| Compressed file | Filesystem | Decoded size | SHA-256 of compressed bytes |
| --- | --- | --- | --- |
| test_disk_ext2.bin.zst | Ext2 | 96 MiB | 1aa036e4ba6075dfe21a400127dcea52669d04e2481ee007c7891e827dbbe37a |
| test_disk_ext3.bin.zst | Ext3 | 96 MiB | 479c773c5863d2a6135287c451cf743b970146feeae7cb3d00ffbc8869eafa62 |
| test_disk1.bin.zst | Ext4 | 64 MiB | d0f452c1e1289a3020df14b54b26bbca9cfc4a47f2b9abdefedfb357319af098 |

Included only under `cfg(test)` and decompressed into memory (256 MiB maximum).
Tests never mount them, execute upstream creation scripts, open a device or
modify these fixture files. They are not bundled in the application. Mutation
cases operate on private `Vec` copies, including cycle, inode reuse, allocation,
non-UTF-8 name and hardlink variants.

Expected contents are independent of the reader implementation:

- Ext2 and Ext4 `/small_file`: `hello, world!` (13 bytes).
- Ext3 `/medium_dir/42`: `42`; exercises a TEA HTREE directory and 128-byte inodes.
- Ext2 and Ext4 `/holes`: ten 1024-byte blocks filled with
  `00,00,a1,a2,00,00,a3,a4,00,00` respectively.
- Ext2 `/big_file`: 69,900 blocks of 1024 bytes; first and last four bytes of
  each block contain its little-endian u32 index, all other bytes zero. The
  streamed SHA-256 is compared with this independently generated sequence,
  covering direct, single, double and triple indirect blocks.
- Ext4 `/empty_dir`: retained as an empty directory. Symlinks and special files
  are counted as skipped, not followed or reconstructed.

These fixtures validate clean existing-file reads, not deleted-file recovery,
journal replay, live-volume consistency, all feature combinations, or VM/raw
device integration. See `vendor/ext4-view/DISKSTUDIO-PATCH.md` for the small
read-only metadata identity patch to the vendored dependency.
