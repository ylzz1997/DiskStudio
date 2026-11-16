# DiskStudio local change

Source: `ntfs` 0.4.0 from the crates.io cache, maintained by Colin Finck.
Crate SHA-256: `26fe2140f3b6eef2648fa331ca7af7652da1bf3bb84f715878bf50f2f8f03c38`.
Upstream: https://github.com/ColinFinck/ntfs/tree/v0.4.0
Original MIT and Apache-2.0 licenses are retained alongside the sources.

The only Rust library change is the read-only `NtfsAttribute::record_bytes`
accessor in `src/attribute.rs`. It exposes the constructor-validated attribute
slice from the owning FILE record, after upstream update-sequence fixups. It
does not expose paths, open devices, change run decoding or implement writes.
DiskStudio's bounded compression parser consumes these bytes instead of
re-reading possibly fragmented FILE records as contiguous raw sectors.

`src`, Cargo manifests, README and license files are retained. Upstream examples
and test data are not runtime dependencies and are not copied.
The explicit `ntfs-shell` example target is removed from the active manifest;
the original manifest remains in `Cargo.toml.orig`.
Application integration tests use the existing provenance-documented NTFS fixture in
`tests/fixtures`; the new accessor is exercised for base and extension records.

LZNT1 decoding and compression-unit handling are implemented in
`src-tauri/src/ntfs_compression.rs`, not in this library. References:

- https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-xca/5655f4a3-6ba4-489b-959f-e1f407c52f15
- https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-xca/94164d22-2928-4417-876e-d193766c4db6
- https://kernel.googlesource.com/pub/scm/linux/kernel/git/torvalds/linux/+/865730eec5435ce40a5dc3c615077d04c8f95098/fs/ntfs/layout.h

The Windows test-only codec oracle calls RtlCompressBuffer on memory buffers;
it does not invoke filesystem compression or operate on any volume.
