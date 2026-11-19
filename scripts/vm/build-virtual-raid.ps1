param()
$ErrorActionPreference = 'Stop'
Set-Location -LiteralPath ([IO.Path]::GetFullPath((Join-Path $PSScriptRoot '../..')))
$manifest=Get-Content -LiteralPath '.vm/virtual-raid-native/manifest.json' -Raw | ConvertFrom-Json
$desktop=(Get-FileHash -LiteralPath 'target/release/DiskStudio.exe').Hash.ToLowerInvariant()
if ((Get-FileHash -LiteralPath '.vm/artifacts/DiskStudio.exe').Hash -ne $desktop) { throw 'Desktop archive mismatch' }
$payload=Get-Item -LiteralPath '.vm/payload'
if (!$payload.PSIsContainer -or ($payload.Attributes -band [IO.FileAttributes]::ReparsePoint)) { throw 'Invalid payload location' }
$names = @('Virtual-Raid-4Kn-Prepare.exe','Virtual-Raid-4Kn-Inspect.exe')
$hashes = foreach($index in 0..2) {
    $item=$manifest.files[$index]
    $leaf="Virtual-Raid-$($index+1).raw"
    $path=Join-Path '.vm/virtual-raid-native' $leaf
    $data=Get-Item -LiteralPath $path
    if($item.name -cne $leaf -or $data.Length -ne 25MB -or ($data.Attributes -band [IO.FileAttributes]::ReparsePoint) -or
       $item.sha256 -cnotmatch '^[a-f0-9]{64}$' -or (Get-FileHash -LiteralPath $path).Hash -ne $item.sha256) { throw 'Bad virtual RAID member' }
    $item.sha256
}
foreach($leaf in $names + $manifest.files.name) {
    if(Test-Path -LiteralPath (Join-Path $payload.FullName $leaf)) { throw "Preserve existing payload $leaf" }
}
$desktopBefore = $env:DISKSTUDIO_ACCEPTANCE_DESKTOP_SHA256
$membersBefore = $env:DISKSTUDIO_VIRTUAL_RAID_MEMBER_SHA256
$flagsBefore = $env:RUSTFLAGS
try {
    $env:DISKSTUDIO_ACCEPTANCE_DESKTOP_SHA256=$desktop
    $env:DISKSTUDIO_VIRTUAL_RAID_MEMBER_SHA256=$hashes -join ','
    $env:RUSTFLAGS='-C target-feature=+crt-static'
    cargo build --locked --release --manifest-path scripts/vm/guest-checks/Cargo.toml --target-dir target/guest-checks
    if($LASTEXITCODE -ne 0) { throw 'Virtual RAID acceptance helper build failed' }
    foreach($leaf in $names) {
        Copy-Item -LiteralPath 'target/guest-checks/release/diskstudio-guest-checks.exe' -Destination (Join-Path $payload.FullName $leaf)
    }
    foreach($item in $manifest.files) {
        $target=Join-Path $payload.FullName $item.name
        Copy-Item -LiteralPath (Join-Path '.vm/virtual-raid-native' $item.name) -Destination $target
        if((Get-FileHash -LiteralPath $target).Hash -ne $item.sha256) { throw 'Copied member digest differs' }
    }
} finally {
    $env:DISKSTUDIO_ACCEPTANCE_DESKTOP_SHA256=$desktopBefore
    $env:DISKSTUDIO_VIRTUAL_RAID_MEMBER_SHA256=$membersBefore
    $env:RUSTFLAGS=$flagsBefore
}
