param([switch]$Recheck)
$ErrorActionPreference = 'Stop'
$diskStudioRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..\..'))
$diskStudioOldFlags = $env:RUSTFLAGS
$diskStudioOldDesktop = $env:DISKSTUDIO_ACCEPTANCE_DESKTOP_SHA256
Push-Location $diskStudioRoot
try {
    # Compile only. All helper disk work is read-only inside the guarded guest.
    $diskStudioDigest = (Get-FileHash -LiteralPath 'target/release/diskstudio.exe' -Algorithm SHA256).Hash.ToLowerInvariant()
    foreach ($diskStudioCopy in @('.vm/artifacts/DiskStudio.exe', '.vm/payload/DiskStudio.exe')) {
        if ((Get-FileHash -LiteralPath $diskStudioCopy -Algorithm SHA256).Hash.ToLowerInvariant() -ne $diskStudioDigest) { throw 'Desktop artifact mismatch' }
    }
    $env:DISKSTUDIO_ACCEPTANCE_DESKTOP_SHA256 = $diskStudioDigest
    $env:RUSTFLAGS = '-C target-feature=+crt-static'
    cargo build --manifest-path scripts/vm/guest-checks/Cargo.toml --release --target-dir target/guest-checks
    if ($LASTEXITCODE -ne 0) { throw 'Quick progress guest build failed' }
    if ((Get-Item -LiteralPath '.vm/payload').Attributes -band [IO.FileAttributes]::ReparsePoint) { throw 'Payload is a link' }
    $diskStudioNames = if ($Recheck) {
        @('Quick-Progress-Recheck.exe', 'Quick-Progress-Recheck-Report.exe')
    } else {
        @('Quick-Progress-Prepare.exe', 'Quick-Progress-Prepare-Report.exe', 'Quick-Progress-Inspect.exe', 'Quick-Progress-Inspect-Report.exe')
    }
    foreach ($diskStudioName in $diskStudioNames) {
        Copy-Item -LiteralPath 'target/guest-checks/release/diskstudio-guest-checks.exe' -Destination (Join-Path '.vm/payload' $diskStudioName)
    }
    Get-FileHash -LiteralPath (Join-Path '.vm/payload' $diskStudioNames[0]) -Algorithm SHA256
} finally {
    $env:RUSTFLAGS = $diskStudioOldFlags
    $env:DISKSTUDIO_ACCEPTANCE_DESKTOP_SHA256 = $diskStudioOldDesktop
    Pop-Location
}
