[CmdletBinding(DefaultParameterSetName = 'Original')]
param(
    [Parameter(ParameterSetName = 'Inspect', Mandatory)] [switch]$InspectMergeLockFailure,
    [Parameter(ParameterSetName = 'FreshLeft', Mandatory)] [switch]$FreshLeftMerge,
    [Parameter(ParameterSetName = 'ResumeLeft', Mandatory)] [switch]$ResumeFreshLeft,
    [Parameter(ParameterSetName = 'InspectCleanStop', Mandatory)] [switch]$InspectCleanStop,
    [Parameter(ParameterSetName = 'RevisedLeft', Mandatory)] [switch]$RevisedLeftMerge,
    [Parameter(ParameterSetName = 'ProbeLeftLock', Mandatory)] [switch]$ProbeRevisedLeftLock,
    [Parameter(ParameterSetName = 'ProbeCopyLock', Mandatory)] [switch]$ProbeLockAfterCopy,
    [Parameter(ParameterSetName = 'Grace', Mandatory)] [switch]$LockGrace
)

$ErrorActionPreference = 'Stop'
$dsProject = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..\..'))
$dsPreviousFlags = $env:RUSTFLAGS
$dsPreviousDigest = $env:DISKSTUDIO_ACCEPTANCE_DESKTOP_SHA256
Push-Location $dsProject
try {
    $dsDesktop = (Get-FileHash -LiteralPath 'target/release/diskstudio.exe' -Algorithm SHA256).Hash.ToLowerInvariant()
    if ((Get-FileHash -LiteralPath '.vm/artifacts/DiskStudio.exe' -Algorithm SHA256).Hash.ToLowerInvariant() -ne $dsDesktop) {
        throw 'Desktop artifact mismatch'
    }
    $dsPayload = Get-Item -LiteralPath '.vm/payload'
    if (!$dsPayload.PSIsContainer -or ($dsPayload.Attributes -band [IO.FileAttributes]::ReparsePoint)) {
        throw 'Invalid payload directory'
    }
    $dsNames = if ($InspectMergeLockFailure) { @('Merge-Lock-Failure') }
        elseif ($FreshLeftMerge) { @('Left-Prepare', 'Left-Hold', 'Left-Busy', 'Left-Success') }
        elseif ($ResumeFreshLeft) { @('Left-Interrupted', 'Left-Resume') }
        elseif ($InspectCleanStop) { @('Left-Clean-Stopped') }
        elseif ($RevisedLeftMerge) { @('Left-Revised-Prepare', 'Left-Revised-Hold', 'Left-Revised-Busy', 'Left-Revised-Success', 'Left-Revised-Clean-Stopped') }
        elseif ($ProbeRevisedLeftLock) { @('Left-Revised-Lock-Probe') }
        elseif ($ProbeLockAfterCopy) { @('Left-Revised-Copy-Lock-Probe') }
        elseif ($LockGrace) { @('Left-Grace-Prepare', 'Left-Grace-Hold', 'Left-Grace-Busy', 'Left-Grace-Success', 'Left-Grace-Clean-Stopped') }
        else { @('Prepare', 'Split', 'Adjacent', 'Across', 'Transfer', 'Merge-Left') }
    $dsFiles = foreach ($dsName in $dsNames) {
        Join-Path $dsPayload.FullName "Partition-Reallocation-4Kn-$dsName.exe"
        Join-Path $dsPayload.FullName "Partition-Reallocation-4Kn-$dsName-Report.exe"
    }
    foreach ($dsDestination in $dsFiles) {
        if (Test-Path -LiteralPath $dsDestination) { throw "Preserve existing guest entry: $dsDestination" }
    }
    $env:DISKSTUDIO_ACCEPTANCE_DESKTOP_SHA256 = $dsDesktop
    $env:RUSTFLAGS = '-C target-feature=+crt-static'
    cargo build --locked --manifest-path scripts/vm/guest-checks/Cargo.toml --release --target-dir target/guest-checks
    if ($LASTEXITCODE -ne 0) { throw 'Partition reallocation guest build failed' }
    foreach ($dsDestination in $dsFiles) {
        Copy-Item -LiteralPath 'target/guest-checks/release/diskstudio-guest-checks.exe' -Destination $dsDestination
    }
    Get-FileHash -LiteralPath $dsFiles -Algorithm SHA256
} finally {
    $env:RUSTFLAGS = $dsPreviousFlags
    $env:DISKSTUDIO_ACCEPTANCE_DESKTOP_SHA256 = $dsPreviousDigest
    Pop-Location
}
