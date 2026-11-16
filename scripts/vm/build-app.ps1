$ErrorActionPreference = 'Stop'
$diskStudioProject = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..\..'))
$diskStudioArtifacts = Join-Path $diskStudioProject '.vm\artifacts'
$diskStudioRelease = Join-Path $diskStudioProject 'target\release\diskstudio.exe'
foreach ($diskStudioFolder in @((Join-Path $diskStudioProject '.vm'), $diskStudioArtifacts)) {
    if (!(Test-Path -LiteralPath $diskStudioFolder)) { New-Item -ItemType Directory -Path $diskStudioFolder | Out-Null }
    if ((Get-Item -LiteralPath $diskStudioFolder).Attributes -band [IO.FileAttributes]::ReparsePoint) { throw 'VM artifact folders must not be links' }
}
Push-Location $diskStudioProject
try {
    # Build the production application ONCE. The VM receives these exact bytes.
    # No binary is executed and no elevation is requested here.
    npm exec tauri build -- --no-bundle
    if ($LASTEXITCODE -ne 0) { throw 'Desktop build failed' }
    $diskStudioArtifact = Join-Path $diskStudioArtifacts 'DiskStudio.exe'
    Copy-Item -LiteralPath $diskStudioRelease -Destination $diskStudioArtifact
    Get-FileHash -LiteralPath $diskStudioRelease,$diskStudioArtifact -Algorithm SHA256
} finally { Pop-Location }
