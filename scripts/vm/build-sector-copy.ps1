$ErrorActionPreference = 'Stop'
$project = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..\..'))
$previousFlags = $env:RUSTFLAGS
$previousDigest = $env:DISKSTUDIO_ACCEPTANCE_DESKTOP_SHA256
Push-Location $project
try {
    $desktop = (Get-FileHash -LiteralPath 'target/release/diskstudio.exe').Hash.ToLowerInvariant()
    if ((Get-FileHash -LiteralPath '.vm/artifacts/DiskStudio.exe').Hash.ToLowerInvariant() -ne $desktop) {
        throw 'Desktop artifact mismatch'
    }
    $payload = Get-Item -LiteralPath '.vm/payload'
    if (!$payload.PSIsContainer -or ($payload.Attributes -band [IO.FileAttributes]::ReparsePoint)) {
        throw 'Invalid payload directory'
    }
    $entries = foreach ($name in @('Baseline', 'Verify', 'Verify-Report')) {
        Join-Path $payload.FullName "Sector-Copy-4Kn-$name.exe"
    }
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
