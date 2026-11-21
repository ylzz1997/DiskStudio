param(
    [switch]$Verify,
    [switch]$RecordBasicBaseline,
    [switch]$VerifyBasic,
    [switch]$Mixed
)

$ErrorActionPreference = 'Stop'
$system = @(Get-CimInstance -Query 'SELECT UUID,Name,Vendor FROM Win32_ComputerSystemProduct')
if ($system.Count -ne 1 -or $system[0].UUID -ne 'e20adb9f-f665-432e-9a62-dd5d897b06a0' -or
    $system[0].Name -ne 'DiskStudio-Test' -or $system[0].Vendor -ne 'DiskStudio') {
    throw 'Only run this fixture inside DiskStudio-Test after QMP preflight.'
}

$sourceSerial = 'DS-TEST-A-0001      _00000001.'
$destinationSerial = 'DS-TEST-B-0001      _00000001.'
$legacy = @(Get-CimInstance -Query 'SELECT Index,SerialNumber,BytesPerSector FROM Win32_DiskDrive')
if ($legacy.Count -ne 3) { throw 'The guest must have exactly three disks.' }
function Get-UniqueMember($serial) {
    $match = @($legacy | Where-Object { ([string]$_.SerialNumber).Trim() -eq $serial })
    if ($match.Count -ne 1) { throw "Guest disk identity did not resolve: $serial" }
    $match[0]
}
$sourceLegacy = Get-UniqueMember $sourceSerial
$destinationLegacy = Get-UniqueMember $destinationSerial
if ($sourceLegacy.Index -eq $destinationLegacy.Index) { throw 'The test disks must be distinct.' }
if ($Mixed -and ($sourceLegacy.BytesPerSector -ne 512 -or $destinationLegacy.BytesPerSector -ne 512 -or $Verify)) {
    throw 'The mixed conversion fixture requires the dedicated 512-byte test disks.'
}
$mixedVolumes = @(
    @{ Letter = 'Q'; Label = 'DSLDMFAT32'; FileSystem = 'FAT32'; Length = 320MB },
    @{ Letter = 'S'; Label = 'DSLDMEXFAT'; FileSystem = 'exFAT'; Length = 192MB },
    @{ Letter = 'T'; Label = 'DSLDMNTFS'; FileSystem = 'NTFS'; Length = 256MB }
)

function Get-BasicFixture($member, $serial) {
    $disk = Get-Disk -Number $member.Index
    if ($disk.SerialNumber.Trim() -ne $serial -or $disk.Size -ne 16GB -or $disk.IsBoot -ne $false -or
        $disk.IsSystem -ne $false -or $disk.IsClustered -ne $false -or
        $disk.IsReadOnly -ne $false -or $disk.LogicalSectorSize -notin 512,4096 -or
        $disk.LogicalSectorSize -ne $member.BytesPerSector) {
        throw 'The disposable disk did not match its full guest identity and geometry.'
    }
    $disk
}

function Initialize-BlankFixture($disk) {
    if ($disk.PartitionStyle -ne 'RAW' -or $disk.NumberOfPartitions -ne 0) {
        throw 'Fixture setup requires a blank dedicated disk.'
    }
    if ($disk.IsOffline) { $disk | Set-Disk -IsOffline $false }
    Initialize-Disk -Number $disk.Number -PartitionStyle GPT
}

function New-FixtureVolume($disk, $letter, $label, $size, $filesystem = 'NTFS') {
    $part = New-Partition -DiskNumber $disk.Number -Size $size -DriveLetter $letter
    $after = Get-Partition -DriveLetter $letter
    if ($after.DiskNumber -ne $disk.Number -or $part.DiskNumber -ne $disk.Number -or
        $after.PartitionNumber -ne $part.PartitionNumber -or $after.Size -ne $size) {
        throw 'New volume did not bind to the requested test disk.'
    }
    $part | Format-Volume -FileSystem $filesystem -AllocationUnitSize 4096 -NewFileSystemLabel $label -Confirm:$false | Out-Null
    $volume = Get-Volume -DriveLetter $letter
    if ($volume.FileSystemLabel -ne $label -or $volume.FileSystem -ne $filesystem) {
        throw 'Fixture format did not produce the requested file system.'
    }
    $after
}

