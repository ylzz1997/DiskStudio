param([switch]$Images, [switch]$Raid, [switch]$Thin, [switch]$Cow, [switch]$RaidDegraded, [switch]$CrossDeleted, [switch]$Raid10)
$ErrorActionPreference = 'Stop'
if (@($Images,$Raid,$Thin,$Cow,$RaidDegraded,$CrossDeleted,$Raid10 | Where-Object { $_ }).Count -gt 1) { throw 'Choose one LVM2 fixture set' }
$lvmFixture = if ($Raid) {
    @{ Directory = '.vm/lvm-raid-native'; Prefix = 'Lvm-Raid-4Kn'; Container = 'Lvm-Raid-Group.vhdx'; Cap = 128MB; Volumes = @('r1','r6'); Members = 1..5 }
} elseif ($Thin) {
    @{ Directory = '.vm/lvm-thin-native'; Prefix = 'Lvm-Thin-4Kn'; Container = 'Lvm-Thin-Group.vhdx'; Cap = 64MB; Volumes = @('origin','point1'); Members = 1,2 }
} elseif ($Cow) {
    @{ Directory = '.vm/lvm-cow-native'; Prefix = 'Lvm-Cow-4Kn'; Container = 'Lvm-Cow-Group.vhdx'; Cap = 64MB; Volumes = @('point1','sparse'); Members = 1,2 }
} elseif ($RaidDegraded) {
    @{ Directory = '.vm/lvm-raid-native'; Prefix = 'Lvm-Degraded-4Kn'; Container = 'Lvm-Raid-Group.vhdx'; Cap = 128MB; Volumes = @('r5','r6'); Members = 1..5 }
} elseif ($Raid10) {
    @{ Directory = '.vm/lvm-raid10-native'; Prefix = 'Lvm-Raid10-4Kn'; Container = 'Lvm-Raid10-Group.vhdx'; Cap = 128MB; Volumes = @('far','offset'); Members = 1..4 }
} elseif ($Images) {
    @{ Directory = '.vm/lvm-native'; Prefix = 'Lvm-Images-4Kn'; Container = 'Lvm-Group.vhdx'; Cap = 64MB; Members = 1,2 }
} elseif ($CrossDeleted) {
    @{ Directory = '.vm/lvm-cross-native'; Prefix = 'Lvm-Cross-4Kn'; Members = 1,2 }
} else {
    @{ Directory = '.vm/lvm-native'; Prefix = 'Lvm-Lab-4Kn'; Members = 1,2 }
}
$lvmContainerOnly = [bool]$lvmFixture.Volumes
Set-Location -LiteralPath ([IO.Path]::GetFullPath((Join-Path $PSScriptRoot '../..')))
$lvmDesktop = (Get-FileHash -LiteralPath 'target/release/diskstudio.exe').Hash.ToLowerInvariant()
if ((Get-FileHash -LiteralPath '.vm/artifacts/DiskStudio.exe').Hash -ne $lvmDesktop) {
    throw 'LVM2 acceptance requires the matching archived desktop build'
}
$lvmPayload = Get-Item -LiteralPath '.vm/payload'
$lvmFixtures = Get-Item -LiteralPath $lvmFixture.Directory
foreach ($lvmDirectory in $lvmPayload,$lvmFixtures) {
    if (!$lvmDirectory.PSIsContainer -or ($lvmDirectory.Attributes -band [IO.FileAttributes]::ReparsePoint)) {
        throw 'Fixture and payload directories must be ordinary folders'
    }
}
$lvmMediaPrefix = if ($CrossDeleted) { 'Lvm-Cross-PV' } else { 'Lvm-PV' }
$lvmFiles = @(foreach ($lvmMember in $lvmFixture.Members) {
    $lvmRaw = Get-Item -LiteralPath (Join-Path $lvmFixtures.FullName "pv-$lvmMember.raw")
    $lvmManifest = Get-Item -LiteralPath (Join-Path $lvmFixtures.FullName "pv-$lvmMember.json")
    $lvmLayout = Get-Content -Raw -LiteralPath $lvmManifest.FullName | ConvertFrom-Json
    if ($lvmRaw.PSIsContainer -or $lvmManifest.PSIsContainer -or
        (($lvmRaw.Attributes -bor $lvmManifest.Attributes) -band [IO.FileAttributes]::ReparsePoint) -or
        $lvmRaw.Length -ne 256MB -or $lvmLayout.member -ne $lvmMember -or
        $lvmLayout.imageSha256 -cnotmatch '^[a-f0-9]{64}$' -or
        (Get-FileHash -LiteralPath $lvmRaw.FullName).Hash -ne $lvmLayout.imageSha256) {
        throw "LVM2 fixture $lvmMember does not match its Linux manifest"
    }
    @{ Image = $lvmRaw.FullName; Manifest = $lvmManifest.FullName; Sha256 = $lvmLayout.imageSha256; Base = "$lvmMediaPrefix-$lvmMember"; Contents = $lvmLayout.files }
})
$lvmTargets = @("$($lvmFixture.Prefix)-Prepare.exe", "$($lvmFixture.Prefix)-Inspect.exe")
foreach ($lvmLeaf in $lvmTargets) {
    if (Test-Path -LiteralPath (Join-Path $lvmPayload.FullName $lvmLeaf)) {
        throw "Preserve existing payload $lvmLeaf"
    }
}
foreach ($lvmFile in $(if (!$lvmContainerOnly) { $lvmFiles })) {
    foreach ($lvmExtension in 'raw','json') {
        $lvmPath = Join-Path $lvmPayload.FullName "$($lvmFile.Base).$lvmExtension"
        if (!(Test-Path -LiteralPath $lvmPath)) { continue }
        $lvmItem = Get-Item -LiteralPath $lvmPath
        $lvmOriginal = if ($lvmExtension -eq 'raw') { $lvmFile.Image } else { $lvmFile.Manifest }
        if ((!$Images -and !$CrossDeleted) -or $lvmItem.PSIsContainer -or ($lvmItem.Attributes -band [IO.FileAttributes]::ReparsePoint) -or
            (Get-FileHash -LiteralPath $lvmPath).Hash -ne (Get-FileHash -LiteralPath $lvmOriginal).Hash) {
            throw "Preserve existing payload $lvmPath"
        }
    }
}
$lvmPreviousDesktop = $env:DISKSTUDIO_ACCEPTANCE_DESKTOP_SHA256
$lvmPreviousFixture = $env:DISKSTUDIO_LVM_FIXTURE_SHA256
$lvmPreviousContainer = $env:DISKSTUDIO_LVM_CONTAINER_SHA256
$lvmPreviousGroup = $env:DISKSTUDIO_LVM_GROUP_OUTPUT_SHA256
$lvmPreviousFlags = $env:RUSTFLAGS
try {
    $env:DISKSTUDIO_ACCEPTANCE_DESKTOP_SHA256 = $lvmDesktop
    $env:DISKSTUDIO_LVM_FIXTURE_SHA256 = $lvmFiles.Sha256 -join ','
    if ($lvmContainerOnly) {
        $lvmOutputProofs = foreach ($lvmVolume in $lvmFixture.Volumes) {
            $lvmProof = $lvmFiles[0].Contents.$lvmVolume
            if ($lvmProof.bytes -ne 1179648 -or $lvmProof.sha256 -cnotmatch '^[a-f0-9]{64}$' -or
                ($lvmFiles.Contents.$lvmVolume.sha256 | Sort-Object -Unique).Count -ne 1) {
                throw "Missing consistent Linux proof for $lvmVolume"
            }
            $lvmProof.sha256
        }
        $env:DISKSTUDIO_LVM_GROUP_OUTPUT_SHA256 = $lvmOutputProofs -join ','
    }
    if ($lvmFixture.Container) {
        $lvmContainer = Get-Item -LiteralPath (Join-Path $lvmFixtures.FullName $lvmFixture.Container)
        $lvmContainerDest = Join-Path $lvmPayload.FullName $lvmContainer.Name
        if ($lvmContainer.PSIsContainer -or ($lvmContainer.Attributes -band [IO.FileAttributes]::ReparsePoint) -or
            $lvmContainer.Length -le 0 -or $lvmContainer.Length -gt $lvmFixture.Cap -or
            (!$RaidDegraded -and (Test-Path -LiteralPath $lvmContainerDest))) {
            throw 'The LVM2 container fixture is invalid or has already been staged'
        }
        $env:DISKSTUDIO_LVM_CONTAINER_SHA256 = (Get-FileHash -LiteralPath $lvmContainer.FullName).Hash.ToLowerInvariant()
        if ($RaidDegraded) {
            $lvmExisting = Get-Item -LiteralPath $lvmContainerDest
            if ($lvmExisting.PSIsContainer -or ($lvmExisting.Attributes -band [IO.FileAttributes]::ReparsePoint) -or
                $lvmExisting.Length -ne $lvmContainer.Length -or
                (Get-FileHash -LiteralPath $lvmExisting.FullName).Hash -ne $env:DISKSTUDIO_LVM_CONTAINER_SHA256) {
                throw 'The staged RAID image differs from the Linux fixture'
            }
        }
    }
    $env:RUSTFLAGS = '-C target-feature=+crt-static'
    cargo build --locked --release --manifest-path scripts/vm/guest-checks/Cargo.toml --target-dir target/guest-checks
    if ($LASTEXITCODE -ne 0) { throw 'LVM2 guest helpers failed to build' }
    foreach ($lvmLeaf in $lvmTargets) {
        $lvmPath = Join-Path $lvmPayload.FullName $lvmLeaf
        Copy-Item -LiteralPath 'target/guest-checks/release/diskstudio-guest-checks.exe' -Destination $lvmPath
        Get-FileHash -LiteralPath $lvmPath
    }
    foreach ($lvmFile in $(if (!$lvmContainerOnly) { $lvmFiles })) {
        $lvmPath = Join-Path $lvmPayload.FullName "$($lvmFile.Base).raw"
        if (!(Test-Path -LiteralPath $lvmPath)) { Copy-Item -LiteralPath $lvmFile.Image -Destination $lvmPath }
        $lvmManifestPath = Join-Path $lvmPayload.FullName "$($lvmFile.Base).json"
        if (!(Test-Path -LiteralPath $lvmManifestPath)) { Copy-Item -LiteralPath $lvmFile.Manifest -Destination $lvmManifestPath }
        if ((Get-FileHash -LiteralPath $lvmPath).Hash -ne $lvmFile.Sha256) { throw 'LVM2 payload mismatch' }
    }
    if ($lvmFixture.Container -and !$RaidDegraded) {
        Copy-Item -LiteralPath $lvmContainer.FullName -Destination $lvmContainerDest
        if ((Get-FileHash -LiteralPath $lvmContainerDest).Hash -ne $env:DISKSTUDIO_LVM_CONTAINER_SHA256) {
            throw 'LVM2 container payload mismatch'
        }
    }
} finally {
    $env:DISKSTUDIO_ACCEPTANCE_DESKTOP_SHA256 = $lvmPreviousDesktop
    $env:DISKSTUDIO_LVM_FIXTURE_SHA256 = $lvmPreviousFixture
    $env:DISKSTUDIO_LVM_CONTAINER_SHA256 = $lvmPreviousContainer
    $env:DISKSTUDIO_LVM_GROUP_OUTPUT_SHA256 = $lvmPreviousGroup
    $env:RUSTFLAGS = $lvmPreviousFlags
}
