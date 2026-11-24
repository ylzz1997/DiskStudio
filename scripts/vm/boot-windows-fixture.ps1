param(
    [ValidateSet('Gpt', 'Volumes')][string]$ContinueFrom,
    [switch]$PrepareMissingStore,
    [ValidateSet('Online', 'Offline')][string]$LoaderTest
)

$ErrorActionPreference = 'Stop'

$identity = @(Get-CimInstance -Query 'SELECT UUID,Name,Vendor FROM Win32_ComputerSystemProduct')
if ($identity.Count -ne 1 -or $identity[0].UUID -ne 'e20adb9f-f665-432e-9a62-dd5d897b06a0' -or
    $identity[0].Name -ne 'DiskStudio-Test' -or $identity[0].Vendor -ne 'DiskStudio') {
    throw 'Run this fixture only inside DiskStudio-Test after QMP preflight.'
}

$legacy = @(Get-CimInstance -Query 'SELECT Index,SerialNumber,BytesPerSector FROM Win32_DiskDrive')
$catalog = @(Get-Disk)
$system = @($catalog | Where-Object { ([string]$_.SerialNumber).Trim() -eq 'DS-OS-0001' })
if ($legacy.Count -ne 3 -or $catalog.Count -ne 3 -or $system.Count -ne 1 -or
    $system[0].Size -ne 80GB -or !$system[0].IsBoot -or !$system[0].IsSystem) {
    throw 'Preparation requires the original protected guest OS and the two test disks.'
}

function Get-LabDisk([string]$slot) {
    $serial = "DS-TEST-$slot-0001      _00000001."
    $member = @($legacy | Where-Object { ([string]$_.SerialNumber).Trim() -ceq $serial })
    if ($member.Count -ne 1 -or $member[0].BytesPerSector -ne 4096) {
        throw 'Unexpected test controller identity or geometry.'
    }
    $disk = Get-Disk -Number $member[0].Index
    $physical = @(Get-CimInstance -Namespace 'root/Microsoft/Windows/Storage' -Query 'SELECT SerialNumber,Size,BusType,LogicalSectorSize,PhysicalSectorSize FROM MSFT_PhysicalDisk' |
        Where-Object { ([string]$_.SerialNumber).Trim() -ceq $serial })
    if (([string]$disk.SerialNumber).Trim() -cne $serial -or $disk.Size -ne 16GB -or
        $disk.LogicalSectorSize -ne 4096 -or $disk.PhysicalSectorSize -ne 4096 -or
        $disk.CimInstanceProperties['BusType'].Value -ne 17 -or
        $disk.IsBoot -ne $false -or $disk.IsSystem -ne $false -or $disk.IsClustered -ne $false -or $disk.IsReadOnly -ne $false -or
        $physical.Count -ne 1 -or $physical[0].Size -ne 16GB -or $physical[0].CimInstanceProperties['BusType'].Value -ne 17 -or
        $physical[0].LogicalSectorSize -ne 4096 -or $physical[0].PhysicalSectorSize -ne 4096) {
        throw 'Test disk identity or protection disagrees with the verified fixture.'
    }
    $disk
}

$a = Get-LabDisk 'A'
$b = Get-LabDisk 'B'
$reservedType = [Guid]'e3c9e316-0b5c-4db8-817d-f92df00215ae'
$efiType = [Guid]'c12a7328-f81f-11d2-ba4b-00a0c93ec93b'
$basicType = [Guid]'ebd0a0a2-b9e5-4433-87c0-68b6b72699c7'

function Get-StagedLayout($disk) {
    $parts = @(Get-Partition -DiskNumber $disk.Number | Sort-Object Offset)
    if ($disk.PartitionStyle -ne 'GPT' -or $parts.Count -ne 3 -or
        [Guid]$parts[0].GptType -ne $reservedType -or $parts[0].Offset -ne 24KB -or
        $parts[0].Offset + $parts[0].Size -ne 16MB -or
        [Guid]$parts[1].GptType -ne $efiType -or $parts[1].Size -ne 272MB -or $parts[1].Offset -lt 16MB -or
        [Guid]$parts[2].GptType -ne $basicType -or $parts[2].Offset -lt ($parts[1].Offset + $parts[1].Size) -or
        $parts[2].Size -lt 15GB) {
        throw 'The prepared EFI and Windows partitions did not match the verified Test B layout.'
    }
    $parts
}

