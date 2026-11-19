$ErrorActionPreference = 'Stop'
$diskStudioProject = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '../..'))
$diskStudioCommit = '3fa5cf21dde2913d58d7b74e150625e18bd490bd'
$diskStudioArchive = Join-Path $diskStudioProject "third-party/winbtrfs/winbtrfs-$diskStudioCommit.tar.gz"
$diskStudioToolArchive = Join-Path $diskStudioProject '.vm/downloads/w64devkit.7z.exe'
if ((Get-FileHash -LiteralPath $diskStudioArchive -Algorithm SHA256).Hash -ne 'c21258d7e755fbdc23f02c3febb0adce0b576f394d2f8ebbc89c7bfe71a3febc') { throw 'WinBtrfs archive hash mismatch' }
if ((Get-FileHash -LiteralPath $diskStudioToolArchive -Algorithm SHA256).Hash -ne '54c2bb9962e8a30c8270e34c5efc3d5fee579f53087fa7d435b95fbba2af7b2b') { throw 'Compiler archive hash mismatch' }
$diskStudioBuild = Join-Path $diskStudioProject ('.vm/btrfs-source/build-' + [guid]::NewGuid())
New-Item -ItemType Directory -Path $diskStudioBuild | Out-Null
tar.exe -xf $diskStudioArchive -C $diskStudioBuild
if ($LASTEXITCODE -ne 0) { throw 'WinBtrfs source extraction failed' }
$diskStudioSource = Join-Path $diskStudioBuild "btrfs-$diskStudioCommit/src"
node (Join-Path $PSScriptRoot 'patch.mjs') (Split-Path -Parent $diskStudioSource)
if ($LASTEXITCODE -ne 0) { throw 'WinBtrfs source patch failed' }
$diskStudioCompiler = Join-Path $diskStudioProject '.vm/tools/w64devkit/bin/gcc.exe'
$diskStudioBinary = Join-Path $diskStudioBuild 'diskstudio-mkbtrfs.exe'
$diskStudioFiles = @('ubtrfs/diskstudio-mkbtrfs.c', 'crc32c.c', 'crc32c-gas.S') | ForEach-Object { Join-Path $diskStudioSource $_ }
& $diskStudioCompiler -std=gnu11 -O2 -D_AMD64_ -D__USE_MINGW_ANSI_STDIO=0 @diskStudioFiles -o $diskStudioBinary -lbcrypt -lntdll
if ($LASTEXITCODE -ne 0) { throw 'WinBtrfs engine compilation failed' }
$diskStudioOutput = Join-Path $diskStudioProject 'src-tauri/btrfs-engine'
New-Item -ItemType Directory -Force -Path $diskStudioOutput | Out-Null
Copy-Item -LiteralPath $diskStudioBinary -Destination $diskStudioOutput
Get-FileHash -LiteralPath (Join-Path $diskStudioOutput 'diskstudio-mkbtrfs.exe') -Algorithm SHA256
