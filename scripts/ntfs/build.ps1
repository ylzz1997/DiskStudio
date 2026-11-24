$ErrorActionPreference = 'Stop'
$project = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '../..'))
$version = '2026.9.28'
$archive = Join-Path $project "third-party/ntfs-3g/ntfs-3g_ntfsprogs-$version.tgz"
$archiveSha256 = '350d9415c59f7c3fa74e23985ad2d56423c6a168337368f672e2e362d8f295c8'
if (-not (Test-Path -LiteralPath $archive -PathType Leaf)) {
    throw "The packaged NTFS-3G source archive is missing from $archive"
}
if ((Get-FileHash -LiteralPath $archive).Hash -ne $archiveSha256) {
    throw 'NTFS-3G archive hash mismatch'
}
$toolchain = Join-Path $project '.vm/tools/w64devkit/bin'
$gcc = Join-Path $toolchain 'x86_64-w64-mingw32-gcc.exe'
if (-not (Test-Path -LiteralPath $gcc -PathType Leaf)) { throw 'w64devkit is missing' }
$build = Join-Path $project ('.vm/ntfs-engine-' + [guid]::NewGuid())
$source = Join-Path $build "ntfs-3g-$version"
New-Item -ItemType Directory -Path $build | Out-Null
tar.exe -xf $archive -C $build
if ($LASTEXITCODE -ne 0) { throw "NTFS-3G archive extraction failed in $build" }

$configure = @'
set -eu
export PATH="/usr/bin:$(cygpath -u "$2"):$PATH"
cd "$(cygpath -u "$1")"
options=(--build=x86_64-w64-mingw32 --host=x86_64-w64-mingw32
  --disable-ntfs-3g --disable-plugins --disable-shared --disable-posix-acls
  --disable-xattr-mappings)
./configure "${options[@]}" CFLAGS="-O2 -D_WIN32_WINNT=0x0601" > configure-win.log 2>&1
'@
& 'C:/Program Files/Git/bin/bash.exe' --noprofile --norc -c $configure diskstudio-ntfs $source $toolchain
if ($LASTEXITCODE -ne 0) { throw "NTFS-3G configure failed in $source" }
node (Join-Path $PSScriptRoot 'patch.mjs') $source
if ($LASTEXITCODE -ne 0) { throw 'NTFS-3G source patch failed' }

# Replace the default device implementation at link time. Neither upstream
# platform I/O implementation is linked into the engine.
$pathBeforeBuild = $env:Path
Push-Location $source
try {
    $env:Path = "$toolchain;$pathBeforeBuild"
    [IO.File]::AppendAllText((Join-Path $source 'config.h'), "`n#define HAVE_DAEMON 1`n#define HAVE_FFS 1`n")
    $libraries = @(
        Get-ChildItem -LiteralPath libntfs-3g -Filter '*.c' |
        Where-Object { $_.Name -notin @('win32_io.c', 'unix_io.c') } |
        Sort-Object Name | ForEach-Object FullName
    )
    $formatter = @('attrdef.c', 'boot.c', 'mkntfs.c', 'ntfsresize.c', 'sd.c', 'utils.c') |
        ForEach-Object { Join-Path 'ntfsprogs' $_ }
    $engine = Join-Path $build 'diskstudio-ntfs.exe'
    & $gcc -static -O2 -mno-ms-bitfields -DHAVE_CONFIG_H -D_FILE_OFFSET_BITS=64 -D_WIN32_WINNT=0x0601 -I. -Iinclude/ntfs-3g `
        -include (Join-Path $PSScriptRoot 'windows_compat.h') @libraries @formatter `
        (Join-Path $PSScriptRoot 'engine.c') -o $engine *> (Join-Path $build 'build-win.log')
    if ($LASTEXITCODE -ne 0) { throw "NTFS engine compilation failed in $build" }
    Get-FileHash -LiteralPath $engine
} finally {
    Pop-Location
    $env:Path = $pathBeforeBuild
}
