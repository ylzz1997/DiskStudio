param(
    [Parameter(Mandatory)]
    [ValidateSet('ResetTarget', 'PrepareOccupiedTarget', 'PrepareResize', 'Launch', 'LaunchOccupied', 'CheckResult', 'PrepareBackupOutput', 'LaunchBackup', 'CheckBackup')]
    [string]$Mode
)
$ErrorActionPreference = 'Stop'
$identities = @(Get-CimInstance -Query 'SELECT UUID FROM Win32_ComputerSystemProduct')
if ($identities.Count -ne 1 -or [guid]$identities[0].UUID -ne [guid]'e20adb9f-f665-432e-9a62-dd5d897b06a0') {
    throw 'The hot-migration fixture requires the isolated DiskStudio-Test guest'
}

function Test-Serial([string]$serial, [string]$expected) {
    return $serial.Trim() -ceq $expected -or $serial.Trim() -ceq "$expected      _00000001."
}
function Confirm-Disk([string]$serial, $storage, $legacy) {
    $rows = @($storage | Where-Object { Test-Serial $_.SerialNumber $serial })
    if ($rows.Count -ne 1) { throw "Expected exactly one $serial" }
    $disk = $rows[0]
    $drives = @($legacy | Where-Object { $_.Index -eq $disk.Number })
    if ($drives.Count -ne 1 -or !(Test-Serial $drives[0].SerialNumber $serial) -or
        $disk.Size -ne 16GB -or $disk.LogicalSectorSize -ne 4096 -or $disk.PhysicalSectorSize -ne 4096 -or
        $disk.BusType -ne 17 -or $disk.IsClustered -ne $false) {
        throw "Unverified disk geometry or identity for $serial"
    }
    return $disk
}
$storage = @(Get-CimInstance -Namespace 'ROOT/Microsoft/Windows/Storage' -Query 'SELECT Number,SerialNumber,Size,LogicalSectorSize,PhysicalSectorSize,BusType,IsClustered,IsBoot,IsSystem,IsOffline,IsReadOnly,PartitionStyle,NumberOfPartitions FROM MSFT_Disk')
$legacy = @(Get-CimInstance -Query 'SELECT Index,SerialNumber FROM Win32_DiskDrive')
$archiveCase = $Mode -in 'PrepareBackupOutput', 'LaunchBackup', 'CheckBackup'
$preparing = $Mode -in 'ResetTarget', 'PrepareOccupiedTarget', 'PrepareBackupOutput'
$expectedCount = if ($preparing) { 1 } else { 2 }
if ($storage.Count -ne $expectedCount -or $legacy.Count -ne $expectedCount) { throw 'Unexpected physical disk inventory' }
$targetSerial = if ($archiveCase) { 'DS-TEST-B-0001' } else { 'DS-TEST-A-0001' }
$target = Confirm-Disk $targetSerial $storage $legacy
if ($target.IsBoot -ne $false -or $target.IsSystem -ne $false -or $target.IsReadOnly -ne $false) {
    throw 'The disposable target is protected or its state is unknown'
}

