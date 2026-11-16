$ErrorActionPreference = 'Stop'
$diskStudioRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..\..'))
$diskStudioOldFlags = $env:RUSTFLAGS
$diskStudioOldDesktop = $env:DISKSTUDIO_ACCEPTANCE_DESKTOP_SHA256
Push-Location $diskStudioRoot
try {
    # Compile only; never execute the guest driver on the host.
    $diskStudioDigest = (Get-FileHash -LiteralPath 'target/release/diskstudio.exe' -Algorithm SHA256).Hash.ToLowerInvariant()
    foreach ($diskStudioCopy in @('.vm/artifacts/DiskStudio.exe', '.vm/payload/DiskStudio.exe')) {
        if ((Get-FileHash -LiteralPath $diskStudioCopy -Algorithm SHA256).Hash.ToLowerInvariant() -ne $diskStudioDigest) { throw 'Desktop artifact mismatch' }
    }
    $env:DISKSTUDIO_ACCEPTANCE_DESKTOP_SHA256 = $diskStudioDigest
    $env:RUSTFLAGS = '-C target-feature=+crt-static'
    cargo build --manifest-path scripts/vm/guest-checks/Cargo.toml --release --target-dir target/guest-checks
    if ($LASTEXITCODE -ne 0) { throw 'Letter/outcome guest build failed' }
    if ((Get-Item -LiteralPath '.vm/payload').Attributes -band [IO.FileAttributes]::ReparsePoint) { throw 'Payload is a link' }
    foreach ($diskStudioName in @('Letter-Outcome.exe', 'Letter-Outcome-Report.exe', 'Letter-Outcome-UI-Inspect.exe', 'Letter-Outcome-UI-Report.exe')) {
        Copy-Item -LiteralPath 'target/guest-checks/release/diskstudio-guest-checks.exe' -Destination (Join-Path '.vm/payload' $diskStudioName)
    }
    Get-FileHash -LiteralPath '.vm/payload/Letter-Outcome.exe' -Algorithm SHA256
} finally {
    $env:RUSTFLAGS = $diskStudioOldFlags
    $env:DISKSTUDIO_ACCEPTANCE_DESKTOP_SHA256 = $diskStudioOldDesktop
    Pop-Location
}
