param([switch]$Verify, [switch]$Thin)
$ErrorActionPreference = 'Stop'
$machine = @(Get-CimInstance -Query 'SELECT UUID,Name,Vendor FROM Win32_ComputerSystemProduct')
if ($machine.Count -ne 1 -or $machine[0].UUID -ne 'e20adb9f-f665-432e-9a62-dd5d897b06a0' -or
    $machine[0].Name -ne 'DiskStudio-Test' -or $machine[0].Vendor -ne 'DiskStudio') {
    throw 'Run only in DiskStudio-Test after exact QMP preflight.'
}
$fixtureName = if ($Thin) { 'storage-spaces-recovery-thin.json' } else { 'storage-spaces-recovery-clean-cache.json' }
$fixture = Get-Content -LiteralPath (Join-Path $PSScriptRoot $fixtureName) -Raw | ConvertFrom-Json
$sourceDirectory = if ($Thin) { 'S:\spaces-thin' } else { 'S:\spaces' }
$outputDirectory = if ($Thin) { 'R:\thin-out' } else { 'R:\out' }
$serials = @('DS-TEST-A-0001      _00000001.', 'DS-TEST-B-0001      _00000001.')
$letters = @('S', 'R')
$legacy = @(Get-CimInstance -Query 'SELECT Index,SerialNumber,BytesPerSector FROM Win32_DiskDrive')
if ($legacy.Count -ne 3 -or @(Get-StoragePool | Where-Object { $_.IsPrimordial -ne $true }).Count -ne 0) {
    throw 'Expected the dedicated two ordinary data disks, plus the guest OS.'
}

function Find-TestDisk([int]$Slot) {
    $identity = @($legacy | Where-Object { ([string]$_.SerialNumber).Trim() -ceq $serials[$Slot] })
    if ($identity.Count -ne 1 -or $identity[0].Index -eq 0 -or $identity[0].BytesPerSector -ne 4096) {
        throw 'Guest device identity or geometry is invalid.'
    }
    $rows = @(Get-Disk -Number $identity[0].Index -ErrorAction Stop)
    if ($rows.Count -ne 1) { throw 'The guest reported an ambiguous disk.' }
    $disk = $rows[0]
    if (([string]$disk.SerialNumber).Trim() -cne $serials[$Slot] -or $disk.Size -ne 16GB -or
        $disk.LogicalSectorSize -ne 4096 -or $disk.PhysicalSectorSize -ne 4096 -or
        $disk.BusType -ne 'NVMe' -or $disk.IsBoot -ne $false -or $disk.IsSystem -ne $false -or
        $disk.IsClustered -ne $false -or $disk.IsReadOnly -ne $false -or $null -eq $disk.IsOffline) {
        throw 'The target is protected or does not match the disposable 4Kn NVMe device.'
    }
    $disk
}

function Confirm-TestVolume([int]$Slot) {
    $disk = Find-TestDisk $Slot
    $part = @(Get-Partition -DriveLetter $letters[$Slot] -ErrorAction Stop)
    $vol = @(Get-Volume -DriveLetter $letters[$Slot] -ErrorAction Stop)
    if ($disk.PartitionStyle -ne 'GPT' -or $disk.IsOffline -ne $false -or $part.Count -ne 1 -or
        $part[0].DiskNumber -ne $disk.Number -or $part[0].GptType -ne '{ebd0a0a2-b9e5-4433-87c0-68b6b72699c7}' -or
        $vol.Count -ne 1 -or $vol[0].FileSystem -ne 'NTFS' -or $vol[0].FileSystemLabel -ne "DS-SPACES-$Slot") {
        throw 'The fixture volume does not belong to the verified disposable disk.'
    }
}

function Confirm-Segments($member, [string]$slot) {
    if ($member.slot -cne $slot -or $member.capacity -ne 16GB) {
        throw 'Unexpected sparse member fixture.'
    }
    if ($Thin) {
        $middle = if ($slot -ceq 'A') {
            @(0xc1000000L, 0xc1002000L, 0xc1003000L, 0xc1004000L, 0xc1005000L, 0xc1006000L, 0xc1007000L,
              0xc1202000L, 0xc1203000L, 0xc1240000L, 0xc1270000L, 0xf0ff2000L, 0xf0ffa000L, 0xf0ffe000L, 0xf0fff000L)
        } else {
            @(0xc1240000L, 0xc127f000L, 0xe1000000L, 0xe1001000L, 0xe1002000L, 0xf0ff9000L, 0xf0ffa000L)
        }
        $expected = @(0, 16MB) + $middle + (16GB - 64KB)
        $sizes = @(64KB, 16KB) + (@(4KB) * $middle.Count) + 64KB
    } else {
        $mirror = 2164260864
        $expected = @(0, 16MB, 0x31000000, 0x41000000, 0x41002000, 0x41003000, $mirror,
            ($mirror + 1MB), ($mirror + 1GB - 20KB), (16GB - 64KB))
        $sizes = @(64KB, 16KB, 4KB, 4KB, 4KB, 4KB, 24KB, 128KB, 20KB, 64KB)
    }
    if ($member.segments.Count -ne $expected.Count) { throw 'Unexpected sparse member segments.' }
    for ($i = 0; $i -lt $expected.Count; $i++) {
        $segment = $member.segments[$i]
        if ($segment.offset -ne $expected[$i] -or $segment.data.Length -gt 180000) { throw 'Unexpected fixture offset.' }
        $bytes = [Convert]::FromBase64String($segment.data)
        $sha = [Security.Cryptography.SHA256]::Create()
        try { $digest = [BitConverter]::ToString($sha.ComputeHash($bytes)).Replace('-', '') } finally { $sha.Dispose() }
        if ($bytes.Length -ne $sizes[$i] -or $digest -ine $segment.sha256) { throw 'Sparse member segment mismatch.' }
    }
}