if ($Mode -eq 'PrepareResize') {
    if ($env:SystemDrive -ne 'X:') { throw 'Prepare the disposable installation from WinPE' }
    $source = Confirm-Disk 'DS-TEST-B-0001' $storage $legacy
    if ($source.IsBoot -ne $false -or $source.IsSystem -ne $false -or
        $source.IsOffline -ne $false -or $source.IsReadOnly -ne $false -or
        "$($source.PartitionStyle)" -notin '0','RAW' -or $source.NumberOfPartitions -ne 0) {
        throw 'The Windows installation requires a verified blank Test B'
    }
    $media = [IO.DriveInfo]::new([IO.Path]::GetPathRoot($PSScriptRoot))
    $wim = Get-Item -LiteralPath (Join-Path $PSScriptRoot 'sources\install.wim')
    $wimInfo = Get-Content -LiteralPath (Join-Path $PSScriptRoot 'install-info.json') -Raw | ConvertFrom-Json
    if (!$media.IsReady -or $media.DriveType -ne [IO.DriveType]::CDRom -or
        $media.VolumeLabel -ne 'DISKSTUDIO' -or $wim.Length -ne $wimInfo.bytes -or
        (Get-FileHash -LiteralPath $wim.FullName).Hash -ne $wimInfo.sha256) {
        throw 'The Windows installer must match the sealed optical payload'
    }
    foreach ($letter in 'S','W','U') {
        if (Get-PSDrive -Name $letter -ErrorAction SilentlyContinue) { throw "Required drive letter $letter is in use" }
    }
    $commands = @"
select disk $($source.Number)
convert gpt
create partition efi offset=16384 size=272
format fs=fat32 quick label=DSEFI
assign letter=S
create partition primary offset=294912 size=14800
format fs=ntfs quick label=DSWIN
assign letter=W
create partition primary offset=15462400 size=64
format fs=ntfs quick label=DSHOTDATA
assign letter=U
"@
    $commands | Set-Content -LiteralPath 'X:\ds-hot-resize-layout.txt' -Encoding ascii
    & diskpart.exe /s 'X:\ds-hot-resize-layout.txt'
    if ($LASTEXITCODE -ne 0) { throw 'Could not prepare the Test B layout' }
    $parts = @(Get-Partition -DiskNumber $source.Number)
    $efi = @($parts | Where-Object GptType -eq '{c12a7328-f81f-11d2-ba4b-00a0c93ec93b}')
    if ($parts.Count -ne 4 -or $efi.Count -ne 1 -or $efi[0].Offset -ne 16MB -or
        $efi[0].Size -ne 272MB -or (Get-Volume -DriveLetter S).FileSystemLabel -ne 'DSEFI') {
        throw 'The disposable EFI layout did not match'
    }
    foreach ($spec in @(@('W',288MB,14800MB,'DSWIN'),@('U',15100MB,64MB,'DSHOTDATA'))) {
        $part = @(Get-Partition -DriveLetter $spec[0])
        $volume = Get-Volume -DriveLetter $spec[0]
        if ($part.Count -ne 1 -or $part[0].DiskNumber -ne $source.Number -or
            $part[0].Offset -ne $spec[1] -or $part[0].Size -ne $spec[2] -or
            $volume.FileSystem -ne 'NTFS' -or $volume.FileSystemLabel -ne $spec[3]) {
            throw 'The disposable NTFS layout did not match'
        }
    }
    $scratch = (New-Item -ItemType Directory -Path 'W:\DSSetupScratch').FullName
    & dism.exe /Apply-Image "/ImageFile:$($wim.FullName)" /Index:1 /ApplyDir:W:\ "/ScratchDir:$scratch" /CheckIntegrity
    if ($LASTEXITCODE -ne 0) { throw 'Could not apply the clean Server Core image to Test B' }
    & bcdboot.exe 'W:\Windows' /s S: /f UEFI
    if ($LASTEXITCODE -ne 0 -or !(Test-Path -LiteralPath 'S:\EFI\Boot\bootx64.efi' -PathType Leaf)) {
        throw 'Could not prepare the native Test B boot chain'
    }
    [IO.File]::WriteAllText('U:\DSHotResizeData.txt', 'DiskStudio hot migration NTFS resize fixture')
    $current = Get-Disk -Number $target.Number
    if (!(Test-Serial $current.SerialNumber 'DS-TEST-A-0001') -or $current.Size -ne 16GB -or
        $current.LogicalSectorSize -ne 4096 -or $current.PhysicalSectorSize -ne 4096 -or
        $current.IsBoot -ne $false -or $current.IsSystem -ne $false -or $current.IsReadOnly -ne $false) {
        throw 'Test A no longer matches the disposable target'
    }
    if ($current.IsOffline) { Set-Disk -Number $target.Number -IsOffline $false }
    if ($current.PartitionStyle -ne 'RAW') { Clear-Disk -Number $target.Number -RemoveData -RemoveOEM -Confirm:$false }
    $fresh = Get-Disk -Number $target.Number
    if ($fresh.PartitionStyle -ne 'RAW' -or $fresh.NumberOfPartitions -ne 0) { throw 'Expected an empty Test A' }
    Initialize-Disk -Number $target.Number -PartitionStyle GPT
    $fresh = Get-Disk -Number $target.Number
    if ($fresh.PartitionStyle -ne 'GPT' -or $fresh.NumberOfPartitions -ne 0) { throw 'Expected an empty GPT Test A' }
    Write-Output 'HOT MIGRATION RESIZE FIXTURES READY'
    return
}

