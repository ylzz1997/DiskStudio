param([switch]$Fault, [switch]$Device, [switch]$ContinueDevice, [switch]$Reread, [switch]$RereadProbe, [switch]$Continuation, [switch]$Legacy, [switch]$Image)
$ErrorActionPreference = 'Stop'
$project = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..\..'))
$previousFlags = $env:RUSTFLAGS
$previousDigest = $env:DISKSTUDIO_ACCEPTANCE_DESKTOP_SHA256
Push-Location $project
try {
    $desktop = (Get-FileHash -LiteralPath 'target/release/diskstudio.exe').Hash.ToLowerInvariant()
    if (@($Fault, $Device, $ContinueDevice, $Reread, $RereadProbe, $Continuation, $Legacy, $Image).Where({ $_ }).Count -gt 1) { throw 'Choose one sector-copy acceptance profile' }
    if ((Get-FileHash -LiteralPath '.vm/artifacts/DiskStudio.exe').Hash.ToLowerInvariant() -ne $desktop) {
        throw 'Desktop artifact mismatch'
    }
    $payload = Get-Item -LiteralPath '.vm/payload'
    if (!$payload.PSIsContainer -or ($payload.Attributes -band [IO.FileAttributes]::ReparsePoint)) {
        throw 'Invalid payload directory'
    }
    $names = if ($RereadProbe) { @('Sector-Copy-Reread-4Kn-Probe.exe') }
        elseif ($Image) { @('Sector-Image-4Kn-Prepare.exe','Sector-Image-4Kn-File.exe','Sector-Image-4Kn-Device.exe') }
        elseif ($Legacy) { @('Sector-Legacy-4Kn-Prepare.exe','Sector-Legacy-4Kn-File.exe','Sector-Legacy-4Kn-Rearm.exe','Sector-Legacy-4Kn-Device.exe') }
        elseif ($Continuation) { @('Sector-Continuation-4Kn-Prepare.exe','Sector-Continuation-4Kn-Interrupted.exe','Sector-Continuation-4Kn-Reread.exe','Sector-Continuation-4Kn-Finish.exe') }
        elseif ($Reread) { @('Sector-Copy-Reread-4Kn-Prepare.exe','Sector-Copy-Reread-4Kn-Verify-File.exe','Sector-Copy-Reread-4Kn-Verify-Device.exe') }
        elseif ($ContinueDevice) { @('Sector-Copy-Device-4Kn-Continue.exe') }
        elseif ($Device) { @('Sector-Copy-Device-4Kn-Prepare.exe','Sector-Copy-Device-4Kn-Verify.exe','Sector-Copy-Device-4Kn-Continue.exe') }
        elseif ($Fault) { @('Sector-Copy-Fault-4Kn-Prepare.exe','Sector-Copy-Fault-4Kn-Verify.exe') }
        else { @('Sector-Copy-4Kn-Baseline.exe','Sector-Copy-4Kn-Verify.exe','Sector-Copy-4Kn-Verify-Report.exe') }
    $entries = foreach ($name in $names) { Join-Path $payload.FullName $name }
    foreach ($entry in $entries) {
        if (Test-Path -LiteralPath $entry) { throw "Preserve existing guest entry: $entry" }
    }
    # Compile only. The executable runs exclusively inside DiskStudio-Test.
    $env:DISKSTUDIO_ACCEPTANCE_DESKTOP_SHA256 = $desktop
    $env:RUSTFLAGS = '-C target-feature=+crt-static'
    cargo build --locked --manifest-path scripts/vm/guest-checks/Cargo.toml --release --target-dir target/guest-checks
    if ($LASTEXITCODE -ne 0) { throw 'Sector copy guest build failed' }
    foreach ($entry in $entries) {
        Copy-Item -LiteralPath 'target/guest-checks/release/diskstudio-guest-checks.exe' -Destination $entry
    }
    Get-FileHash -LiteralPath $entries
} finally {
    $env:RUSTFLAGS = $previousFlags
    $env:DISKSTUDIO_ACCEPTANCE_DESKTOP_SHA256 = $previousDigest
    Pop-Location
}