function Write-Witness($letter, $seed) {
    $root = "${letter}:\nested"
    New-Item -ItemType Directory -Path $root | Out-Null
    $witness = Join-Path $root 'ldm-witness.bin'
    $bytes = New-Object byte[] (192KB + 17)
    for ($index = 0; $index -lt $bytes.Length; $index++) {
        $bytes[$index] = ($index * 31 + ($index -shr 9) + $seed) -band 255
    }
    [IO.File]::WriteAllBytes($witness, $bytes)
    (Get-FileHash -LiteralPath $witness -Algorithm SHA256).Hash
}

function Read-VolumeDigest($file, $offset, $length, $sector, $filesystem) {
    if ($offset -lt 1MB -or $offset -gt (16GB - $length) -or $offset % $sector -ne 0 -or
        $length -le 0 -or $length % 4MB -ne 0) {
        throw 'The witness partition is outside the expected data range.'
    }
    $hash = [Security.Cryptography.SHA256]::Create()
    try {
        $file.Position = $offset
        $buffer = New-Object byte[] (4MB)
        for ($done = 0; $done -lt $length; $done += $buffer.Length) {
            if ($file.Read($buffer, 0, $buffer.Length) -ne $buffer.Length) {
                throw 'Short read during full volume verification.'
            }
            if ($done -eq 0) {
                $signature = if ($filesystem -eq 'FAT32') { 82 } else { 3 }
                if ([Text.Encoding]::ASCII.GetString($buffer, $signature, 8).TrimEnd() -ne $filesystem) {
                    throw "The expected $filesystem boot structure is absent."
                }
            }
            $null = $hash.TransformBlock($buffer, 0, $buffer.Length, $buffer, 0)
        }
        $null = $hash.TransformFinalBlock([byte[]]::new(0), 0, 0)
        [BitConverter]::ToString($hash.Hash).Replace('-', '')
    } finally {
        $hash.Dispose()
    }
}

function Read-ProtectedGpt($file, $sector) {
    $header = New-Object byte[] (2 * $sector)
    $file.Position = 0
    if ($file.Read($header, 0, $header.Length) -ne $header.Length -or
        [Text.Encoding]::ASCII.GetString($header, $sector, 8) -ne 'EFI PART') {
        throw 'The fixture did not report a GPT header.'
    }
    $tableLba = [BitConverter]::ToUInt64($header, $sector + 72)
    $entries = [BitConverter]::ToUInt32($header, $sector + 80)
    if ($tableLba -ne 2 -or $entries -ne 128 -or [BitConverter]::ToUInt32($header, $sector + 84) -ne 128) {
        throw 'The fixture GPT does not have the expected geometry.'
    }
    $table = New-Object byte[] (128 * 128)
    if ($file.Read($table, 0, $table.Length) -ne $table.Length) { throw 'Short GPT read.' }
    $reservedType = [Guid]'e3c9e316-0b5c-4db8-817d-f92df00215ae'
    $reserved = @(
        for ($index = 0; $index -lt $entries; $index++) {
            $slot = $index * 128
            if ([Guid]::new([byte[]]($table[$slot..($slot + 15)])) -eq $reservedType) {
                [Convert]::ToBase64String($table, $slot, 128)
            }
        }
    )
    if ($reserved.Count -ne 1) { throw 'Expected one Microsoft Reserved partition in the fixture.' }
    @{
        Pmbr = [Convert]::ToBase64String($header, 0, $sector)
        GptId = [Convert]::ToBase64String($header, $sector + 56, 16)
        Reserved = $reserved[0]
    }
}

function Show-DynamicMetadata {
    $modern = @(Get-CimInstance -Namespace 'root/Microsoft/Windows/Storage' -Query 'SELECT Number,SerialNumber,PartitionStyle,LogicalSectorSize,IsOffline FROM MSFT_Disk')
    $physical = @(Get-CimInstance -Namespace 'root/Microsoft/Windows/Storage' -Query 'SELECT DeviceId,SerialNumber,Size,LogicalSectorSize,PhysicalSectorSize,BusType FROM MSFT_PhysicalDisk')
    $modern | Where-Object { $_.Number -in $sourceLegacy.Index,$destinationLegacy.Index } |
        Format-Table Number,SerialNumber,PartitionStyle,LogicalSectorSize,IsOffline
    $physical | Where-Object { ([string]$_.SerialNumber).Trim() -in $sourceSerial,$destinationSerial } |
        Format-Table DeviceId,SerialNumber,Size,LogicalSectorSize,PhysicalSectorSize,BusType
    Get-CimInstance -Query 'SELECT DiskIndex,Index,Type,StartingOffset,Size FROM Win32_DiskPartition' |
        Where-Object { $_.DiskIndex -eq $sourceLegacy.Index } |
        Format-Table DiskIndex,Index,Type,StartingOffset,Size
}