if ($preparing) {
    # QMP has confirmed the read-only WinPE optical and only the target disk.
    # This also prevents bringing up a second instance of the same GPT IDs.
    if ($env:SystemDrive -ne 'X:') { throw 'Reset the target from isolated WinPE with the source detached' }
    if ($target.IsOffline) { Set-Disk -Number $target.Number -IsOffline $false }
    Clear-Disk -Number $target.Number -RemoveData -RemoveOEM -Confirm:$false
    $fresh = Get-Disk -Number $target.Number
    if ($fresh.PartitionStyle -ne 'RAW' -or $fresh.NumberOfPartitions -ne 0) { throw 'Target did not return to an empty RAW layout' }
    if ($Mode -eq 'PrepareBackupOutput') {
        Initialize-Disk -Number $target.Number -PartitionStyle GPT
        $partition = New-Partition -DiskNumber $target.Number -UseMaximumSize -AssignDriveLetter
        if ($partition.DiskNumber -ne $target.Number -or $partition.Size -lt 15GB -or
            $partition.DriveLetter -notmatch '^[A-WY-Z]$') {
            throw 'The backup output fixture did not match its requested extent'
        }
        $partition | Format-Volume -FileSystem NTFS -NewFileSystemLabel 'DS-ARCHIVE-OUT' -Confirm:$false | Out-Null
        Write-Output 'WINDOWS HOT BACKUP OUTPUT READY'
        return
    }
    if ($Mode -eq 'PrepareOccupiedTarget') {
        Initialize-Disk -Number $target.Number -PartitionStyle GPT
        $partition = New-Partition -DiskNumber $target.Number -Offset 2MB -Size 128MB -AssignDriveLetter
        if ($partition.DiskNumber -ne $target.Number -or $partition.Offset -ne 2MB -or
            $partition.Size -ne 128MB -or $partition.DriveLetter -notmatch '^[A-WY-Z]$') {
            throw 'The disposable GPT fixture partition did not match its requested extent'
        }
        $partition | Format-Volume -FileSystem NTFS -NewFileSystemLabel 'DS-REPLACE' -Confirm:$false | Out-Null
        [IO.File]::WriteAllText(('{0}:\BeforeMigration.txt' -f $partition.DriveLetter), 'Disposable destination before hot migration')
        Write-Output 'HOT MIGRATION OCCUPIED TARGET READY'
        return
    }
    Write-Output 'HOT MIGRATION TARGET READY'
    return
}

$sourceSerial = if ($archiveCase) { 'DS-TEST-A-0001' } else { 'DS-TEST-B-0001' }
$source = Confirm-Disk $sourceSerial $storage $legacy
$system = @(Get-Partition -DriveLetter $env:SystemDrive.Substring(0, 1))
if ($env:SystemDrive -eq 'X:' -or $source.Number -eq $target.Number -or $source.IsOffline -ne $false -or
    $source.IsBoot -ne $true -or $source.IsSystem -ne $true -or "$($source.PartitionStyle)" -notin '2','GPT' -or
    $system.Count -ne 1 -or $system[0].DiskNumber -ne $source.Number -or $system[0].IsBoot -ne $true) {
    throw "The guest must be running Windows and its EFI partition on $sourceSerial"
}
if ($Mode -eq 'CheckResult') {
    if ($target.IsOffline -ne $true -or "$($target.PartitionStyle)" -notin '2','GPT' -or $target.NumberOfPartitions -lt 3) {
        throw 'The migrated target has not been verified offline with its GPT layout'
    }
    if (@(Get-CimInstance -Query 'SELECT ID FROM Win32_ShadowCopy').Count) { throw 'Unexpected retained VSS snapshot' }
    Write-Output 'HOT MIGRATION SOURCE STILL RUNNING. TARGET OFFLINE. SNAPSHOT RELEASED.'
    return
}

