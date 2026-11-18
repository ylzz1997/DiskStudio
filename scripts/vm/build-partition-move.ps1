[CmdletBinding(DefaultParameterSetName = 'First')]
param(
    [Parameter(ParameterSetName = 'Diagnose', Mandatory)] [switch]$Diagnose,
    [Parameter(ParameterSetName = 'Stop', Mandatory)] [switch]$Stop,
    [Parameter(ParameterSetName = 'FourKn', Mandatory)] [switch]$FourKn,
    [Parameter(ParameterSetName = 'FourKnDiagnose', Mandatory)] [switch]$FourKnDiagnose,
    [Parameter(ParameterSetName = 'FourKnFat', Mandatory)] [switch]$FourKnFat,
    [Parameter(ParameterSetName = 'FourKnFatContinue', Mandatory)] [switch]$FourKnFatContinue
)

$ErrorActionPreference = 'Stop'
$diskStudioRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..\..'))
$previousFlags = $env:RUSTFLAGS
$previousDigest = $env:DISKSTUDIO_ACCEPTANCE_DESKTOP_SHA256
Push-Location $diskStudioRoot
try {
    $desktop = (Get-FileHash -LiteralPath 'target/release/diskstudio.exe' -Algorithm SHA256).Hash.ToLowerInvariant()
    if ((Get-FileHash -LiteralPath '.vm/artifacts/DiskStudio.exe' -Algorithm SHA256).Hash.ToLowerInvariant() -ne $desktop) {
        throw 'Desktop artifact mismatch'
    }
    $payload = Get-Item -LiteralPath '.vm/payload'
    if ($payload.Attributes -band [IO.FileAttributes]::ReparsePoint) { throw 'Payload is a link' }
    $entries = if ($Diagnose) { @('Restart-Diagnose') }
        elseif ($Stop) { @('Stop-Prepare', 'Stop-Interrupted', 'Stop-Inspect') }
        elseif ($FourKn) { @('4Kn-Exfat-Prepare', '4Kn-Exfat-Inspect', '4Kn-Ntfs-Prepare', '4Kn-Ntfs-Inspect') }
        elseif ($FourKnDiagnose) { @('4Kn-Ntfs-Diagnose') }
        elseif ($FourKnFat) { @('4Kn-Fat12-Prepare', '4Kn-Fat12-Inspect', '4Kn-Fat32-Prepare', '4Kn-Fat32-Interrupted', '4Kn-Fat32-Inspect') }
        elseif ($FourKnFatContinue) { @('4Kn-Fat12-Continue-Prepare', '4Kn-Fat32-Range-Prepare') }
        else { @('Prepare', 'Prepare-Report', 'Interrupted', 'Inspect') }
    $destinations = $entries | ForEach-Object { Join-Path $payload.FullName "Partition-Move-$_.exe" }
    foreach ($destination in $destinations) {
        if (Test-Path -LiteralPath $destination) { throw "Preserve existing guest entry before building: $destination" }
    }
    $env:DISKSTUDIO_ACCEPTANCE_DESKTOP_SHA256 = $desktop
    $env:RUSTFLAGS = '-C target-feature=+crt-static'
    cargo build --locked --manifest-path scripts/vm/guest-checks/Cargo.toml --release --target-dir target/guest-checks
    if ($LASTEXITCODE -ne 0) { throw 'Partition move guest build failed' }
    foreach ($destination in $destinations) {
        Copy-Item -LiteralPath 'target/guest-checks/release/diskstudio-guest-checks.exe' -Destination $destination
    }
    Get-FileHash -LiteralPath $destinations -Algorithm SHA256
} finally {
    $env:RUSTFLAGS = $previousFlags
    $env:DISKSTUDIO_ACCEPTANCE_DESKTOP_SHA256 = $previousDigest
    Pop-Location
}
