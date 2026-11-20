# NTFS-3G helper

DiskStudio embeds a separate native executable built from NTFS-3G 2026.9.28 and the DiskStudio pipe adapter. The source archive in this directory contains the upstream notices and license texts. The upstream SHA-256 is `350d9415c59f7c3fa74e23985ad2d56423c6a168337368f672e2e362d8f295c8`. [Upstream release](https://github.com/tuxera/ntfs-3g/releases/tag/2026.9.28)

The NTFS-3G tools and library used by the helper are under GPL-2.0-or-later. The separate helper's source and build instructions include this archive and the adapter, portability changes, and build script under `scripts/ntfs`. [Build instructions](../../scripts/ntfs/README.md)