Confirm-Segments $fixture.members[0] 'A'
Confirm-Segments $fixture.members[1] 'B'
if (-not $Verify -and -not $Thin) {
    $disks = @(Find-TestDisk 0; Find-TestDisk 1)
    if ($disks[0].Number -eq $disks[1].Number -or
        @($disks | Where-Object { $_.PartitionStyle -ne 'RAW' -or $_.NumberOfPartitions -ne 0 }).Count -ne 0 -or
        @(Get-Volume | Where-Object { $_.DriveLetter -in $letters }).Count -ne 0) {
        throw 'The image fixture requires two distinct blank disks and unused drive letters.'
    }
    for ($slot = 0; $slot -lt 2; $slot++) {
        $disk = Find-TestDisk $slot
        if ($disk.PartitionStyle -ne 'RAW' -or $disk.NumberOfPartitions -ne 0) { throw 'Target layout changed.' }
        if ($disk.IsOffline) { $disk | Set-Disk -IsOffline $false -ErrorAction Stop }
        $disk = Find-TestDisk $slot
        $disk | Initialize-Disk -PartitionStyle GPT -ErrorAction Stop
        $disk = Find-TestDisk $slot
        $part = New-Partition -DiskNumber $disk.Number -UseMaximumSize -DriveLetter $letters[$slot] -ErrorAction Stop
        if ($part.DiskNumber -ne $disk.Number) { throw 'The returned partition is on the wrong disk.' }
        $part | Format-Volume -FileSystem NTFS -NewFileSystemLabel "DS-SPACES-$slot" -Confirm:$false -ErrorAction Stop | Out-Null
        Confirm-TestVolume $slot
    }

}
Confirm-TestVolume 0
Confirm-TestVolume 1
if (-not $Verify) {
    if ((Test-Path -LiteralPath $sourceDirectory) -or (Test-Path -LiteralPath $outputDirectory)) {
        throw 'The fixture source and output directories must be new.'
    }
    New-Item -ItemType Directory -Path $sourceDirectory, $outputDirectory -ErrorAction Stop | Out-Null
    foreach ($member in $fixture.members) {
        Confirm-TestVolume 0
        $path = Join-Path $sourceDirectory "Member-$($member.slot).raw"
        [IO.File]::Open($path,[IO.FileMode]::CreateNew).Dispose()
        & "$env:SystemRoot\System32\fsutil.exe" sparse setflag $path | Out-Null
        if ($LASTEXITCODE -ne 0 -or -not ((Get-Item -LiteralPath $path).Attributes -band [IO.FileAttributes]::SparseFile)) {
            throw 'The regular fixture file could not be made sparse.'
        }
        $stream = [IO.File]::Open($path,[IO.FileMode]::Open,[IO.FileAccess]::ReadWrite,[IO.FileShare]::None)
        try {
            $stream.SetLength($member.capacity)
            foreach ($segment in $member.segments) {
                $data = [Convert]::FromBase64String($segment.data)
                $stream.Position = $segment.offset
                $stream.Write($data, 0, $data.Length)
            }
            $stream.Flush($true)
        } finally { $stream.Dispose() }
    }
}

Confirm-TestVolume 0
Confirm-TestVolume 1
foreach ($member in $fixture.members) {
    $stream = [IO.File]::OpenRead((Join-Path $sourceDirectory "Member-$($member.slot).raw"))
    try {
        if ($stream.Length -ne $member.capacity) { throw 'Sparse member length mismatch.' }
        foreach ($segment in $member.segments) {
            $data = [Convert]::FromBase64String($segment.data)
            $stream.Position = $segment.offset
            $actual = New-Object byte[] $data.Length
            if ($stream.Read($actual,0,$data.Length) -ne $data.Length -or
                [Convert]::ToBase64String($actual) -cne $segment.data) { throw 'Sparse member data readback differs.' }
        }
    } finally { $stream.Dispose() }
}
if ($Verify) {
    $files = @(Get-ChildItem -LiteralPath $outputDirectory -File -Recurse)
    foreach ($record in @($fixture.existing, $fixture.deleted)) {
        $matches = @($files | Where-Object { $_.Length -eq $record.bytes -and (Get-FileHash -LiteralPath $_.FullName).Hash -ieq $record.sha256 })
        if ($matches.Count -ne 1) { throw 'Expected one exported file with the independent fixture digest.' }
        Write-Output "VERIFIED $($matches[0].Name) $($record.bytes) bytes $($record.sha256)"
    }
} else {
    "READY: read-only member images in $sourceDirectory and output directory $outputDirectory."
}
