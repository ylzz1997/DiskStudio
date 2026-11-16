# DiskStudio integration

Source: ext4-view 1.0.0, crates.io package, upstream commit
`8facbf4c2986b7e2dbdef92514d696f1559a2b90`.
Upstream: https://github.com/nicholasbishop/ext4-view-rs
Retained MIT and Apache 2.0 license files.

Local patch adds the inode number and validated raw inode record to `Metadata`,
with read-only getters. DiskStudio uses these to bind scan candidates to inode
identity and compare records before extraction, including generation and mapping.
No write APIs are added. The `std` feature remains disabled, so the library does
not expose a host-filesystem path loader in this application.

The inode timestamp parser also respects `i_extra_isize` for each extra timestamp
field. The allocated inode size alone is not proof that creation time or extra
epoch/nanosecond fields exist. Zero extra size uses legacy timestamps; invalid
extra size rejects the inode. DiskStudio's in-memory Ext2 fixture regression
covers a 256-byte inode with absent extra fields and nonzero padding, alongside
regular Ext2/3/4 reads. Timestamp display is independently bounded in
`src-tauri/src/file_metadata.rs` and keeps ctime separate from creation time.
Reference: https://www.kernel.org/doc/html/latest/filesystems/ext4/inodes.html
Timestamp base seconds are decoded as signed two's-complement values before
adding epoch bits. This preserves pre-1970 dates and extended dates with bit 31
set; the upstream checked numeric conversion rejected those values.
