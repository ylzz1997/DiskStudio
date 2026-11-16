# DiskStudio

**English** · [简体中文](README.zh-CN.md)

DiskStudio is a Windows tool for disk management, file recovery, and backups. Use the partition map, file browser, and operation previews to inspect drives, manage partitions, recover files, and create backups.

This is an early preview. Back up important data and verify the selected drive before changing partitions, formatting, restoring, or cloning.

## Features

| Feature                    | What you can do                                                                                                                                                                  |
| -------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Disk management            | Inspect disks, partitions, and volumes. Create and delete ordinary GPT data partitions, change volume labels and drive letters, and resize the end of supported NTFS partitions. |
| Formatting                 | Format as NTFS, FAT12/16/32, exFAT, or supported Ext variants. Choose quick or full formatting, cluster sizes, and NTFS options where available.                                 |
| File browsing and recovery | Browse supported file systems, scan for deleted file candidates or file signatures, filter, preview, and export files, and save or resume scan progress.                         |
| Backup and restore         | Create full or incremental backups with Zstd compression and password protection. Browse backups, extract files, and restore to a target of matching capacity.                   |
| Disk and partition cloning | Copy every sector or copy used space, with readback verification.                                                                                                                |
| Image browsing             | Open RAW/IMG files and supported VHD, VHDX, VDI, VMDK, and Parallels HDD variants in read-only mode.                                                                             |
| Disk tools                 | Back up and check partition tables, view SMART data for supported devices, run surface read scans, view and search sectors, benchmark drives, and wipe data.                     |

Available operations depend on the state of the disk, partition type, and file system. The app explains why an operation is unavailable when it can determine the reason.

## Getting started

DiskStudio runs on Windows x64 and requires Microsoft Edge WebView2 Runtime. After extracting the portable package or building from source, open `DiskStudio.exe`.

- In **Disk Management** (磁盘管理), select a disk on the left, then select a partition in the map or table to see its actions. Use the **Disk** (磁盘) menu to open or close images or refresh the disk list.
- In **File Recovery** (文件恢复), choose the source and scan mode. After scanning, filter and preview candidates, then export selected files to another physical disk.
- In **Backup & Clone** (备份与克隆), select a backup, restore, or clone operation. Review the source, target, capacity, and options in the preview before confirming.

Save backups to another physical disk to protect the source.

## Supported operations and limits

- **Partition operations** primarily support ordinary GPT data partitions. System, boot, hidden, and read-only partitions have additional restrictions. NTFS resizing changes the end boundary while keeping the start fixed.
- **Quick partitioning** previews batch creation and formatting. It deletes only partitions you explicitly select that pass the protection checks.
- **Ext formatting and resizing** are limited to supported Linux data partitions. Resizing requires a clean, unmounted file system and changes only the end boundary.
- **File recovery** depends on the state of the original data. Overwriting, TRIM, fragmentation, and metadata damage can make a candidate unrecoverable. EFS, deleted Ext files, and general fragmented file reconstruction are unsupported.
- **Cloning and restoring** require matching scope, capacity, and sector geometry as applicable. Resizing during cloning and system migration are unsupported. A destination may remain offline after writing. The app leaves duplicate disk identifiers inactive.
- **Image browsing** is read-only. Support varies by container variant, snapshot chain, and encryption format.
- **Interrupted operations** may leave changes in place. If a job fails or you cancel it, check which steps completed and inspect the target before proceeding.

## Build from source

### Requirements

- Windows x64.
- Node.js 22.12 or a later supported LTS version, with npm.
- Stable Rust with the `x86_64-pc-windows-msvc` toolchain.
- Visual Studio C++ Build Tools with the Desktop development with C++ workload and a Windows SDK.
- Microsoft Edge WebView2 Runtime.

See the [Tauri Windows prerequisites](https://v2.tauri.app/start/prerequisites/) for setup instructions.

### Build

Download or clone the complete source and run these commands in PowerShell from the repository root.

```powershell
npm ci
npm run tauri -- build --no-bundle
```

Tauri builds the frontend and the desktop executable. The executable is ready to run without a development server.

```text
target/release/diskstudio.exe
```

This command builds a portable executable. See the [Tauri CLI reference](https://v2.tauri.app/reference/cli/) for packaging options.

Keep `vendor/`, `third-party/`, and `src-tauri/ext4-engine/` in the source tree. The build embeds the file system tools. See the [e2fsprogs notes](third-party/e2fsprogs/README.md) for their source, licenses, and separate build instructions.
