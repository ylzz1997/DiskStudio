param([switch]$NtfsReconstruction, [switch]$FatReconstruction, [switch]$LostBackupBoot, [switch]$BitLockerMarker, [switch]$ResizeClone, [switch]$ResizeCloneContinue, [switch]$RetainedRestore, [switch]$ArchiveRestore, [switch]$CapacityRestore, [switch]$FatRestore, [switch]$ExtCopy, [switch]$BtrfsExactClone, [switch]$SignatureScope, [switch]$SignatureFormats, [switch]$SignatureMedia, [switch]$FileClone, [switch]$FileCloneExfat, [switch]$FileCloneNtfs, [switch]$FileCloneNtfsMetadata, [switch]$FileCloneNtfsMetadataContinue, [switch]$FileCloneNtfsStorage, [switch]$FileCloneNtfsStorageAudit, [switch]$FileCloneNtfsLinks, [switch]$FileCloneNtfsLinksInspect, [switch]$FileCloneNtfsLinksContinue, [switch]$FileCloneNtfsLinksAudit, [switch]$FileCloneNtfsWof, [switch]$FileCloneNtfsWofContinue, [switch]$FileCloneNtfsWofImage, [switch]$FileCloneNtfsWofImageContinue, [switch]$FileCloneNtfsWofImageAfterOnline, [switch]$FileCloneNtfsEfs, [switch]$WholeFileClone, [switch]$SurfaceRefinement, [switch]$SurfaceRefinementSetup, [switch]$SurfaceTimeout, [switch]$SurfaceParallel, [switch]$SurfaceRepair, [switch]$SectorEdit)
$ErrorActionPreference = 'Stop'
$diskStudioProject = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..\..'))
Push-Location $diskStudioProject
$diskStudioPreviousRustFlags = $env:RUSTFLAGS
$diskStudioPreviousDesktopHash = $env:DISKSTUDIO_ACCEPTANCE_DESKTOP_SHA256
$diskStudioPreviousWofHash = $env:DISKSTUDIO_ACCEPTANCE_WOF_IMAGE_SHA256
try {
    # Build only. Never launch this executable on the host.
    # Apply CRT selection to C dependencies too (not just the final Rust crate).
    $env:RUSTFLAGS = '-C target-feature=+crt-static'
    $fixtureBatches = @($SectorEdit, $SurfaceRepair, $SurfaceParallel, $SurfaceTimeout, $SurfaceRefinementSetup, $SurfaceRefinement, $NtfsReconstruction, $FatReconstruction, $LostBackupBoot, $BitLockerMarker, $ResizeClone, $ResizeCloneContinue, $RetainedRestore, $ArchiveRestore, $CapacityRestore, $FatRestore, $ExtCopy, $BtrfsExactClone, $SignatureScope, $SignatureFormats, $SignatureMedia, $FileClone, $FileCloneExfat, $FileCloneNtfs, $FileCloneNtfsMetadata, $FileCloneNtfsMetadataContinue, $FileCloneNtfsStorage, $FileCloneNtfsStorageAudit, $FileCloneNtfsLinks, $FileCloneNtfsLinksInspect, $FileCloneNtfsLinksContinue, $FileCloneNtfsLinksAudit, $FileCloneNtfsWof, $FileCloneNtfsWofContinue, $FileCloneNtfsWofImage, $FileCloneNtfsWofImageContinue, $FileCloneNtfsWofImageAfterOnline, $FileCloneNtfsEfs, $WholeFileClone) | Where-Object { $_ }
    if (@($fixtureBatches).Count -gt 1) { throw 'Select one fixture batch' }
    if ($fixtureBatches) {
        $desktopHash = (Get-FileHash -LiteralPath 'target/release/diskstudio.exe').Hash.ToLowerInvariant()
        if ((Get-FileHash -LiteralPath '.vm/artifacts/DiskStudio.exe').Hash.ToLowerInvariant() -ne $desktopHash) {
            throw 'Desktop artifact does not match the completed build'
        }
        $env:DISKSTUDIO_ACCEPTANCE_DESKTOP_SHA256 = $desktopHash
    }
    if ($FileCloneNtfsWofImage -or $FileCloneNtfsWofImageContinue -or $FileCloneNtfsWofImageAfterOnline) {
        $wof = Get-Item -LiteralPath '.vm/payload/Ntfs-Wof-Source.raw'
        if ($wof.PSIsContainer -or ($wof.Attributes -band [IO.FileAttributes]::ReparsePoint) -or $wof.Length -ne 96MB) {
            throw 'Expected a regular 96 MiB NTFS WOF fixture'
        }
        $env:DISKSTUDIO_ACCEPTANCE_WOF_IMAGE_SHA256 = (Get-FileHash -LiteralPath $wof.FullName).Hash.ToLowerInvariant()
    }
    cargo build --manifest-path scripts/vm/guest-checks/Cargo.toml --release --target-dir target/guest-checks
    if ($LASTEXITCODE -ne 0) { throw 'Guest acceptance driver build failed' }
    $diskStudioPayload = Join-Path $diskStudioProject '.vm\payload'
    if (!(Test-Path -LiteralPath $diskStudioPayload)) { New-Item -ItemType Directory -Path $diskStudioPayload | Out-Null }
    if ((Get-Item -LiteralPath $diskStudioPayload).Attributes -band [IO.FileAttributes]::ReparsePoint) { throw 'Payload must not be a link' }
    if ($fixtureBatches) {
        $testEntrypoints = if ($SectorEdit) {
            @('Sector-Edit-4Kn-Baseline.exe', 'Sector-Edit-4Kn-Verify.exe')
        } elseif ($SurfaceRepair) {
            @('Surface-Repair-4Kn-Baseline.exe', 'Surface-Repair-4Kn-Verify.exe', 'Surface-Repair-4Kn-Reports.exe')
        } elseif ($SurfaceParallel) {
            @('Surface-Parallel-512-Inspect.exe')
        } elseif ($SurfaceTimeout) {
            @('Surface-Refinement-4Kn-Inspect.exe', 'Surface-Timeout-4Kn-Verify.exe')
        } elseif ($SurfaceRefinementSetup) {
            @('Surface-Refinement-4Kn-Prepare.exe')
        } elseif ($SurfaceRefinement) {
            @('Surface-Refinement-4Kn-Inspect.exe', 'Surface-Refinement-4Kn-Verify.exe')
        } elseif ($WholeFileClone) {
            @('Whole-File-Clone-4Kn-Prepare.exe', 'Whole-File-Clone-4Kn-Resume.exe', 'Whole-File-Clone-4Kn-Rebind.exe', 'Whole-File-Clone-4Kn-Verify.exe')
        } elseif ($FileCloneNtfsEfs) {
            @('File-Clone-Ntfs-4Kn-Efs-Prepare.exe', 'File-Clone-Ntfs-4Kn-Efs-Baseline.exe', 'File-Clone-Ntfs-4Kn-Efs-Verify.exe')
        } elseif ($FileCloneNtfsWofImageAfterOnline) {
            @('File-Clone-Ntfs-4Kn-Wof-Image-After-Online.exe', 'File-Clone-Ntfs-4Kn-Wof-Image-Settled-Baseline.exe', 'File-Clone-Ntfs-4Kn-Wof-Image-Settled-Verify.exe')
        } elseif ($FileCloneNtfsWofImageContinue) {
            @('File-Clone-Ntfs-4Kn-Wof-Image-Continue.exe', 'File-Clone-Ntfs-4Kn-Wof-Image-Resume-Baseline.exe', 'File-Clone-Ntfs-4Kn-Wof-Image-Resume-Verify.exe')
        } elseif ($FileCloneNtfsWofImage) {
            @('File-Clone-Ntfs-4Kn-Wof-Image-Prepare.exe', 'File-Clone-Ntfs-4Kn-Wof-Image-Baseline.exe', 'File-Clone-Ntfs-4Kn-Wof-Image-Verify.exe')
        } elseif ($FileCloneNtfsWofContinue) {
            @('File-Clone-Ntfs-4Kn-Wof-Continue.exe', 'File-Clone-Ntfs-4Kn-Wof-Resume-Baseline.exe', 'File-Clone-Ntfs-4Kn-Wof-Resume-Verify.exe')
        } elseif ($FileCloneNtfsWof) {
            @('File-Clone-Ntfs-4Kn-Wof-Prepare.exe', 'File-Clone-Ntfs-4Kn-Wof-Baseline.exe', 'File-Clone-Ntfs-4Kn-Wof-Verify.exe')
        } elseif ($FileCloneNtfsLinksAudit) {
            @('File-Clone-Ntfs-4Kn-Links-Root-Audit.exe')
        } elseif ($FileCloneNtfsLinksContinue) {
            @('File-Clone-Ntfs-4Kn-Links-Resume-Baseline.exe', 'File-Clone-Ntfs-4Kn-Links-Resume-Verify.exe')
        } elseif ($FileCloneNtfsLinksInspect) {
            @('File-Clone-Ntfs-4Kn-Links-Inspect.exe')
        } elseif ($FileCloneNtfsLinks) {
            @('File-Clone-Ntfs-4Kn-Links-Prepare.exe', 'File-Clone-Ntfs-4Kn-Links-Baseline.exe', 'File-Clone-Ntfs-4Kn-Links-Verify.exe')
        } elseif ($FileCloneNtfsStorageAudit) {
            @('File-Clone-Ntfs-4Kn-Storage-Audit.exe')
        } elseif ($FileCloneNtfsStorage) {
            @('File-Clone-Ntfs-4Kn-Storage-Prepare.exe', 'File-Clone-Ntfs-4Kn-Storage-Baseline.exe', 'File-Clone-Ntfs-4Kn-Storage-Verify.exe')
        } elseif ($FileCloneNtfsMetadataContinue) {
            @('File-Clone-Ntfs-4Kn-Metadata-Continue.exe')
        } elseif ($FileCloneNtfsMetadata) {
            @('File-Clone-Ntfs-4Kn-Metadata-Prepare.exe', 'File-Clone-Ntfs-4Kn-Metadata-Baseline.exe', 'File-Clone-Ntfs-4Kn-Metadata-Verify.exe')
        } elseif ($FileCloneNtfs) {
            @(
                'File-Clone-Ntfs-Prepare.exe'
                'File-Clone-Ntfs-Continue.exe'
                'File-Clone-Ntfs-Assess.exe'
                'File-Clone-Ntfs-Confirm-Prepared.exe'
                'File-Clone-Ntfs-Verify.exe'
                'File-Clone-Ntfs-Source-Lock.exe'
                'File-Clone-Ntfs-4Kn-Prepare.exe'
                'File-Clone-Ntfs-4Kn-Baseline.exe'
                'File-Clone-Ntfs-4Kn-Verify.exe'
                'File-Clone-Ntfs-4Kn-Inspect-Source.exe'
                'File-Clone-Ntfs-4Kn-Settled-Baseline.exe'
                'File-Clone-Ntfs-4Kn-Verify-Settled.exe'
                'File-Clone-Ntfs-4Kn-Audit-Settled.exe'
            )
        } elseif ($FileCloneExfat) {
            @('File-Clone-Exfat-Prepare.exe', 'File-Clone-Exfat-Continue.exe', 'File-Clone-Exfat-Verify.exe')
        } elseif ($FileClone) {
            @('File-Clone-Prepare.exe', 'File-Clone-Verify.exe')
        } elseif ($SignatureMedia) {
            @('Signature-Media-4Kn-Prepare.exe', 'Signature-Media-4Kn-Verify.exe')
        } elseif ($SignatureFormats) {
            @('Signature-Formats-4Kn-Prepare.exe', 'Signature-Formats-4Kn-Verify.exe')
        } elseif ($SignatureScope) {
            @('Signature-Scope-4Kn-Prepare.exe', 'Signature-Scope-4Kn-Verify.exe')
        } elseif ($BtrfsExactClone) {
            @('Btrfs-Exact-Clone-4Kn-Prepare.exe', 'Btrfs-Exact-Clone-4Kn-Verify.exe')
        } elseif ($ExtCopy) {
            @('Ext-Copy-Prepare.exe', 'Ext-Copy-Verify-Restores.exe', 'Ext-Copy-Verify-Clones.exe')
        } elseif ($FatRestore) {
            @('FAT-Restore-4Kn-Prepare.exe', 'FAT-Restore-4Kn-Verify.exe')
        } elseif ($CapacityRestore) {
            @('Capacity-Restore-4Kn-Prepare.exe', 'Capacity-Restore-4Kn-Continue.exe', 'Capacity-Restore-4Kn-After-Ntfs.exe', 'Capacity-Restore-4Kn-After-Fat32.exe', 'Capacity-Restore-4Kn-Verify.exe')
        } elseif ($ArchiveRestore) {
            @('Archive-Restore-4Kn-Prepare.exe', 'Archive-Restore-4Kn-Continue.exe', 'Archive-Restore-4Kn-Verify-Partition.exe', 'Archive-Restore-4Kn-Verify-Disk.exe')
        } elseif ($RetainedRestore) {
            @('Retained-Restore-Prepare.exe', 'Retained-Restore-Assess.exe', 'Retained-Restore-After-Exact.exe', 'Retained-Restore-Verify.exe')
        } elseif ($ResizeCloneContinue) {
            @('Resize-Clone-Continue.exe')
        } elseif ($ResizeClone) {
            @('Resize-Clone-Prepare.exe', 'Resize-Clone-After-Growth.exe', 'Resize-Clone-Verify.exe')
        } elseif ($BitLockerMarker) {
            @('BitLocker-Marker-Prepare.exe', 'BitLocker-Marker-Verify.exe')
        } elseif ($LostBackupBoot) {
            @('Lost-Backup-Boot-Prepare.exe', 'Lost-Backup-Boot-Verify.exe')
        } elseif ($FatReconstruction) {
            @('Fat-Reconstruct-Prepare.exe', 'Fat-Reconstruct-Verify.exe')
        } else {
            @('Ntfs-Reconstruct-Prepare.exe', 'Ntfs-Reconstruct-Inspect.exe', 'Ntfs-Reconstruct-Seed.exe', 'Ntfs-Reconstruct-Verify.exe')
        }
        foreach ($name in $testEntrypoints) {
            $destination = Join-Path $diskStudioPayload $name
            if (Test-Path -LiteralPath $destination) { throw "Preserve existing guest entry: $destination" }
            Copy-Item -LiteralPath 'target/guest-checks/release/diskstudio-guest-checks.exe' -Destination $destination
            Get-FileHash -LiteralPath $destination
        }
        return
    }
    Copy-Item -LiteralPath 'target/guest-checks/release/diskstudio-guest-checks.exe' -Destination (Join-Path $diskStudioPayload 'DiskStudio-Guest-Checks.exe')
    Copy-Item -LiteralPath 'target/guest-checks/release/diskstudio-guest-checks.exe' -Destination (Join-Path $diskStudioPayload 'Fat-Resume-Checks.exe')
    Copy-Item -LiteralPath 'target/guest-checks/release/diskstudio-guest-checks.exe' -Destination (Join-Path $diskStudioPayload 'Ext-Resume-Checks.exe')
    Copy-Item -LiteralPath 'target/guest-checks/release/diskstudio-guest-checks.exe' -Destination (Join-Path $diskStudioPayload 'Lost-Fixture-Seed.exe')
    Copy-Item -LiteralPath 'target/guest-checks/release/diskstudio-guest-checks.exe' -Destination (Join-Path $diskStudioPayload 'Lost-Fixture-Verify.exe')
    Copy-Item -LiteralPath 'target/guest-checks/release/diskstudio-guest-checks.exe' -Destination (Join-Path $diskStudioPayload 'Lost-Fixture-Inspect.exe')
    Copy-Item -LiteralPath 'target/guest-checks/release/diskstudio-guest-checks.exe' -Destination (Join-Path $diskStudioPayload 'Device-State-Baseline.exe')
    Copy-Item -LiteralPath 'target/guest-checks/release/diskstudio-guest-checks.exe' -Destination (Join-Path $diskStudioPayload 'Device-State-Verify.exe')
    Copy-Item -LiteralPath 'target/guest-checks/release/diskstudio-guest-checks.exe' -Destination (Join-Path $diskStudioPayload 'Maintenance-Zero-Verify.exe')
    Copy-Item -LiteralPath 'target/guest-checks/release/diskstudio-guest-checks.exe' -Destination (Join-Path $diskStudioPayload 'Maintenance-Inspect.exe')
    Copy-Item -LiteralPath 'target/guest-checks/release/diskstudio-guest-checks.exe' -Destination (Join-Path $diskStudioPayload 'DiskStudio-Ext-Format.exe')
    Copy-Item -LiteralPath 'target/guest-checks/release/diskstudio-guest-checks.exe' -Destination (Join-Path $diskStudioPayload 'DiskStudio-Ext-Resize.exe')
    Copy-Item -LiteralPath 'target/guest-checks/release/diskstudio-guest-checks.exe' -Destination (Join-Path $diskStudioPayload 'DiskStudio-Format-Checks.exe')
    Copy-Item -LiteralPath 'target/guest-checks/release/diskstudio-guest-checks.exe' -Destination (Join-Path $diskStudioPayload 'DiskStudio-Format-Report.exe')
    Copy-Item -LiteralPath 'target/guest-checks/release/diskstudio-guest-checks.exe' -Destination (Join-Path $diskStudioPayload 'DiskStudio-Native-FAT.exe')
    Copy-Item -LiteralPath 'target/guest-checks/release/diskstudio-guest-checks.exe' -Destination (Join-Path $diskStudioPayload 'DiskStudio-Native-FAT-Report.exe')
    Copy-Item -LiteralPath 'target/guest-checks/release/diskstudio-guest-checks.exe' -Destination (Join-Path $diskStudioPayload 'DiskStudio-Native-FAT-Finish.exe')
    Copy-Item -LiteralPath 'target/guest-checks/release/diskstudio-guest-checks.exe' -Destination (Join-Path $diskStudioPayload 'DiskStudio-Volume-Probe.exe')
    Copy-Item -LiteralPath 'target/guest-checks/release/diskstudio-guest-checks.exe' -Destination (Join-Path $diskStudioPayload 'DiskStudio-Lock-Probe.exe')
    Copy-Item -LiteralPath 'target/guest-checks/release/diskstudio-guest-checks.exe' -Destination (Join-Path $diskStudioPayload 'DiskStudio-Metadata-Checks.exe')
    Copy-Item -LiteralPath 'target/guest-checks/release/diskstudio-guest-checks.exe' -Destination (Join-Path $diskStudioPayload 'DiskStudio-Format-Support.exe')
    Copy-Item -LiteralPath 'target/guest-checks/release/diskstudio-guest-checks.exe' -Destination (Join-Path $diskStudioPayload 'DiskStudio-Ext4-Format.exe')
    Copy-Item -LiteralPath 'target/guest-checks/release/diskstudio-guest-checks.exe' -Destination (Join-Path $diskStudioPayload 'DiskStudio-Ext4-Resize.exe')
    Copy-Item -LiteralPath 'target/guest-checks/release/diskstudio-guest-checks.exe' -Destination (Join-Path $diskStudioPayload 'DiskStudio-Ext4-Resume.exe')
    Copy-Item -LiteralPath 'target/guest-checks/release/diskstudio-guest-checks.exe' -Destination (Join-Path $diskStudioPayload 'DiskStudio-Ext4-Online.exe')
    Copy-Item -LiteralPath 'target/guest-checks/release/diskstudio-guest-checks.exe' -Destination (Join-Path $diskStudioPayload 'DiskStudio-Ext4-After-UI.exe')
    Get-FileHash -LiteralPath (Join-Path $diskStudioPayload 'DiskStudio-Guest-Checks.exe') -Algorithm SHA256
} finally {
    $env:DISKSTUDIO_ACCEPTANCE_WOF_IMAGE_SHA256 = $diskStudioPreviousWofHash
    $env:DISKSTUDIO_ACCEPTANCE_DESKTOP_SHA256 = $diskStudioPreviousDesktopHash
    $env:RUSTFLAGS = $diskStudioPreviousRustFlags
    Pop-Location
}