function Confirm-InitialLayout($disk) {
    if ($ContinueFrom -eq 'Volumes') {
        Get-StagedLayout $disk | Out-Null
    } elseif ($ContinueFrom -eq 'Gpt') {
        if ($disk.PartitionStyle -ne 'GPT' -or $disk.NumberOfPartitions -ne 1) {
            throw 'Continuation requires Test B to contain only the original Microsoft Reserved partition.'
        }
        $reserved = Get-Partition -DiskNumber $disk.Number
        if ([Guid]$reserved.GptType -ne $reservedType -or $reserved.Offset -ne 24KB -or
            $reserved.Offset + $reserved.Size -ne 16MB) {
            throw 'The initial Windows-created Microsoft Reserved partition differs.'
        }
    } elseif ($disk.PartitionStyle -ne 'RAW' -or $disk.NumberOfPartitions -ne 0) {
        throw 'Test B must be blank. Preparation never clears an existing partition table.'
    }
}
$backup = Get-Partition -DriveLetter D
$backupVolume = Get-Volume -DriveLetter D
if ($a.Number -eq $b.Number -or $backup.DiskNumber -ne $a.Number -or $a.IsOffline -or
    $backup.Size -ne 2GB -or $backupVolume.FileSystem -ne 'NTFS' -or $backupVolume.SizeRemaining -lt 512MB) {
    throw 'Test A must have its verified 2 GiB NTFS backup volume at D:.'
}
if ($LoaderTest) {
    if ($ContinueFrom -or $PrepareMissingStore) { throw 'Loader testing is separate from staging and repair.' }
    $baseline = Get-Content -LiteralPath 'D:\DS-Boot-Windows\staged.json' -Raw | ConvertFrom-Json
    $parts = @(Get-StagedLayout $b)
    if ($baseline.partitions.Count -ne $parts.Count -or [Guid]$baseline.disk -ne [Guid]$b.Guid) {
        throw 'The disk differs from the staged Test B.'
    }
    for ($slot = 0; $slot -lt $parts.Count; $slot++) {
        $prior = $baseline.partitions[$slot]
        $current = $parts[$slot]
        if ([Guid]$prior.Guid -ne [Guid]$current.Guid -or [Guid]$prior.GptType -ne [Guid]$current.GptType -or
            $prior.Offset -ne $current.Offset -or $prior.Size -ne $current.Size) {
            throw 'The GPT of Test B differs from the staged layout.'
        }
    }
    $expected = $baseline.installed.'Windows\Boot\EFI\bootmgfw.efi'
    if ($expected -cnotmatch '^[0-9a-f]{64}$') { throw 'Missing Windows loader digest in the staged witness.' }

    $offline = $LoaderTest -eq 'Offline'
    if ($b.IsOffline -ne $offline) { $b | Set-Disk -IsOffline $offline }
    $observed = Get-LabDisk 'B'
    $observedParts = @(Get-StagedLayout $observed)
    if ($observed.IsOffline -ne $offline -or [Guid]$observed.Guid -ne [Guid]$baseline.disk -or
        [Guid]$observedParts[1].Guid -ne [Guid]$baseline.partitions[1].Guid) {
        throw 'Unable to confirm the requested state and existing EFI partition on Test B.'
    }
    Write-Host "PASS: Test B $LoaderTest, EFI partition $($observedParts[1].PartitionNumber)."
    Write-Host "EFI Microsoft boot manager SHA-256: $expected"
    return
}
if ($PrepareMissingStore) {
    if ($ContinueFrom -or !$b.IsOffline) { throw 'Use an offline copy of the repaired Test B fixture.' }
    $files = 'D:\DS-Boot-Windows'
    $baseline = Get-Content -LiteralPath (Join-Path $files 'staged.json') -Raw | ConvertFrom-Json
    $expected = Get-StagedLayout $b
    if ([Guid]$baseline.disk -ne [Guid]$b.Guid -or
        [Guid]$baseline.partitions[1].Guid -ne [Guid]$expected[1].Guid -or
        [Guid]$baseline.partitions[2].Guid -ne [Guid]$expected[2].Guid) {
        throw 'The copy does not match the original Windows boot fixture.'
    }
    $archive = Join-Path $files 'EFI-files-before-repair.zip'
    $manifestJson = & "$env:SystemRoot\System32\tar.exe" -xOf $archive manifest.json
    if ($LASTEXITCODE -ne 0) { throw 'Could not read the original boot repair manifest.' }
    $manifest = $manifestJson | Out-String | ConvertFrom-Json
    $digest = @($manifest.files | Where-Object path -eq '\EFI\Microsoft\Boot\BCD')
    if ($digest.Count -ne 1 -or $digest[0].originalSha256 -or $digest[0].installedSha256 -notmatch '^[0-9a-f]{64}$') {
        throw 'Expected the manifest for the originally missing Windows BCD.'
    }
    if (Test-Path -LiteralPath 'S:\') { throw 'Temporary drive letter S: must be free.' }
    try {
        $b | Set-Disk -IsOffline $false
        $b = Get-LabDisk 'B'
        $esp = (Get-StagedLayout $b)[1]
        if ($b.IsOffline -or $esp.Guid -ne $expected[1].Guid) { throw 'The test EFI partition changed.' }
        if (!(Test-Path -LiteralPath 'S:\')) {
            $plan = Join-Path $files 'assign-test-b-efi.txt'
            @("select disk $($b.Number)", "select partition $($esp.PartitionNumber)", 'assign letter=S') |
                Set-Content -LiteralPath $plan -Encoding ASCII
            & "$env:SystemRoot\System32\diskpart.exe" /s $plan
            if ($LASTEXITCODE -ne 0) { throw 'Could not assign a test EFI access path.' }
        }
        $mounted = @(& "$env:SystemRoot\System32\mountvol.exe" 'S:\' /L |
            ForEach-Object { $_.Trim() } | Where-Object { $_ -match '^\\\\\?\\Volume\{[0-9a-f-]{36}\}\\$' })
        $esp = (Get-StagedLayout (Get-LabDisk 'B'))[1]
        if ($mounted.Count -ne 1 -or $esp.Guid -ne $expected[1].Guid -or
            ($mounted[0] -notin $esp.AccessPaths -and 'S:\' -notin $esp.AccessPaths)) {
            throw 'The temporary drive letter could not be bound to the verified EFI partition.'
        }
        $bcd = 'S:\EFI\Microsoft\Boot\BCD'
        if ((Get-FileHash -LiteralPath $bcd).Hash -ne $digest[0].installedSha256 -or
            (Test-Path -LiteralPath "$bcd.ds-first-run")) {
            throw 'The BCD does not match the saved repair or was already staged.'
        }
        [IO.File]::Move($bcd, "$bcd.ds-first-run")
        if (Test-Path -LiteralPath $bcd) { throw 'The original BCD path still exists.' }
    } finally {
        $b = Get-LabDisk 'B'
        if (!$b.IsOffline) { $b | Set-Disk -IsOffline $true }
        if (!(Get-LabDisk 'B').IsOffline) { throw 'Could not confirm Test B is offline.' }
    }
    Write-Host 'PASS: The previously rebuilt BCD is preserved beside its original path on offline Test B.'
    return
}
Confirm-InitialLayout $b

$logical = @(Get-CimInstance -Query 'SELECT DeviceID,DriveType FROM Win32_LogicalDisk')
if (@($logical | Where-Object DeviceID -eq 'W:').Count -ne 0) {
    throw 'The temporary drive letter W: must be free while Test B is offline.'
}
$media = @($logical | Where-Object DriveType -eq 5 | ForEach-Object {
    $candidate = "$($_.DeviceID)\sources\install.wim"
    if (Test-Path -LiteralPath $candidate -PathType Leaf) { $candidate }
})
if ($media.Count -ne 1) { throw 'Expected the Windows installation WIM on the read-only installer disc.' }

$workspacePath = 'D:\DS-Boot-Windows'
if ($ContinueFrom) {
    $workspace = Get-Item -LiteralPath $workspacePath
    $scratch = Get-Item -LiteralPath (Join-Path $workspacePath 'scratch')
    if (!$workspace.PSIsContainer -or !$scratch.PSIsContainer -or
        $workspace.Attributes -band [IO.FileAttributes]::ReparsePoint -or
        $scratch.Attributes -band [IO.FileAttributes]::ReparsePoint -or
        (Test-Path -LiteralPath (Join-Path $workspacePath 'staged.json'))) {
        throw 'Test A staging directory is missing or already has a complete staged result.'
    }
} else {
    $workspace = New-Item -ItemType Directory -Path $workspacePath
    $scratch = New-Item -ItemType Directory -Path (Join-Path $workspace.FullName 'scratch')
}
$requiredFree = 3GB
$env:TEMP = $scratch.FullName
$env:TMP = $scratch.FullName

try {
    if ($b.IsOffline) { $b | Set-Disk -IsOffline $false }
    $b = Get-LabDisk 'B'
    if ($b.IsOffline) { throw 'Test B did not come online for preparation.' }
    Confirm-InitialLayout $b
    if ($ContinueFrom -ne 'Volumes') {
        $plan = Join-Path $workspace.FullName $(if ($ContinueFrom -eq 'Gpt') { 'partition-test-b-after-gpt.txt' } else { 'partition-test-b.txt' })
        @(
            "select disk $($b.Number)"
            if (!$ContinueFrom) { 'convert gpt' }
            'create partition efi size=272'
            'format fs=fat32 quick unit=4096 label=DS-BOOT-EFI'
            'create partition primary'
            'format fs=ntfs quick unit=4096 label=DS-BOOT-WIN'
            'assign letter=W'
        ) | Set-Content -LiteralPath $plan -Encoding ASCII
        & "$env:SystemRoot\System32\diskpart.exe" /s $plan
        if ($LASTEXITCODE -ne 0) { throw "Fixture partition preparation failed with exit code $LASTEXITCODE." }
    }
    $b = Get-LabDisk 'B'
    $parts = @(Get-StagedLayout $b)
    if ($ContinueFrom -eq 'Volumes' -and $parts[2].DriveLetter -ne 'W') {
        Add-PartitionAccessPath -DiskNumber $b.Number -PartitionNumber $parts[2].PartitionNumber -AccessPath 'W:\'
    }
    $windows = Get-Partition -DriveLetter W
    if ($windows.DiskNumber -ne $b.Number -or $windows.Guid -ne $parts[2].Guid -or
        (Get-Volume -DriveLetter W).FileSystem -ne 'NTFS') {
        throw 'W: did not bind to the verified NTFS partition of Test B.'
    }
    $prior = @(Get-ChildItem -LiteralPath 'W:\' -Force | Where-Object Name -notin 'System Volume Information', '$RECYCLE.BIN')
    if ($prior.Count -ne 0) {
        throw 'The verified Windows volume already contains files. Preserve it and use a fresh fixture.'
    }
    $log = Join-Path $workspace.FullName 'apply-windows.log'
    & "$env:SystemRoot\System32\dism.exe" '/Apply-Image' "/ImageFile:$($media[0])" '/Index:1' '/ApplyDir:W:\' "/ScratchDir:$($scratch.FullName)" "/LogPath:$log" '/Verify'
    if ($LASTEXITCODE -ne 0) { throw "Applying the original Server Core image failed with exit code $LASTEXITCODE." }
    if ((Get-Partition -DriveLetter W).Guid -ne $windows.Guid -or
        (Get-Volume -DriveLetter W).SizeRemaining -lt $requiredFree) {
        throw 'The staged Windows volume has changed or has insufficient free capacity.'
    }
    $expected = @(
        'Windows\Boot\EFI\bootmgfw.efi'
        'Windows\System32\winload.efi'
        'Windows\System32\Config\BCD-Template'
        'Windows\System32\ntoskrnl.exe'
    )
    $files = [ordered]@{}
    foreach ($relative in $expected) {
        $hash = Get-FileHash -LiteralPath (Join-Path 'W:\' $relative) -Algorithm SHA256
        $files[$relative] = $hash.Hash.ToLowerInvariant()
    }
    @{
        disk = $b.Guid
        partitions = @($parts | Select-Object Guid, Offset, Size, GptType)
        installed = $files
    } | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath (Join-Path $workspace.FullName 'staged.json') -Encoding ASCII
} finally {
    $b = Get-LabDisk 'B'
    if (!$b.IsOffline) { $b | Set-Disk -IsOffline $true }
    if (!(Get-LabDisk 'B').IsOffline) { throw 'Could not confirm Test B is offline.' }
}

Write-Host 'PASS: Windows Server Core staged on offline Test B. Its EFI system partition has no boot files.'
Write-Host 'In DiskStudio, restore the x64 fallback file and rebuild the missing BCD. Save the manifest ZIP under D:\DS-Boot-Windows.'
