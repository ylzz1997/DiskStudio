[CmdletBinding(DefaultParameterSetName = 'First')]
param(
    [Parameter(ParameterSetName = 'Diagnose', Mandatory)] [switch]$Diagnose,
    [Parameter(ParameterSetName = 'Continue', Mandatory)] [switch]$Continue,
    [Parameter(ParameterSetName = 'CatalogueProbe', Mandatory)] [switch]$CatalogueProbe,
    [Parameter(ParameterSetName = 'Corrected', Mandatory)] [switch]$Corrected,
    [Parameter(ParameterSetName = 'OwnerInspect', Mandatory)] [switch]$OwnerInspect,
    [Parameter(ParameterSetName = 'Fresh', Mandatory)] [switch]$Fresh
)

$ErrorActionPreference = 'Stop'
$diskStudioProject = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..\..'))
$previousFlags = $env:RUSTFLAGS
$previousDigest = $env:DISKSTUDIO_ACCEPTANCE_DESKTOP_SHA256
Push-Location $diskStudioProject
try {
    $desktop = (Get-FileHash -LiteralPath 'target/release/diskstudio.exe' -Algorithm SHA256).Hash.ToLowerInvariant()
    if ((Get-FileHash -LiteralPath '.vm/artifacts/DiskStudio.exe' -Algorithm SHA256).Hash.ToLowerInvariant() -ne $desktop) {
        throw 'Desktop artifact mismatch'
    }
    $payload = Get-Item -LiteralPath '.vm/payload'
    if ($payload.Attributes -band [IO.FileAttributes]::ReparsePoint) { throw 'Payload is a link' }
    $entries = if ($Diagnose) { @('Diagnose') }
        elseif ($Continue) { @('Continue', 'Continue-Report', 'Continue-Inspect') }
        elseif ($CatalogueProbe) { @('Catalogue-Recheck', 'Catalogue-Recheck-Report') }
        elseif ($Corrected) { @('Corrected', 'Corrected-Report', 'Corrected-Inspect') }
        elseif ($OwnerInspect) { @('Owner-Inspect') }
        elseif ($Fresh) { @('Fresh-Prepare', 'Fresh-Check', 'Fresh-Report', 'Fresh-Inspect') }
        else { @('Prepare', 'Check', 'Inspect', 'Report') }
    $destinations = $entries | ForEach-Object {
        Join-Path $payload.FullName "Partition-Merge-4Kn-$_.exe"
    }
    foreach ($destination in $destinations) {
        if (Test-Path -LiteralPath $destination) { throw "Preserve existing guest entry: $destination" }
    }
    $env:DISKSTUDIO_ACCEPTANCE_DESKTOP_SHA256 = $desktop
    $env:RUSTFLAGS = '-C target-feature=+crt-static'
    cargo build --locked --manifest-path scripts/vm/guest-checks/Cargo.toml --release --target-dir target/guest-checks
    if ($LASTEXITCODE -ne 0) { throw 'Partition merge guest build failed' }
    foreach ($destination in $destinations) {
        Copy-Item -LiteralPath 'target/guest-checks/release/diskstudio-guest-checks.exe' -Destination $destination
    }
    Get-FileHash -LiteralPath $destinations -Algorithm SHA256
} finally {
    $env:RUSTFLAGS = $previousFlags
    $env:DISKSTUDIO_ACCEPTANCE_DESKTOP_SHA256 = $previousDigest
    Pop-Location
}
