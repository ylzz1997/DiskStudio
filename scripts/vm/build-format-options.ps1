param([switch]$Continue)
$ErrorActionPreference = 'Stop'
$diskStudioRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..\..'))
Push-Location $diskStudioRoot
$diskStudioOldFlags = $env:RUSTFLAGS
$diskStudioOldDesktop = $env:DISKSTUDIO_ACCEPTANCE_DESKTOP_SHA256
try {
    # Build only; NEVER execute the guest driver on the host.
    $diskStudioRelease = Join-Path $diskStudioRoot 'target\release\diskstudio.exe'
    $diskStudioArtifact = Join-Path $diskStudioRoot '.vm\artifacts\DiskStudio.exe'
    $diskStudioDigest = (Get-FileHash -LiteralPath $diskStudioRelease -Algorithm SHA256).Hash.ToLowerInvariant()
    if ((Get-FileHash -LiteralPath $diskStudioArtifact -Algorithm SHA256).Hash.ToLowerInvariant() -ne $diskStudioDigest) { throw 'Release/artifact mismatch' }
    $env:DISKSTUDIO_ACCEPTANCE_DESKTOP_SHA256 = $diskStudioDigest
    $env:RUSTFLAGS = '-C target-feature=+crt-static'
    cargo build --manifest-path scripts/vm/guest-checks/Cargo.toml --release --target-dir target/guest-checks
    if ($LASTEXITCODE -ne 0) { throw 'Format-options guest driver build failed' }
    $diskStudioStage = Join-Path $diskStudioRoot '.vm\payload'
    foreach ($diskStudioFolder in @((Join-Path $diskStudioRoot '.vm'), $diskStudioStage)) {
        if (!(Test-Path -LiteralPath $diskStudioFolder)) { New-Item -ItemType Directory -Path $diskStudioFolder | Out-Null }
        if ((Get-Item -LiteralPath $diskStudioFolder).Attributes -band [IO.FileAttributes]::ReparsePoint) { throw 'Payload folder must not be a link' }
    }
    $diskStudioNames = if ($Continue) { @('Format-Options-Continue.exe','Format-Options-Continue-Report.exe') } else { @('Format-Options.exe','Format-Options-Report.exe') }
    foreach ($diskStudioName in $diskStudioNames) {
        Copy-Item -LiteralPath 'target/guest-checks/release/diskstudio-guest-checks.exe' -Destination (Join-Path $diskStudioStage $diskStudioName)
    }
    Get-FileHash -LiteralPath (Join-Path $diskStudioStage $diskStudioNames[0]) -Algorithm SHA256
} finally {
    $env:RUSTFLAGS = $diskStudioOldFlags
    $env:DISKSTUDIO_ACCEPTANCE_DESKTOP_SHA256 = $diskStudioOldDesktop
    Pop-Location
}
