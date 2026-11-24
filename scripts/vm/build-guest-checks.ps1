param([switch]$NtfsReconstruction, [switch]$FatReconstruction, [switch]$ExtReconstruction, [switch]$LostBackupBoot, [switch]$LostExt4, [switch]$RetainedLost, [switch]$UnallocatedSearch, [switch]$OfficePreview, [switch]$PagedPreview, [switch]$BitLockerMarker, [switch]$ResizeClone, [switch]$ResizeCloneContinue, [switch]$RetainedRestore, [switch]$ArchiveRestore, [switch]$CapacityRestore, [switch]$FatRestore, [switch]$ExtCopy, [switch]$BtrfsExactClone, [switch]$SignatureScope, [switch]$SignatureFormats, [switch]$SignatureMedia, [switch]$CompleteRecovery, [switch]$SignatureImages, [switch]$SignatureContainers, [switch]$SignatureAudio, [switch]$SignatureDocuments, [switch]$ExtJournal, [switch]$ExtJournalTree, [switch]$ExtJournalResume, [switch]$ExtComplete, [switch]$RetainedSignatures, [switch]$RetainedSignatureProgress, [switch]$VhdxChain, [switch]$VmdkChain, [switch]$VdiChain, [switch]$VhdxLog, [switch]$EsxiChain, [switch]$ParallelsBundle, [switch]$ParallelsHistory, [switch]$ImageArchive, [switch]$DeviceImage, [switch]$BootInspection, [switch]$BootReferences, [switch]$BootInheritance, [switch]$BootRepair, [switch]$BootFiles, [switch]$BootStore, [switch]$BootDamaged, [switch]$TableMbr, [switch]$NtfsWofRecovery, [switch]$FileClone, [switch]$FileCloneExfat, [switch]$FileCloneNtfs, [switch]$FileCloneNtfsMetadata, [switch]$FileCloneNtfsMetadataContinue, [switch]$FileCloneNtfsStorage, [switch]$FileCloneNtfsStorageAudit, [switch]$FileCloneNtfsLinks, [switch]$FileCloneNtfsLinksInspect, [switch]$FileCloneNtfsLinksContinue, [switch]$FileCloneNtfsLinksAudit, [switch]$FileCloneNtfsWof, [switch]$FileCloneNtfsWofContinue, [switch]$FileCloneNtfsWofImage, [switch]$FileCloneNtfsWofImageContinue, [switch]$FileCloneNtfsWofImageAfterOnline, [switch]$FileCloneNtfsEfs, [switch]$WholeFileClone, [switch]$SurfaceRefinement, [switch]$SurfaceRefinementSetup, [switch]$SurfaceTimeout, [switch]$SurfaceParallel, [switch]$SurfaceRepair, [switch]$SectorEdit)
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
    $fixtureBatches = @($SectorEdit, $SurfaceRepair, $SurfaceParallel, $SurfaceTimeout, $SurfaceRefinementSetup, $SurfaceRefinement, $NtfsReconstruction, $FatReconstruction, $ExtReconstruction, $LostBackupBoot, $LostExt4, $RetainedLost, $UnallocatedSearch, $OfficePreview, $PagedPreview, $BitLockerMarker, $ResizeClone, $ResizeCloneContinue, $RetainedRestore, $ArchiveRestore, $CapacityRestore, $FatRestore, $ExtCopy, $BtrfsExactClone, $SignatureScope, $SignatureFormats, $SignatureMedia, $CompleteRecovery, $SignatureImages, $SignatureContainers, $SignatureAudio, $SignatureDocuments, $ExtJournal, $ExtJournalTree, $ExtJournalResume, $ExtComplete, $RetainedSignatures, $RetainedSignatureProgress, $VhdxChain, $VmdkChain, $VdiChain, $VhdxLog, $EsxiChain, $ParallelsBundle, $ParallelsHistory, $ImageArchive, $DeviceImage, $BootInspection, $BootReferences, $BootInheritance, $BootRepair, $BootFiles, $BootStore, $BootDamaged, $TableMbr, $NtfsWofRecovery, $FileClone, $FileCloneExfat, $FileCloneNtfs, $FileCloneNtfsMetadata, $FileCloneNtfsMetadataContinue, $FileCloneNtfsStorage, $FileCloneNtfsStorageAudit, $FileCloneNtfsLinks, $FileCloneNtfsLinksInspect, $FileCloneNtfsLinksContinue, $FileCloneNtfsLinksAudit, $FileCloneNtfsWof, $FileCloneNtfsWofContinue, $FileCloneNtfsWofImage, $FileCloneNtfsWofImageContinue, $FileCloneNtfsWofImageAfterOnline, $FileCloneNtfsEfs, $WholeFileClone) | Where-Object { $_ }
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
    $diskStudioGuestBuild = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { 'target/guest-checks' }
    cargo build --manifest-path scripts/vm/guest-checks/Cargo.toml --release --target-dir $diskStudioGuestBuild
    if ($LASTEXITCODE -ne 0) { throw 'Guest acceptance driver build failed' }
    $diskStudioGuestExe = Join-Path $diskStudioGuestBuild 'release/diskstudio-guest-checks.exe'
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
        } elseif ($NtfsWofRecovery) {
            @('Ntfs-Wof-Recovery-4Kn-Baseline.exe', 'Ntfs-Wof-Recovery-4Kn-Verify.exe')
        } elseif ($SignatureDocuments) {
            @('Signature-Documents-4Kn-Prepare.exe', 'Signature-Documents-4Kn-Verify.exe')
        } elseif ($VhdxChain) {
            @('Vhdx-Chain-4Kn-Prepare.exe', 'Vhdx-Chain-4Kn-Verify.exe')
        } elseif ($VmdkChain) {
            @('Vmdk-Chain-4Kn-Prepare.exe', 'Vmdk-Chain-4Kn-Verify.exe')
        } elseif ($VdiChain) {
            @('Vdi-Chain-4Kn-Prepare.exe', 'Vdi-Chain-4Kn-Verify.exe')
        } elseif ($VhdxLog) {
            @('Vhdx-Log-4Kn-Prepare.exe', 'Vhdx-Log-4Kn-Verify.exe')
        } elseif ($BootInspection) {
            @('Boot-Inspection-4Kn-Prepare.exe', 'Boot-Inspection-4Kn-Verify.exe')
        } elseif ($BootReferences) {
            @('Boot-References-4Kn-Prepare.exe', 'Boot-References-4Kn-Verify.exe')
        } elseif ($BootInheritance) {
            @('Boot-Inheritance-4Kn-Prepare.exe', 'Boot-Inheritance-4Kn-Verify.exe')
        } elseif ($BootRepair) {
            @('Boot-Repair-4Kn-Prepare.exe', 'Boot-Repair-4Kn-Verify.exe')
        } elseif ($BootFiles) {
            @('Boot-Files-4Kn-Prepare.exe', 'Boot-Files-4Kn-Verify.exe')
        } elseif ($BootStore) {
            @('Boot-Store-4Kn-Prepare.exe', 'Boot-Store-4Kn-Verify.exe')
        } elseif ($BootDamaged) {
            @('Boot-Dirty-4Kn-Prepare.exe', 'Boot-Dirty-4Kn-Verify.exe', 'Boot-Damaged-4Kn-Prepare.exe', 'Boot-Damaged-4Kn-Verify.exe')
        } elseif ($DeviceImage) {
            @('Device-Image-4Kn-Prepare.exe', 'Device-Image-4Kn-Verify.exe')
        } elseif ($ImageArchive) {
            @('Image-Archive-4Kn-Verify.exe')
        } elseif ($ParallelsHistory) {
            @('Parallels-History-4Kn-Prepare.exe', 'Parallels-History-4Kn-Verify.exe')
        } elseif ($ParallelsBundle) {
            @('Parallels-Bundle-4Kn-Prepare.exe', 'Parallels-Bundle-4Kn-Verify.exe')
        } elseif ($EsxiChain) {
            @('Esxi-Chain-4Kn-Prepare.exe', 'Esxi-Chain-4Kn-Verify.exe')
        } elseif ($RetainedSignatureProgress) {
            @('Retained-Progress-4Kn-Prepare.exe', 'Retained-Progress-4Kn-Verify.exe')
        } elseif ($RetainedSignatures) {
            @('Retained-Signatures-4Kn-Prepare.exe', 'Retained-Signatures-4Kn-Verify.exe')
        } elseif ($ExtComplete) {
            @('Ext-Complete-4Kn-Prepare.exe', 'Ext-Complete-4Kn-Verify.exe')
        } elseif ($ExtJournalResume) {
            @('Ext-Journal-Resume-4Kn-Verify.exe')
        } elseif ($ExtJournalTree) {
            @('Ext-Journal-Tree-4Kn-Prepare.exe', 'Ext-Journal-Tree-4Kn-Verify.exe')
        } elseif ($ExtJournal) {
            @('Ext-Journal-4Kn-Prepare.exe', 'Ext-Journal-4Kn-Verify.exe')
        } elseif ($TableMbr) {
            @('Table-MBR-4Kn-Prepare.exe', 'Table-MBR-4Kn-Verify.exe')
        } elseif ($SignatureAudio) {
            @('Signature-Audio-4Kn-Prepare.exe', 'Signature-Audio-4Kn-Verify.exe')
        } elseif ($SignatureContainers) {
            @('Signature-Containers-4Kn-Prepare.exe', 'Signature-Containers-4Kn-Verify.exe')
        } elseif ($SignatureImages) {
            @('Signature-Images-4Kn-Prepare.exe', 'Signature-Images-4Kn-Verify.exe')
        } elseif ($CompleteRecovery) {
            @('Complete-Recovery-4Kn-Prepare.exe', 'Complete-Recovery-4Kn-Verify.exe')
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
        } elseif ($PagedPreview) {
            @('Paged-Preview-Prepare.exe', 'Paged-Preview-Verify.exe')
        } elseif ($OfficePreview) {
            @('Office-Preview-Prepare.exe', 'Office-Preview-Verify.exe')
        } elseif ($UnallocatedSearch) {
            @('Unallocated-Search-Prepare.exe', 'Unallocated-Search-Verify.exe')
        } elseif ($RetainedLost) {
            @('Retained-Lost-Prepare.exe', 'Retained-Lost-Verify.exe')
        } elseif ($LostExt4) {
            @('Lost-Ext4-Prepare.exe', 'Lost-Ext4-Verify.exe')
        } elseif ($LostBackupBoot) {
            @('Lost-Backup-Boot-Prepare.exe', 'Lost-Backup-Boot-Verify.exe')
        } elseif ($ExtReconstruction) {
            @('Ext-Reconstruct-Prepare.exe', 'Ext-Reconstruct-Verify.exe')
        } elseif ($FatReconstruction) {
            @('Fat-Reconstruct-Prepare.exe', 'Fat-Reconstruct-Verify.exe')
        } else {
            @('Ntfs-Reconstruct-Prepare.exe', 'Ntfs-Reconstruct-Inspect.exe', 'Ntfs-Reconstruct-Seed.exe', 'Ntfs-Reconstruct-Verify.exe')
        }
        foreach ($name in $testEntrypoints) {
            $destination = Join-Path $diskStudioPayload $name
            if (Test-Path -LiteralPath $destination) { throw "Preserve existing guest entry: $destination" }
            Copy-Item -LiteralPath $diskStudioGuestExe -Destination $destination
            Get-FileHash -LiteralPath $destination
        }
        return
    }
    Copy-Item -LiteralPath $diskStudioGuestExe -Destination (Join-Path $diskStudioPayload 'DiskStudio-Guest-Checks.exe')
    Copy-Item -LiteralPath $diskStudioGuestExe -Destination (Join-Path $diskStudioPayload 'Fat-Resume-Checks.exe')
    Copy-Item -LiteralPath $diskStudioGuestExe -Destination (Join-Path $diskStudioPayload 'Ext-Resume-Checks.exe')
    Copy-Item -LiteralPath $diskStudioGuestExe -Destination (Join-Path $diskStudioPayload 'Lost-Fixture-Seed.exe')
    Copy-Item -LiteralPath $diskStudioGuestExe -Destination (Join-Path $diskStudioPayload 'Lost-Fixture-Verify.exe')
    Copy-Item -LiteralPath $diskStudioGuestExe -Destination (Join-Path $diskStudioPayload 'Lost-Fixture-Inspect.exe')
    Copy-Item -LiteralPath $diskStudioGuestExe -Destination (Join-Path $diskStudioPayload 'Device-State-Baseline.exe')
    Copy-Item -LiteralPath $diskStudioGuestExe -Destination (Join-Path $diskStudioPayload 'Device-State-Verify.exe')
    Copy-Item -LiteralPath $diskStudioGuestExe -Destination (Join-Path $diskStudioPayload 'Maintenance-Zero-Verify.exe')
    Copy-Item -LiteralPath $diskStudioGuestExe -Destination (Join-Path $diskStudioPayload 'Maintenance-Inspect.exe')
    Copy-Item -LiteralPath $diskStudioGuestExe -Destination (Join-Path $diskStudioPayload 'DiskStudio-Ext-Format.exe')
    Copy-Item -LiteralPath $diskStudioGuestExe -Destination (Join-Path $diskStudioPayload 'DiskStudio-Ext-Resize.exe')
    Copy-Item -LiteralPath $diskStudioGuestExe -Destination (Join-Path $diskStudioPayload 'DiskStudio-Format-Checks.exe')
    Copy-Item -LiteralPath $diskStudioGuestExe -Destination (Join-Path $diskStudioPayload 'DiskStudio-Format-Report.exe')
    Copy-Item -LiteralPath $diskStudioGuestExe -Destination (Join-Path $diskStudioPayload 'DiskStudio-Native-FAT.exe')
    Copy-Item -LiteralPath $diskStudioGuestExe -Destination (Join-Path $diskStudioPayload 'DiskStudio-Native-FAT-Report.exe')
    Copy-Item -LiteralPath $diskStudioGuestExe -Destination (Join-Path $diskStudioPayload 'DiskStudio-Native-FAT-Finish.exe')
    Copy-Item -LiteralPath $diskStudioGuestExe -Destination (Join-Path $diskStudioPayload 'DiskStudio-Volume-Probe.exe')
    Copy-Item -LiteralPath $diskStudioGuestExe -Destination (Join-Path $diskStudioPayload 'DiskStudio-Lock-Probe.exe')
    Copy-Item -LiteralPath $diskStudioGuestExe -Destination (Join-Path $diskStudioPayload 'DiskStudio-Metadata-Checks.exe')
    Copy-Item -LiteralPath $diskStudioGuestExe -Destination (Join-Path $diskStudioPayload 'DiskStudio-Format-Support.exe')
    Copy-Item -LiteralPath $diskStudioGuestExe -Destination (Join-Path $diskStudioPayload 'DiskStudio-Ext4-Format.exe')
    Copy-Item -LiteralPath $diskStudioGuestExe -Destination (Join-Path $diskStudioPayload 'DiskStudio-Ext4-Resize.exe')
    Copy-Item -LiteralPath $diskStudioGuestExe -Destination (Join-Path $diskStudioPayload 'DiskStudio-Ext4-Resume.exe')
    Copy-Item -LiteralPath $diskStudioGuestExe -Destination (Join-Path $diskStudioPayload 'DiskStudio-Ext4-Online.exe')
    Copy-Item -LiteralPath $diskStudioGuestExe -Destination (Join-Path $diskStudioPayload 'DiskStudio-Ext4-After-UI.exe')
    Get-FileHash -LiteralPath (Join-Path $diskStudioPayload 'DiskStudio-Guest-Checks.exe') -Algorithm SHA256
} finally {
    $env:DISKSTUDIO_ACCEPTANCE_WOF_IMAGE_SHA256 = $diskStudioPreviousWofHash
    $env:DISKSTUDIO_ACCEPTANCE_DESKTOP_SHA256 = $diskStudioPreviousDesktopHash
    $env:RUSTFLAGS = $diskStudioPreviousRustFlags
    Pop-Location
}
