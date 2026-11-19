# WinBtrfs formatter used by DiskStudio

DiskStudio embeds a separate Windows formatter built from WinBtrfs commit
`3fa5cf21dde2913d58d7b74e150625e18bd490bd`. The included upstream archive has
SHA-256 `c21258d7e755fbdc23f02c3febb0adce0b576f394d2f8ebbc89c7bfe71a3febc`.

The upstream archive, licence and DiskStudio modifications in
`scripts/btrfs/patch.mjs` accompany the executable. The formatter is built
from the LGPL-3.0-or-later WinBtrfs user-space formatter and its CRC32C
implementation. See the source file headers and `LICENCE` for the terms.

Run `pwsh -NoProfile -File scripts/btrfs/build.ps1` from the project root.
The build uses the same pinned w64devkit 2.11.0 compiler as the Ext tools.
The executable accepts an inherited Windows file handle, a sector-aligned
partition extent, a UTF-8 label encoded in hex and a new filesystem UUID.
It performs bounded writes and flushes the trees and mirrors before
publishing the primary superblock. DiskStudio supplies the handle after
disk and partition identity verification, volume locking and disk offlining.