$destination = Get-BasicFixture $destinationLegacy $destinationSerial
if (@($Verify, $RecordBasicBaseline, $VerifyBasic | Where-Object { $_ }).Count -gt 1) {
    throw 'Select one verification phase.'
}
if ($RecordBasicBaseline -or $VerifyBasic) {
    if ($destination.IsOffline -or (Get-Partition -DriveLetter R).DiskNumber -ne $destination.Number -or
        (Get-Volume -DriveLetter R).FileSystemLabel -ne 'DSLDMEXPORT') {
        throw 'The proof directory does not belong to the expected B volume.'
    }
    $physical = @(Get-CimInstance -Namespace 'root/Microsoft/Windows/Storage' -Query 'SELECT SerialNumber,Size,LogicalSectorSize FROM MSFT_PhysicalDisk' |
        Where-Object { ([string]$_.SerialNumber).Trim() -eq $sourceSerial })
    if ($physical.Count -ne 1 -or $physical[0].Size -ne 16GB -or
        $physical[0].LogicalSectorSize -ne $sourceLegacy.BytesPerSector -or
        $physical[0].LogicalSectorSize -notin 512,4096) {
        throw 'Cannot uniquely resolve A geometry for readback.'
    }
    $sourcePartitions = @(Get-CimInstance -Query 'SELECT DiskIndex,Type,StartingOffset,Size,BootPartition FROM Win32_DiskPartition' |
        Where-Object { $_.DiskIndex -eq $sourceLegacy.Index })
    if ($Mixed) {
        $proofPath = 'R:\ldm-mixed-conversion-proof.json'
        $seedDocument = Get-Content -LiteralPath 'R:\ldm-mixed-fixture.json' -Raw | ConvertFrom-Json
        $seed = @($seedDocument.Volumes)
        if ($seed.Count -ne $mixedVolumes.Count) { throw 'The mixed fixture seed is incomplete.' }
        $prior = 0
        for ($index = 0; $index -lt $seed.Count; $index++) {
            $spec = $mixedVolumes[$index]
            $part = $seed[$index]
            if ($part.FileSystem -ne $spec.FileSystem -or $part.Length -ne $spec.Length -or
                $part.Offset -lt $prior) { throw 'The fixture seed does not describe the fixed mixed volumes.' }
            $prior = $part.Offset + $part.Length
        }
        if ($VerifyBasic) {
            $basic = Get-BasicFixture $sourceLegacy $sourceSerial
            $proof = Get-Content -LiteralPath $proofPath -Raw | ConvertFrom-Json
            if (-not $basic.IsOffline -or $basic.PartitionStyle -ne 'GPT' -or
                $sourcePartitions.Count -ne $seed.Count -or $proof.Volumes.Count -ne $seed.Count -or
                @($sourcePartitions | Where-Object { $_.Type -ne 'GPT: Basic Data' -or $_.BootPartition -ne $false }).Count -ne 0) {
                throw 'The guest does not report the expected offline basic GPT data partitions.'
            }
        } else {
            $data = @($sourcePartitions | Where-Object Type -eq 'GPT: Logical Disk Manager Data')
            if ($sourcePartitions.Count -ne 2 -or $data.Count -ne 1 -or $data[0].BootPartition -ne $false -or
                (Test-Path -LiteralPath $proofPath) -or
                @($seed | Where-Object {
                    $_.Offset -lt $data[0].StartingOffset -or $_.Offset + $_.Length -gt $data[0].StartingOffset + $data[0].Size
                }).Count -ne 0) {
                throw 'Need the original mixed dynamic fixture and a fresh proof path.'
            }
        }
        $sourceFile = [IO.File]::Open(('\\.\PhysicalDrive' + $sourceLegacy.Index), 'Open', 'Read', 'ReadWrite')
        try {
            $protected = Read-ProtectedGpt $sourceFile $sourceLegacy.BytesPerSector
            if ($VerifyBasic -and ($protected.Pmbr -cne $proof.Protected.Pmbr -or
                $protected.GptId -cne $proof.Protected.GptId -or $protected.Reserved -cne $proof.Protected.Reserved)) {
                throw 'The protective MBR, disk GUID or reserved GPT entry changed.'
            }
            $volumes = @(
                for ($index = 0; $index -lt $seed.Count; $index++) {
                    $part = $seed[$index]
                    if ($VerifyBasic -and @($sourcePartitions | Where-Object {
                        $_.StartingOffset -eq $part.Offset -and $_.Size -eq $part.Length
                    }).Count -ne 1) { throw 'The converted partition does not match its original extent.' }
                    $sha = Read-VolumeDigest $sourceFile $part.Offset $part.Length $sourceLegacy.BytesPerSector $part.FileSystem
                    if ($VerifyBasic -and $sha -ne $proof.Volumes[$index].Sha256) {
                        throw "Converted $($part.FileSystem) SHA-256 differs from the baseline."
                    }
                    @{ FileSystem = $part.FileSystem; Offset = $part.Offset; Length = $part.Length; Sha256 = $sha }
                }
            )
        } finally {
            $sourceFile.Dispose()
        }
        if ($VerifyBasic) {
            Write-Output 'VERIFIED: three offline GPT data partitions, protective MBR, disk GUID and reserved entry.'
        } else {
            @{ Protected = $protected; Volumes = $volumes } | ConvertTo-Json -Depth 4 |
                Set-Content -LiteralPath $proofPath -Encoding ascii
            Write-Output 'BASELINE: three original dynamic volumes.'
        }
        foreach ($volume in $volumes) {
            Write-Output "$($volume.FileSystem) $($volume.Offset) $($volume.Length) $($volume.Sha256)"
        }
        return
    }
    $proofPath = 'R:\ldm-conversion-proof.json'
    $length = 256MB
    if ($VerifyBasic) {
        $basic = Get-BasicFixture $sourceLegacy $sourceSerial
        $proof = Get-Content -LiteralPath $proofPath -Raw | ConvertFrom-Json
        if (-not $basic.IsOffline -or $basic.PartitionStyle -ne 'GPT' -or $sourcePartitions.Count -ne 1 -or
            $sourcePartitions[0].BootPartition -ne $false -or
            $sourcePartitions[0].Type -ne 'GPT: Basic Data' -or $sourcePartitions[0].StartingOffset -ne $proof.Offset -or
            $sourcePartitions[0].Size -ne $length -or $proof.Length -ne $length) {
            throw 'The guest does not report the expected offline basic GPT partition.'
        }
        $offset = [long]$proof.Offset
    } else {
        $data = @($sourcePartitions | Where-Object Type -eq 'GPT: Logical Disk Manager Data')
        if ($sourcePartitions.Count -ne 2 -or $data.Count -ne 1 -or $data[0].BootPartition -ne $false -or
            $data[0].Size -lt $length -or (Test-Path -LiteralPath $proofPath)) {
            throw 'Need the original dynamic fixture and a fresh proof path.'
        }
        $offset = [long]$data[0].StartingOffset
    }
    # Open only the fully corroborated disposable A disk. The exact range is read-only.
    $sourceFile = [IO.File]::Open(('\\.\PhysicalDrive' + $sourceLegacy.Index), 'Open', 'Read', 'ReadWrite')
    try {
        $sha = Read-VolumeDigest $sourceFile $offset $length $sourceLegacy.BytesPerSector 'NTFS'
    } finally {
        $sourceFile.Dispose()
    }
    if ($VerifyBasic) {
        if ($sha -ne $proof.Sha256) { throw 'Converted volume SHA-256 differs from the baseline.' }
        Write-Output "VERIFIED OFFLINE BASIC GPT: offset $offset, length $length, SHA-256 $sha."
    } else {
        @{ Offset = $offset; Length = $length; Sha256 = $sha } | ConvertTo-Json |
            Set-Content -LiteralPath $proofPath -Encoding ascii
        Write-Output "BASELINE: offset $offset, length $length, SHA-256 $sha."
    }
    return
}
if ($Verify) {
    $target = Get-Partition -DriveLetter R
    if ($destination.IsOffline -or $target.DiskNumber -ne $destination.Number -or
        (Get-Volume -DriveLetter R).FileSystemLabel -ne 'DSLDMEXPORT') {
        throw 'The export directory no longer belongs to the expected B volume.'
    }
    $proof = Get-Content -LiteralPath 'R:\ldm-proof.json' -Raw | ConvertFrom-Json
    $outputs = @(Get-ChildItem -LiteralPath 'R:\out' -Recurse -File -Filter 'ldm-witness.bin')
    if ($outputs.Count -ne 1 -or $outputs[0].Length -ne $proof.Length -or
        (Get-FileHash -LiteralPath $outputs[0].FullName -Algorithm SHA256).Hash -ne $proof.Sha256) {
        throw 'Expected exactly one independently verified exported witness.'
    }
    Write-Output "VERIFIED: $($proof.Length) bytes, SHA-256 $($proof.Sha256)."
    Show-DynamicMetadata
    return
}

