$ErrorActionPreference = 'Stop'
$diskStudioRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..\..'))
$diskStudioPreviousRustFlags = $env:RUSTFLAGS
Push-Location $diskStudioRoot
try {
    # Build only. NEVER start the fixture on the host.
    $env:RUSTFLAGS = '-C target-feature=+crt-static'
    cargo build --release --target x86_64-pc-windows-msvc --manifest-path scripts/vm/gpt-fault/Cargo.toml --target-dir .vm/gpt-fault-build
    if ($LASTEXITCODE -ne 0) { throw 'GPT fault fixture build failed' }
    Copy-Item -LiteralPath .vm/gpt-fault-build/x86_64-pc-windows-msvc/release/diskstudio-gpt-fault.exe -Destination .vm/payload/GPT-Fault-B.exe
    Get-FileHash -LiteralPath .vm/payload/GPT-Fault-B.exe -Algorithm SHA256
} finally { $env:RUSTFLAGS = $diskStudioPreviousRustFlags; Pop-Location }
