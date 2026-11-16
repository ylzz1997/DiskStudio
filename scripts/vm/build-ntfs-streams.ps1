param([switch]$Inspect, [switch]$ContinueExisting)
$ErrorActionPreference = 'Stop'
if ($Inspect -and $ContinueExisting) { throw 'Choose one guest entry set' }
$diskStudioRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..\..'))
$diskStudioOldFlags = $env:RUSTFLAGS
$diskStudioOldDesktop = $env:DISKSTUDIO_ACCEPTANCE_DESKTOP_SHA256
Push-Location $diskStudioRoot
try {
    # Compile only. This guest driver MUST NOT be executed on the host.
    $diskStudioDigest = (Get-FileHash -LiteralPath 'target/release/diskstudio.exe' -Algorithm SHA256).Hash.ToLowerInvariant()
    foreach ($diskStudioCopy in @('.vm/artifacts/DiskStudio.exe', '.vm/payload/DiskStudio.exe')) {
        if ((Get-FileHash -LiteralPath $diskStudioCopy -Algorithm SHA256).Hash.ToLowerInvariant() -ne $diskStudioDigest) { throw 'Desktop artifact mismatch' }
    }
    $env:DISKSTUDIO_ACCEPTANCE_DESKTOP_SHA256 = $diskStudioDigest
    $env:RUSTFLAGS = '-C target-feature=+crt-static'
    cargo build --manifest-path scripts/vm/guest-checks/Cargo.toml --release --target-dir target/guest-checks
    if ($LASTEXITCODE -ne 0) { throw 'Named-stream guest build failed' }
    if ((Get-Item -LiteralPath '.vm/payload').Attributes -band [IO.FileAttributes]::ReparsePoint) { throw 'Payload is a link' }
    $diskStudioEntries = if ($Inspect) { @('Ntfs-Streams-Inspect.exe', 'Ntfs-Streams-Inspect-Report.exe') } elseif ($ContinueExisting) { @('Ntfs-Streams-Continue.exe', 'Ntfs-Streams-Continue-Report.exe') } else { @('Ntfs-Streams.exe', 'Ntfs-Streams-Report.exe') }
    foreach ($diskStudioName in $diskStudioEntries) {
        Copy-Item -LiteralPath 'target/guest-checks/release/diskstudio-guest-checks.exe' -Destination (Join-Path '.vm/payload' $diskStudioName)
    }
    Get-FileHash -LiteralPath (Join-Path '.vm/payload' $diskStudioEntries[0]) -Algorithm SHA256
} finally {
    $env:RUSTFLAGS = $diskStudioOldFlags
    $env:DISKSTUDIO_ACCEPTANCE_DESKTOP_SHA256 = $diskStudioOldDesktop
    Pop-Location
}