$source = Get-BasicFixture $sourceLegacy $sourceSerial
if ($source.PartitionStyle -ne 'RAW' -or $destination.PartitionStyle -ne 'RAW' -or
    $source.NumberOfPartitions -ne 0 -or $destination.NumberOfPartitions -ne 0 -or
    @(Get-Volume | Where-Object { $_.DriveLetter -in @('Q','R') -or
        ($Mixed -and $_.DriveLetter -in @('S','T')) }).Count -ne 0) {
    throw 'Need two blank dedicated disks and unused fixture drive letters.'
}
$source,$destination | Format-Table Number,SerialNumber,Size,LogicalSectorSize,IsOffline
Initialize-BlankFixture $destination
New-FixtureVolume $destination R 'DSLDMEXPORT' 512MB | Out-Null
Initialize-BlankFixture $source
if ($Mixed) {
    $seed = @(
        for ($index = 0; $index -lt $mixedVolumes.Count; $index++) {
            $spec = $mixedVolumes[$index]
            $part = New-FixtureVolume $source $spec.Letter $spec.Label $spec.Length $spec.FileSystem
            $sha = Write-Witness $spec.Letter (17 * ($index + 1))
            @{ FileSystem = $spec.FileSystem; Offset = $part.Offset; Length = $part.Size; Witness = $sha }
        }
    )
    @{ Volumes = $seed } | ConvertTo-Json -Depth 4 |
        Set-Content -LiteralPath 'R:\ldm-mixed-fixture.json' -Encoding ascii
} else {
    New-FixtureVolume $source Q 'DSLDMSEED' 256MB | Out-Null
    New-Item -ItemType Directory -Path 'R:\out' | Out-Null
    $sha = Write-Witness Q 0
    @{ Length = 192KB + 17; Sha256 = $sha } | ConvertTo-Json |
        Set-Content -LiteralPath 'R:\ldm-proof.json' -Encoding ascii
}