if ($archiveCase) {
    $outputPartitions = @(Get-Partition -DiskNumber $target.Number | Where-Object { $_.DriveLetter -match '^[A-WY-Z]$' })
    if ("$($target.PartitionStyle)" -notin '2','GPT' -or $target.IsOffline -ne $false -or
        $outputPartitions.Count -ne 1 -or $outputPartitions[0].Size -lt 15GB) { throw 'Unexpected backup output layout' }
    $outputVolume = $outputPartitions[0] | Get-Volume
    if ($outputVolume.FileSystem -ne 'NTFS' -or $outputVolume.FileSystemLabel -ne 'DS-ARCHIVE-OUT') {
        throw 'Unexpected backup output volume'
    }
    $outputRoot = '{0}:\' -f $outputPartitions[0].DriveLetter
    if ($Mode -eq 'CheckBackup') {
        if (@(Get-CimInstance -Query 'SELECT ID FROM Win32_ShadowCopy').Count) { throw 'Unexpected retained VSS snapshot' }
        foreach ($name in 'windows-partition.dsbackup', 'windows-disk.dsbackup') {
            $file = Get-Item -LiteralPath (Join-Path $outputRoot $name)
            if ($file.Length -le 0 -or $file.Attributes -band [IO.FileAttributes]::ReparsePoint) { throw 'Invalid archive output' }
            Write-Output "VERIFIED OUTPUT LOCATION $($file.Name) $($file.Length) BYTES"
        }
        Write-Output 'WINDOWS SOURCE STILL RUNNING. SNAPSHOTS RELEASED.'
        return
    }
    Write-Output "BACKUP OUTPUT DIRECTORY $outputRoot"
} elseif ($Mode -eq 'LaunchOccupied') {
    $oldPartitions = @(Get-Partition -DiskNumber $target.Number)
    if ("$($target.PartitionStyle)" -notin '2','GPT' -or $target.IsOffline -ne $false -or
        $oldPartitions.Count -ne 1 -or $oldPartitions[0].Offset -ne 2MB -or
        $oldPartitions[0].Size -ne 128MB) { throw 'Unexpected occupied target layout' }
    $oldVolume = $oldPartitions[0] | Get-Volume
    if ($oldVolume.FileSystem -ne 'NTFS' -or $oldVolume.FileSystemLabel -ne 'DS-REPLACE') {
        throw 'Unexpected occupied target volume'
    }
} elseif ($target.NumberOfPartitions -ne 0 -or "$($target.PartitionStyle)" -notin '0','2','RAW','GPT') {
    throw 'The hot-migration target must start empty'
}
if (@(Get-CimInstance -Query 'SELECT ID FROM Win32_ShadowCopy').Count) { throw 'The fixture must start without shadow copies' }
$mediaDrive = [IO.DriveInfo]::new([IO.Path]::GetPathRoot($PSScriptRoot))
if (!$mediaDrive.IsReady -or $mediaDrive.DriveType -ne [IO.DriveType]::CDRom -or
    $mediaDrive.VolumeLabel -ne 'DISKSTUDIO') {
    throw 'The application must come from the read-only DiskStudio tools optical media'
}
$app = Join-Path $PSScriptRoot 'DiskStudio.exe'
$manifest = Get-Content -LiteralPath (Join-Path $PSScriptRoot 'build-info.json') -Raw | ConvertFrom-Json
if ($manifest.sha256 -ne (Get-FileHash -LiteralPath $app -Algorithm SHA256).Hash) { throw 'The same production executable must be used in the guest' }

$windowsRoot = [IO.Path]::GetPathRoot($env:windir)
if ($archiveCase) {
    [IO.File]::WriteAllText((Join-Path $windowsRoot 'DSHotBackupProof.txt'), 'DiskStudio running Windows backup fixture')
    Write-Output "HOT BACKUP FIXTURE READY $($manifest.sha256)"
} else {
    [IO.File]::WriteAllText((Join-Path $windowsRoot 'DSHotMigrationProof.txt'), 'DiskStudio running Windows VSS fixture')
    Write-Output "HOT MIGRATION FIXTURE READY $($manifest.sha256)"
}
Start-Process -FilePath $app -WorkingDirectory $windowsRoot
