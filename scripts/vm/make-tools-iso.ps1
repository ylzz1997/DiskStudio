param([switch]$Live)
$ErrorActionPreference = 'Stop'
$diskStudioProject = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..\..'))
$diskStudioVmRoot = Join-Path $diskStudioProject '.vm'
$diskStudioStage = Join-Path $diskStudioVmRoot 'payload'
$diskStudioIso = Join-Path $diskStudioVmRoot 'media\diskstudio-tools.iso'
if ($Live) { $diskStudioIso = Join-Path $diskStudioVmRoot ('media\diskstudio-tools-' + [guid]::NewGuid().ToString() + '.iso') }
$diskStudioExe = Join-Path $diskStudioProject 'target\release\diskstudio.exe'
if (!(Test-Path -LiteralPath $diskStudioExe)) { throw 'Build the release desktop app first' }
if (!$Live -and (Test-Path -LiteralPath (Join-Path $diskStudioVmRoot 'run.json'))) {
    $diskStudioRun = Get-Content -LiteralPath (Join-Path $diskStudioVmRoot 'run.json') -Raw | ConvertFrom-Json
    if (Get-Process -Id $diskStudioRun.pid -ErrorAction SilentlyContinue) { throw 'Shut down the VM before replacing its optical media' }
}
foreach ($diskStudioDirectory in @($diskStudioVmRoot, $diskStudioStage, (Join-Path $diskStudioVmRoot 'media'))) {
    if (Test-Path -LiteralPath $diskStudioDirectory) {
        if ((Get-Item -LiteralPath $diskStudioDirectory).Attributes -band [IO.FileAttributes]::ReparsePoint) { throw 'VM folders must not be links or junctions' }
    } else { New-Item -ItemType Directory -Path $diskStudioDirectory | Out-Null }
}
if (Test-Path -LiteralPath $diskStudioIso) { throw 'Tools ISO already exists; preserve it or remove that single file after shutting down the VM' }
foreach ($diskStudioExisting in (Get-ChildItem -LiteralPath $diskStudioStage -Recurse -Force)) {
    if ($diskStudioExisting.Attributes -band [IO.FileAttributes]::ReparsePoint) { throw 'Payload must not contain links or junctions' }
}
Copy-Item -LiteralPath $diskStudioExe -Destination (Join-Path $diskStudioStage 'DiskStudio.exe')
@'
DiskStudio VM tools

Run this copy of DiskStudio.exe inside the test VM.
If WebView2 is missing, install MicrosoftEdgeWebView2RuntimeInstallerX64.exe
from this CD inside the guest first. The host does not need this installer.

This is the same production desktop application used on the host, with the
same SHA-256. It supports GPT initialization, partition creation, NTFS quick
format, NTFS resize and deletion. There is no separate VM application build.
Every mutation requires preview/confirmation and fresh backend validation.
Do not run partition tests on any other machine or on the system disk.

System disk: DS-OS-0001, 80 GiB. NEVER use it for partition-operation tests.
Disposable data disks: DS-TEST-A-0001 and DS-TEST-B-0001, 16 GiB each.
There are no passed-through host disks and no shared host folders.
This CD is read only. Copy the app to the guest desktop if desired.
'@ | Set-Content -LiteralPath (Join-Path $diskStudioStage 'README.txt') -Encoding utf8
@{ Application = 'DiskStudio'; Build = 'production'; Sha256 = (Get-FileHash -LiteralPath $diskStudioExe -Algorithm SHA256).Hash; PreparedAt = (Get-Date).ToString('o') } | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $diskStudioStage 'build-info.json') -Encoding utf8

# Build an ordinary ISO file with IMAPI's filesystem-image API. No recorder,
# host volume, Mount-DiskImage, burning or physical media API is used.
Add-Type -TypeDefinition @'
using System;
using System.IO;
using System.Runtime.InteropServices;
using System.Runtime.InteropServices.ComTypes;
public static class DiskStudioIsoWriter {
  public static void Save(object imageStream, string filename) {
    var stream = (IStream)imageStream;
    var read = Marshal.AllocHGlobal(4);
    try {
      using (var output = new FileStream(filename, FileMode.CreateNew, FileAccess.Write)) {
        var buffer = new byte[1024 * 1024];
        for (;;) {
          stream.Read(buffer, buffer.Length, read);
          int count = Marshal.ReadInt32(read);
          if (count == 0) break;
          output.Write(buffer, 0, count);
        }
      }
    } finally { Marshal.FreeHGlobal(read); }
  }
}
'@
$diskStudioImage = New-Object -ComObject IMAPI2FS.MsftFileSystemImage
try {
    $diskStudioImage.FileSystemsToCreate = 3 # ISO9660 + Joliet
    $diskStudioImage.VolumeName = 'DISKSTUDIO'
    $diskStudioImage.FreeMediaBlocks = 1000000
    $diskStudioImage.Root.AddTree($diskStudioStage, $false)
    $diskStudioResult = $diskStudioImage.CreateResultImage()
    [DiskStudioIsoWriter]::Save($diskStudioResult.ImageStream, $diskStudioIso)
} finally { [Runtime.InteropServices.Marshal]::ReleaseComObject($diskStudioImage) | Out-Null }
Get-FileHash -LiteralPath $diskStudioIso -Algorithm SHA256
if ($Live) {
    @{ file = [IO.Path]::GetFileName($diskStudioIso); sha256 = (Get-FileHash -LiteralPath $diskStudioIso -Algorithm SHA256).Hash.ToLowerInvariant() } |
        ConvertTo-Json | Set-Content -LiteralPath (Join-Path $diskStudioVmRoot 'pending-tools.json') -Encoding utf8
    Write-Output 'New immutable tools ISO ready. Close the guest app, then run: node scripts/vm/lab.mjs refresh-tools'
}
