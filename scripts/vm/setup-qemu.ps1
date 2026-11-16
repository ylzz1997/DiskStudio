$ErrorActionPreference = 'Stop'
$diskStudioProject = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..\..'))
$diskStudioVmRoot = Join-Path $diskStudioProject '.vm'
$diskStudioDownloads = Join-Path $diskStudioVmRoot 'downloads'
$diskStudioTools = Join-Path $diskStudioVmRoot 'tools'

# Portable extraction only: no installer, elevation, PATH, services or Windows features.
foreach ($diskStudioDirectory in @($diskStudioVmRoot, $diskStudioDownloads, $diskStudioTools)) {
    if (Test-Path -LiteralPath $diskStudioDirectory) {
        if ((Get-Item -LiteralPath $diskStudioDirectory).Attributes -band [IO.FileAttributes]::ReparsePoint) { throw 'VM folders must not be links or junctions' }
    } else { New-Item -ItemType Directory -Path $diskStudioDirectory | Out-Null }
}
function Get-VerifiedVmDownload([string]$Name, [string]$Url, [string]$Algorithm, [string]$Hash) {
    $diskStudioDestination = Join-Path $diskStudioDownloads $Name
    if (!(Test-Path -LiteralPath $diskStudioDestination)) {
        curl.exe --fail --location --connect-timeout 20 --retry 3 --speed-limit 1024 --speed-time 45 --output $diskStudioDestination $Url
        if ($LASTEXITCODE -ne 0) { throw "Download failed: $Name" }
    }
    $diskStudioFile = Get-Item -LiteralPath $diskStudioDestination
    if ($diskStudioFile.Attributes -band [IO.FileAttributes]::ReparsePoint) { throw 'Downloads must not be links' }
    if ((Get-FileHash -LiteralPath $diskStudioDestination -Algorithm $Algorithm).Hash -ne $Hash) { throw "Hash mismatch: $Name. Keep it for diagnosis; do not execute it." }
    return $diskStudioDestination
}
$diskStudio7zr = Get-VerifiedVmDownload '7zr.exe' 'https://github.com/ip7z/7zip/releases/download/26.06/7zr.exe' 'SHA256' 'E80DB2DAD715A6C5ADABE292D31ED54E11F4E9B99CA349827B50573B175891BC'
$diskStudio7zArchive = Get-VerifiedVmDownload '7z2606-x64.exe' 'https://github.com/ip7z/7zip/releases/download/26.06/7z2606-x64.exe' 'SHA256' '9B697F65ECC2D9B0152F5EE96D72DDE522A7827690349D9307935DACF40F42EC'
$diskStudioQemuArchive = Get-VerifiedVmDownload 'qemu-w64-setup-20260811.exe' 'https://qemu.weilnetz.de/w64/qemu-w64-setup-20260811.exe' 'SHA512' '5BCF9EED634E8575A37B74F445AF41A2FE4106DA512D0C30C368301D4C105037FDFAB40A5287367A28A957624CDDEBBC8C07E16C88AB6634F554CDF3D16BF543'
if (!(Test-Path -LiteralPath (Join-Path $diskStudioTools '7zip\7z.exe'))) {
    & $diskStudio7zr x $diskStudio7zArchive "-o$diskStudioTools\7zip" -y
    if ($LASTEXITCODE -ne 0) { throw '7-Zip extraction failed' }
}
if (!(Test-Path -LiteralPath (Join-Path $diskStudioTools 'qemu\qemu-system-x86_64.exe'))) {
    & (Join-Path $diskStudioTools '7zip\7z.exe') x $diskStudioQemuArchive "-o$diskStudioTools\qemu" -y -bso0
    if ($LASTEXITCODE -ne 0) { throw 'QEMU extraction failed' }
}
& (Join-Path $diskStudioTools 'qemu\qemu-system-x86_64.exe') --version
if ($LASTEXITCODE -ne 0) { throw 'QEMU cannot start' }
node (Join-Path $PSScriptRoot 'lab.mjs') init
if ($LASTEXITCODE -ne 0) { throw 'VM initialization failed' }
