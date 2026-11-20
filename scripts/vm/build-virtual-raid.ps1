param([switch]$Devices, [switch]$Md6, [switch]$MdMetadata, [switch]$MdZones, [switch]$Md10, [switch]$Md10Advanced)
$ErrorActionPreference = 'Stop'
Set-Location -LiteralPath ([IO.Path]::GetFullPath((Join-Path $PSScriptRoot '../..')))
if (@(@($Devices, $Md6, $MdMetadata, $MdZones, $Md10, $Md10Advanced) | Where-Object { $_ }).Count -gt 1) { throw 'Choose one fixture type' }
$fixtureDir = if ($Devices) { '.vm/virtual-raid-devices' }
    elseif ($Md6) { '.vm/virtual-raid-md6' }
    elseif ($MdMetadata) { '.vm/virtual-raid-md-metadata' }
    elseif ($MdZones) { '.vm/virtual-raid-md-zones' }
    elseif ($Md10) { '.vm/virtual-raid-md10' }
    elseif ($Md10Advanced) { '.vm/virtual-raid-md10-advanced' }
    else { '.vm/virtual-raid-native' }
$manifest=Get-Content -LiteralPath (Join-Path $fixtureDir 'manifest.json') -Raw | ConvertFrom-Json
$desktop=(Get-FileHash -LiteralPath 'target/release/DiskStudio.exe').Hash.ToLowerInvariant()
if ((Get-FileHash -LiteralPath '.vm/artifacts/DiskStudio.exe').Hash -ne $desktop) { throw 'Desktop archive mismatch' }
$payload=Get-Item -LiteralPath '.vm/payload'
if (!$payload.PSIsContainer -or ($payload.Attributes -band [IO.FileAttributes]::ReparsePoint)) { throw 'Invalid payload location' }
$names = if ($Devices) { @('Virtual-Raid-Device-Fresh-Prepare.exe','Virtual-Raid-Device-Fresh-Inspect.exe') }
    elseif ($Md6) { @('Virtual-Raid-Md6-Prepare.exe','Virtual-Raid-Md6-Inspect.exe') }
    elseif ($MdMetadata) { @('Virtual-Raid-Md-Metadata-Prepare.exe','Virtual-Raid-Md-Metadata-Inspect.exe') }
    elseif ($MdZones) { @('Virtual-Raid-Md-Zones-Prepare.exe','Virtual-Raid-Md-Zones-Inspect.exe') }
    elseif ($Md10) { @('Virtual-Raid-Md10-Stage.exe','Virtual-Raid-Md10-Verify.exe') }
    elseif ($Md10Advanced) { @('Virtual-Raid-Md10-Advanced-Stage.exe','Virtual-Raid-Md10-Advanced-Verify.exe') }
    else { @('Virtual-Raid-4Kn-Prepare.exe','Virtual-Raid-4Kn-Inspect.exe') }
$expectedFiles = if ($Devices) { @('Virtual-Raid-Device-1.raw','Virtual-Raid-Device-2.raw','Virtual-Raid-Device-Whole.raw') }
    elseif ($Md6) { @(0..4 | ForEach-Object { "Virtual-Raid-Md6-$_.raw" }) }
    elseif ($MdMetadata) { @(0..4 | ForEach-Object { "Virtual-Raid-Md-native-$_.raw" }) }
    elseif ($MdZones) { @(0..2 | ForEach-Object { "Virtual-Raid-Md-alternate-$_.raw" }) }
    elseif ($Md10) { @(0..4 | ForEach-Object { "Virtual-Raid-Md-o2-$_.raw" }) }
    elseif ($Md10Advanced) { @(0..5 | ForEach-Object { "Virtual-Raid-Md-mixed-offset-$_.raw" }) }
    else { @('Virtual-Raid-1.raw','Virtual-Raid-2.raw','Virtual-Raid-3.raw') }
