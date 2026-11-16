$ErrorActionPreference = 'Stop'
$diskStudioProject = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..\..'))
Push-Location $diskStudioProject
$diskStudioPreviousRustFlags = $env:RUSTFLAGS
try {
    # Build only. Never launch this executable on the host.
    # Apply CRT selection to C dependencies too (not just the final Rust crate).
    $env:RUSTFLAGS = '-C target-feature=+crt-static'
    cargo build --manifest-path scripts/vm/guest-checks/Cargo.toml --release --target-dir target/guest-checks
    if ($LASTEXITCODE -ne 0) { throw 'Guest acceptance driver build failed' }
    $diskStudioPayload = Join-Path $diskStudioProject '.vm\payload'
    if (!(Test-Path -LiteralPath $diskStudioPayload)) { New-Item -ItemType Directory -Path $diskStudioPayload | Out-Null }
    if ((Get-Item -LiteralPath $diskStudioPayload).Attributes -band [IO.FileAttributes]::ReparsePoint) { throw 'Payload must not be a link' }
    Copy-Item -LiteralPath 'target/guest-checks/release/diskstudio-guest-checks.exe' -Destination (Join-Path $diskStudioPayload 'DiskStudio-Guest-Checks.exe')
    Copy-Item -LiteralPath 'target/guest-checks/release/diskstudio-guest-checks.exe' -Destination (Join-Path $diskStudioPayload 'Fat-Resume-Checks.exe')
    Copy-Item -LiteralPath 'target/guest-checks/release/diskstudio-guest-checks.exe' -Destination (Join-Path $diskStudioPayload 'Ext-Resume-Checks.exe')
    Copy-Item -LiteralPath 'target/guest-checks/release/diskstudio-guest-checks.exe' -Destination (Join-Path $diskStudioPayload 'Lost-Fixture-Seed.exe')
    Copy-Item -LiteralPath 'target/guest-checks/release/diskstudio-guest-checks.exe' -Destination (Join-Path $diskStudioPayload 'Lost-Fixture-Verify.exe')
    Copy-Item -LiteralPath 'target/guest-checks/release/diskstudio-guest-checks.exe' -Destination (Join-Path $diskStudioPayload 'Lost-Fixture-Inspect.exe')
    Copy-Item -LiteralPath 'target/guest-checks/release/diskstudio-guest-checks.exe' -Destination (Join-Path $diskStudioPayload 'Device-State-Baseline.exe')
    Copy-Item -LiteralPath 'target/guest-checks/release/diskstudio-guest-checks.exe' -Destination (Join-Path $diskStudioPayload 'Device-State-Verify.exe')
    Copy-Item -LiteralPath 'target/guest-checks/release/diskstudio-guest-checks.exe' -Destination (Join-Path $diskStudioPayload 'Maintenance-Zero-Verify.exe')
    Copy-Item -LiteralPath 'target/guest-checks/release/diskstudio-guest-checks.exe' -Destination (Join-Path $diskStudioPayload 'Maintenance-Inspect.exe')
    Copy-Item -LiteralPath 'target/guest-checks/release/diskstudio-guest-checks.exe' -Destination (Join-Path $diskStudioPayload 'DiskStudio-Ext-Format.exe')
    Copy-Item -LiteralPath 'target/guest-checks/release/diskstudio-guest-checks.exe' -Destination (Join-Path $diskStudioPayload 'DiskStudio-Ext-Resize.exe')
    Copy-Item -LiteralPath 'target/guest-checks/release/diskstudio-guest-checks.exe' -Destination (Join-Path $diskStudioPayload 'DiskStudio-Format-Checks.exe')
    Copy-Item -LiteralPath 'target/guest-checks/release/diskstudio-guest-checks.exe' -Destination (Join-Path $diskStudioPayload 'DiskStudio-Format-Report.exe')
    Copy-Item -LiteralPath 'target/guest-checks/release/diskstudio-guest-checks.exe' -Destination (Join-Path $diskStudioPayload 'DiskStudio-Native-FAT.exe')
    Copy-Item -LiteralPath 'target/guest-checks/release/diskstudio-guest-checks.exe' -Destination (Join-Path $diskStudioPayload 'DiskStudio-Native-FAT-Report.exe')
    Copy-Item -LiteralPath 'target/guest-checks/release/diskstudio-guest-checks.exe' -Destination (Join-Path $diskStudioPayload 'DiskStudio-Native-FAT-Finish.exe')
    Copy-Item -LiteralPath 'target/guest-checks/release/diskstudio-guest-checks.exe' -Destination (Join-Path $diskStudioPayload 'DiskStudio-Volume-Probe.exe')
    Copy-Item -LiteralPath 'target/guest-checks/release/diskstudio-guest-checks.exe' -Destination (Join-Path $diskStudioPayload 'DiskStudio-Lock-Probe.exe')
    Copy-Item -LiteralPath 'target/guest-checks/release/diskstudio-guest-checks.exe' -Destination (Join-Path $diskStudioPayload 'DiskStudio-Metadata-Checks.exe')
    Copy-Item -LiteralPath 'target/guest-checks/release/diskstudio-guest-checks.exe' -Destination (Join-Path $diskStudioPayload 'DiskStudio-Format-Support.exe')
    Copy-Item -LiteralPath 'target/guest-checks/release/diskstudio-guest-checks.exe' -Destination (Join-Path $diskStudioPayload 'DiskStudio-Ext4-Format.exe')
    Copy-Item -LiteralPath 'target/guest-checks/release/diskstudio-guest-checks.exe' -Destination (Join-Path $diskStudioPayload 'DiskStudio-Ext4-Resize.exe')
    Copy-Item -LiteralPath 'target/guest-checks/release/diskstudio-guest-checks.exe' -Destination (Join-Path $diskStudioPayload 'DiskStudio-Ext4-Resume.exe')
    Copy-Item -LiteralPath 'target/guest-checks/release/diskstudio-guest-checks.exe' -Destination (Join-Path $diskStudioPayload 'DiskStudio-Ext4-Online.exe')
    Copy-Item -LiteralPath 'target/guest-checks/release/diskstudio-guest-checks.exe' -Destination (Join-Path $diskStudioPayload 'DiskStudio-Ext4-After-UI.exe')
    Get-FileHash -LiteralPath (Join-Path $diskStudioPayload 'DiskStudio-Guest-Checks.exe') -Algorithm SHA256
} finally { $env:RUSTFLAGS = $diskStudioPreviousRustFlags; Pop-Location }
