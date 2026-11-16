$ErrorActionPreference = 'Stop'
$diskStudioProject = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '../..'))
$diskStudioArchive = Join-Path $diskStudioProject '.vm/downloads/e2fsprogs-1.47.4.tar.xz'
$diskStudioToolArchive = Join-Path $diskStudioProject '.vm/downloads/w64devkit.7z.exe'
if ((Get-FileHash -LiteralPath $diskStudioArchive -Algorithm SHA256).Hash -ne 'fd5bf388cbdbe006a3d3b318d983b2948382440acc85a87f1e7d108653e8db0b') { throw 'e2fsprogs archive hash mismatch' }
if ((Get-FileHash -LiteralPath $diskStudioToolArchive -Algorithm SHA256).Hash -ne '54c2bb9962e8a30c8270e34c5efc3d5fee579f53087fa7d435b95fbba2af7b2b') { throw 'Compiler archive hash mismatch' }
$diskStudioBuild = Join-Path $diskStudioProject ('.vm/ext4-source/build-' + [guid]::NewGuid())
$diskStudioOutput = Join-Path $diskStudioProject 'src-tauri/ext4-engine'
New-Item -ItemType Directory -Path $diskStudioBuild | Out-Null
# Skip the upstream RELEASE-NOTES symlink; documentation lives in doc/RelNotes/.
tar.exe -xf $diskStudioArchive -C $diskStudioBuild --exclude 'e2fsprogs-1.47.4/RELEASE-NOTES'
if ($LASTEXITCODE -ne 0) { throw 'Source extraction failed' }
$diskStudioSource = Join-Path $diskStudioBuild 'e2fsprogs-1.47.4'
node (Join-Path $PSScriptRoot 'patch.mjs') $diskStudioSource
if ($LASTEXITCODE -ne 0) { throw 'Source patch failed' }
& 'C:/Program Files/Git/bin/bash.exe' --noprofile --norc (Join-Path $PSScriptRoot 'build.sh') $diskStudioSource
if ($LASTEXITCODE -ne 0) { throw "Engine compilation failed; logs: $diskStudioSource" }
New-Item -ItemType Directory -Path $diskStudioOutput -Force | Out-Null
foreach ($diskStudioBinary in @('misc/mke2fs.exe', 'e2fsck/e2fsck.exe', 'resize/resize2fs.exe')) {
    Copy-Item -LiteralPath (Join-Path $diskStudioSource $diskStudioBinary) -Destination $diskStudioOutput
}
Copy-Item -LiteralPath (Join-Path $PSScriptRoot 'mke2fs.conf') -Destination $diskStudioOutput
Get-FileHash -LiteralPath (Join-Path $diskStudioOutput 'mke2fs.exe'),(Join-Path $diskStudioOutput 'e2fsck.exe'),(Join-Path $diskStudioOutput 'resize2fs.exe') -Algorithm SHA256