$sizes = if ($Devices) { @(32MB,32MB,64MB) }
    elseif ($Md6) { @(12MB,12MB,12MB,12MB,12MB) }
    elseif ($MdMetadata -or $Md10 -or $Md10Advanced) { @(16MB) * $expectedFiles.Count }
    elseif ($MdZones) { @(20MB,6MB,13MB) }
    else { @(25MB,25MB,25MB) }
if ($manifest.files.Count -ne $expectedFiles.Count) { throw 'Unexpected number of RAID fixtures' }
$needsProof = $Md6 -or $MdMetadata -or $MdZones -or $Md10 -or $Md10Advanced
if ($needsProof -and $manifest.proofSha256 -cnotmatch '^[a-f0-9]{64}$') { throw 'Missing Linux file digest' }
$hashes = foreach($index in 0..($expectedFiles.Count-1)) {
    $item=$manifest.files[$index]
    $leaf=$expectedFiles[$index]
    $path=Join-Path $fixtureDir $leaf
    $data=Get-Item -LiteralPath $path
    if($item.name -cne $leaf -or $data.Length -ne $sizes[$index] -or ($data.Attributes -band [IO.FileAttributes]::ReparsePoint) -or
       $item.sha256 -cnotmatch '^[a-f0-9]{64}$' -or (Get-FileHash -LiteralPath $path).Hash -ne $item.sha256) { throw 'Bad virtual RAID member' }
    $item.sha256
}
foreach($leaf in $names) {
    if(Test-Path -LiteralPath (Join-Path $payload.FullName $leaf)) { throw "Preserve existing payload $leaf" }
}
foreach($item in $manifest.files) {
    $target=Join-Path $payload.FullName $item.name
    if(Test-Path -LiteralPath $target) {
        $entry=Get-Item -LiteralPath $target
        if(!($Devices -or $Md10 -or $Md10Advanced) -or $entry.PSIsContainer -or ($entry.Attributes -band [IO.FileAttributes]::ReparsePoint) -or
           (Get-FileHash -LiteralPath $target).Hash -ne $item.sha256) { throw "Preserve existing payload $($item.name)" }
    }
}
$desktopBefore = $env:DISKSTUDIO_ACCEPTANCE_DESKTOP_SHA256
$membersBefore = $env:DISKSTUDIO_VIRTUAL_RAID_MEMBER_SHA256
$proofBefore = $env:DISKSTUDIO_VIRTUAL_RAID_MD6_FILE_SHA256
$flagsBefore = $env:RUSTFLAGS
try {
    $env:DISKSTUDIO_ACCEPTANCE_DESKTOP_SHA256=$desktop
    $env:DISKSTUDIO_VIRTUAL_RAID_MEMBER_SHA256=$hashes -join ','
    if ($needsProof) { $env:DISKSTUDIO_VIRTUAL_RAID_MD6_FILE_SHA256=$manifest.proofSha256 }
    $env:RUSTFLAGS='-C target-feature=+crt-static'
    cargo build --locked --release --manifest-path scripts/vm/guest-checks/Cargo.toml --target-dir target/guest-checks
    if($LASTEXITCODE -ne 0) { throw 'Virtual RAID acceptance helper build failed' }
    foreach($leaf in $names) {
        Copy-Item -LiteralPath 'target/guest-checks/release/diskstudio-guest-checks.exe' -Destination (Join-Path $payload.FullName $leaf)
    }
    foreach($item in $manifest.files) {
        $target=Join-Path $payload.FullName $item.name
        if(!(Test-Path -LiteralPath $target)){ Copy-Item -LiteralPath (Join-Path $fixtureDir $item.name) -Destination $target }
        if((Get-FileHash -LiteralPath $target).Hash -ne $item.sha256) { throw 'Copied member digest differs' }
    }
} finally {
    $env:DISKSTUDIO_ACCEPTANCE_DESKTOP_SHA256=$desktopBefore
    $env:DISKSTUDIO_VIRTUAL_RAID_MEMBER_SHA256=$membersBefore
    $env:DISKSTUDIO_VIRTUAL_RAID_MD6_FILE_SHA256=$proofBefore
    $env:RUSTFLAGS=$flagsBefore
}
