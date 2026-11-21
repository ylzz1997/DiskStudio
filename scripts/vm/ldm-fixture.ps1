param([switch]$Verify)

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

function New-FixtureVolume($disk, $letter, $label, $size) {
    if ($disk.PartitionStyle -ne 'RAW' -or $disk.NumberOfPartitions -ne 0) {
        throw 'Fixture setup requires a blank dedicated disk.'
    }
    if ($disk.IsOffline) { $disk | Set-Disk -IsOffline $false }
    Initialize-Disk -Number $disk.Number -PartitionStyle GPT
    $part = New-Partition -DiskNumber $disk.Number -Size $size -DriveLetter $letter
    $part | Format-Volume -FileSystem NTFS -NewFileSystemLabel $label -Confirm:$false | Out-Null
    $after = Get-Partition -DriveLetter $letter
    if ($after.DiskNumber -ne $disk.Number -or $after.PartitionNumber -ne $part.PartitionNumber -or
        (Get-Volume -DriveLetter $letter).FileSystemLabel -ne $label) {
        throw 'New volume did not bind to the requested test disk.'
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
    @(Get-Volume | Where-Object DriveLetter -in Q,R).Count -ne 0) {
    throw 'Need two blank dedicated disks and unused Q and R drive letters.'
}
$source,$destination | Format-Table Number,SerialNumber,Size,LogicalSectorSize,IsOffline
New-FixtureVolume $destination R 'DSLDMEXPORT' 512MB
New-FixtureVolume $source Q 'DSLDMSEED' 256MB
New-Item -ItemType Directory -Path 'Q:\nested','R:\out' | Out-Null

$witness = 'Q:\nested\ldm-witness.bin'
$bytes = New-Object byte[] (192KB + 17)
for ($index = 0; $index -lt $bytes.Length; $index++) { $bytes[$index] = ($index * 31 + ($index -shr 9)) -band 255 }
[IO.File]::WriteAllBytes($witness, $bytes)
$sha = (Get-FileHash -LiteralPath $witness -Algorithm SHA256).Hash
@{ Length = $bytes.Length; Sha256 = $sha } | ConvertTo-Json |
    Set-Content -LiteralPath 'R:\ldm-proof.json' -Encoding ascii

# Pass the disk number only after rechecking both the selected device and mounted witness.
$freshSource = Get-BasicFixture $sourceLegacy $sourceSerial
if ((Get-Partition -DriveLetter Q).DiskNumber -ne $freshSource.Number -or $freshSource.IsOffline -or
    (Get-Volume -DriveLetter Q).FileSystemLabel -ne 'DSLDMSEED') {
    throw 'Source disk or witness volume changed before conversion.'
}
$convert = 'R:\ldm-prepare.txt'
@("select disk $($freshSource.Number)", 'convert dynamic', 'offline disk', 'detail disk', 'exit') |
    Set-Content -LiteralPath $convert -Encoding ascii
& diskpart.exe /s $convert
if ($LASTEXITCODE -ne 0) { throw "DiskPart stopped with exit code $LASTEXITCODE." }
Write-Output "FIXTURE: witness $($bytes.Length) bytes, SHA-256 $sha."
Show-DynamicMetadata
