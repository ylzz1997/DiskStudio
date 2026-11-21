# NTFS file clone engine

The Windows engine creates and populates a fresh NTFS volume through two pipes. DiskStudio retains the storage handles and restricts all requests to the confirmed target extent. The engine receives partition geometry, a new volume serial, and a stream of file records. The build links the NTFS-3G library and formatter with the pipe adapter in place of the upstream device adapters.

The build uses the pinned NTFS-3G 2026.9.28 source archive under `third-party/ntfs-3g` and the same extracted w64devkit compiler as the Ext4 engine. The script checks the archive SHA-256 against the upstream release and writes the separate executable inside `.vm`. NTFS-3G and the adapter are licensed under GPL-2.0-or-later. [NTFS-3G 2026.9.28 release](https://github.com/tuxera/ntfs-3g/releases/tag/2026.9.28)

```powershell
$engine = & ./scripts/ntfs/build.ps1
Copy-Item -LiteralPath $engine.Path -Destination src-tauri/ntfs-engine/diskstudio-ntfs.exe
$env:DISKSTUDIO_TEST_NTFS_ENGINE = $engine.Path
cargo test -p diskstudio --bin diskstudio ntfs_clone_engine_test -- --ignored
```

The tests serve the entire target from synthetic memory. A separate Rust parser checks the resulting boot records, allocation bitmap, directory entries, file data, named streams, hard-link identity, timestamps, attributes, security descriptors, separate 8.3 aliases, raw extended attributes and reparse metadata. The WOF cases check all four file-provider algorithms, including the sparse unnamed stream's valid data length and the named stream's chunk table.

To exercise real compressed chunks, use the checksum-verified wimlib 1.14.5 Windows library with `wof-fixture.py generate DLL CORPUS`. Use a new corpus directory inside `.vm`. Set `DISKSTUDIO_TEST_WOF_CORPUS` to that directory and run the same engine tests. Then run `wof-fixture.py verify DLL CORPUS` to decompress streams extracted from both cloned geometries and compare the original logical bytes. The 4Kn test also saves the 96 MiB source image as `Ntfs-Wof-Source.raw` for the isolated disk acceptance. [wimlib release and SHA-256 checksums](https://www.wimlib.net/forums/viewtopic.php?t=1133)

The checked-in `tests/fixtures/ntfs-wof-0.bin` through `ntfs-wof-3.bin` are the synthetic backing streams from that generator. Core tests compare the decoded SHA-256 for all four algorithms. The engine test also scans the formatted and cloned volumes, reloads their saved candidates, and compares preview and extracted bytes with the generator's original payload.

## Pipe protocol

All integer fields are little endian. The parent starts with the four-byte magic `DSN6`, a 32-bit logical sector size, three 64-bit values for the volume length in bytes, partition start LBA and volume serial, and a 32-bit cluster size in bytes. The parent supplies an initially zeroed target view. Formatting writes the new filesystem structures.

The engine sends a one-byte opcode, a 64-bit offset relative to the volume, and a 32-bit length. For a write, it sends the payload immediately after the header. The parent checks each range, then responds with a one-byte status. Zero acknowledges success and is followed by exactly the requested bytes for either type of read. A pipe error or a nonzero status ends the exchange.

| Opcode | Request |
| --- | --- |
| 1 | Read bytes from the target volume |
| 2 | Write bytes within the target volume |
| 3 | Flush the target, with zero offset and length |
| 4 | Read the next bytes of the clone plan, with zero offset |

The plan starts with a 32-bit record count. Each record has one-byte operation and name length, 16-bit stream count, and 32-bit parent and link source references, followed by the UTF-16 name. The operations are directory creation (1), file creation (2) and a hard link to an earlier file (3). Reference zero denotes the root directory. Other references are one-based record positions. Each stream has a 16-bit UTF-16 name length, 16-bit storage flags, a 64-bit logical size, and the name. Plain and LZNT1 streams carry logical bytes. A sparse stream carries a 32-bit range count and, for each allocated range, the 64-bit offset, 64-bit length and bytes in that range.

Metadata restoration (4) refers to an earlier file or directory in the source field. The name, stream count and parent fields are zero. The payload contains 32-bit DOS flags, a 32-bit security descriptor length, three 64-bit NTFS timestamps in creation, modification and access order, and a 32-bit raw `$EA` length. This is followed by one byte for directory compression, one byte with bits 0, 1 and 2 for the object ID, link version and EFS metadata, and a 32-bit reparse length. When bit 2 is set, a 32-bit `$EFS` length follows the fixed header. The variable-length fields arrive in order from the security descriptor through the optional EA information and bytes, object ID, reparse data and original `$EFS` stream. Apply metadata after creating the related links, short names and child entries.

For WOF file-provider records, the plan carries the sparse unnamed stream and fully initialized `WofCompressedData` separately. The parent checks chunk offsets and hashes stored bytes. The writer sets the sparse placeholder's valid data length before installing the WOF reparse record in the new volume's index. [Microsoft WOF provider API](https://learn.microsoft.com/en-us/windows/win32/api/wofapi/nf-wofapi-wofsetfiledatalocation)

An EFS data stream carries the original logical size and ciphertext through the last 512-byte encryption unit. The child derives the two-byte padding count for the NTFS-3G import, then restores the logical and initialized sizes and installs the checked `$EFS` stream. Readback checks the original encrypted bytes, stream flags, sizes and `$EFS` metadata. [NTFS-3G EFS raw import](https://github.com/tuxera/ntfs-3g/wiki/Using-Extended-Attributes#efs-info)

Source reference zero restores the root metadata. Operation 5 writes named streams on the existing root. Its header carries the stream count and zero for the name length, parent, and source fields.

Operation 6 attaches the UTF-16 short name to an earlier file or directory in the indicated parent, with a zero stream count. Both passes check that the parent contains a single corresponding long-name link. The writer updates that link to the Win32 namespace and installs the alias in the DOS namespace.