# Pass the disk number only after rechecking both the selected device and mounted witness.
$freshSource = Get-BasicFixture $sourceLegacy $sourceSerial
if ($freshSource.IsOffline) {
    throw 'Source disk or witness volume changed before conversion.'
}
$expected = if ($Mixed) { $mixedVolumes } else { @{ Letter = 'Q'; Label = 'DSLDMSEED'; FileSystem = 'NTFS'; Length = 256MB } }
foreach ($spec in $expected) {
    $part = Get-Partition -DriveLetter $spec.Letter
    $volume = Get-Volume -DriveLetter $spec.Letter
    if ($part.DiskNumber -ne $freshSource.Number -or $part.Size -ne $spec.Length -or
        $volume.FileSystemLabel -ne $spec.Label -or $volume.FileSystem -ne $spec.FileSystem) {
        throw 'Source disk or witness volume changed before conversion.'
    }
}
$convert = 'R:\ldm-prepare.txt'
@("select disk $($freshSource.Number)", 'convert dynamic', 'offline disk', 'detail disk', 'exit') |
    Set-Content -LiteralPath $convert -Encoding ascii
& diskpart.exe /s $convert
if ($LASTEXITCODE -ne 0) { throw "DiskPart stopped with exit code $LASTEXITCODE." }
if ($Mixed) {
    Write-Output 'FIXTURE: mixed FAT32, exFAT and NTFS volumes.'
} else {
    Write-Output "FIXTURE: witness $(192KB + 17) bytes, SHA-256 $sha."
}
Show-DynamicMetadata
