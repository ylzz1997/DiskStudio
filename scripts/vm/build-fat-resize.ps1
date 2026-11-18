param([switch]$Relocation, [switch]$Remaining, [switch]$Capacity, [switch]$Fat12Tail, [switch]$Sectors, [switch]$SectorDiagnosis, [switch]$SectorTail, [switch]$SectorFat32)

$ErrorActionPreference = 'Stop'
$diskStudioRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..\..'))
$diskStudioOldFlags = $env:RUSTFLAGS
$diskStudioOldDesktop = $env:DISKSTUDIO_ACCEPTANCE_DESKTOP_SHA256
Push-Location $diskStudioRoot
try {
    $diskStudioDigest = (Get-FileHash -LiteralPath 'target/release/diskstudio.exe' -Algorithm SHA256).Hash.ToLowerInvariant()
    if ((Get-FileHash -LiteralPath '.vm/artifacts/DiskStudio.exe' -Algorithm SHA256).Hash.ToLowerInvariant() -ne $diskStudioDigest) {
        throw 'Desktop artifact mismatch'
    }
    $diskStudioPayload = Get-Item -LiteralPath '.vm/payload'
    if ($diskStudioPayload.Attributes -band [IO.FileAttributes]::ReparsePoint) {
        throw 'Payload is a link'
    }
    $env:DISKSTUDIO_ACCEPTANCE_DESKTOP_SHA256 = $diskStudioDigest
    $env:RUSTFLAGS = '-C target-feature=+crt-static'
    cargo build --locked --manifest-path scripts/vm/guest-checks/Cargo.toml --release --target-dir target/guest-checks
    if ($LASTEXITCODE -ne 0) { throw 'FAT resize guest build failed' }
    $diskStudioPrefix = if ($Fat12Tail) { 'Fat-Capacity-Fat12-Tail' } elseif ($Capacity -and $Remaining) { 'Fat-Capacity-Remaining' } elseif ($Capacity) { 'Fat-Capacity' } elseif ($Remaining) { 'Fat-Relocation-Remaining' } elseif ($Relocation) { 'Fat-Relocation' } else { 'Fat-Resize' }
    $diskStudioPrefixes = if ($Sectors -and $Remaining) { @('Fat-Sectors-4Kn-Continue') } elseif ($Sectors) { @('Fat-Sectors-512', 'Fat-Sectors-4Kn') } else { @($diskStudioPrefix) }
    $diskStudioEntries = @(if ($SectorFat32) {
        'Fat-Sectors-4Kn-Fat32-Check.exe'
        'Fat-Sectors-4Kn-Fat32-Report.exe'
    } elseif ($SectorTail) {
        'Fat-Sectors-4Kn-Tail-Check.exe'
        'Fat-Sectors-4Kn-Tail-Report.exe'
        'Fat-Sectors-4Kn-Fat32-Diagnose.exe'
    } elseif ($SectorDiagnosis) {
        'Fat-Sectors-4Kn-Diagnose.exe'
    } else {
        foreach ($diskStudioName in $diskStudioPrefixes) {
            "$diskStudioName-Check.exe"
            "$diskStudioName-Report.exe"
        }
    })
    foreach ($diskStudioEntry in $diskStudioEntries) {
        $diskStudioDestination = Join-Path '.vm/payload' $diskStudioEntry
        if (Test-Path -LiteralPath $diskStudioDestination) { throw "Preserve existing guest entry $diskStudioEntry before staging another build" }
        Copy-Item -LiteralPath 'target/guest-checks/release/diskstudio-guest-checks.exe' -Destination $diskStudioDestination
    }
    Get-FileHash -LiteralPath (Join-Path '.vm/payload' $diskStudioEntries[0]) -Algorithm SHA256
} finally {
    $env:RUSTFLAGS = $diskStudioOldFlags
    $env:DISKSTUDIO_ACCEPTANCE_DESKTOP_SHA256 = $diskStudioOldDesktop
    Pop-Location
}
