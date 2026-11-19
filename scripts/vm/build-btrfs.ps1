param([switch]$Relocation, [switch]$ReuseFixture, [switch]$Snapshot, [switch]$Retained, [switch]$Sha256, [switch]$Members, [switch]$Parity, [switch]$Allocation, [switch]$LostPartition, [switch]$LostMembers)
$ErrorActionPreference = 'Stop'
if ($ReuseFixture -and !$Relocation) { throw 'Reusing the empty fixture is a relocation acceptance case' }
$fixtureModes = @($Relocation.IsPresent, $Snapshot.IsPresent, $Retained.IsPresent, $Sha256.IsPresent, $Members.IsPresent, $Parity.IsPresent, $Allocation.IsPresent, $LostPartition.IsPresent, $LostMembers.IsPresent) | Where-Object { $_ }
if (@($fixtureModes).Count -gt 1) { throw 'Choose one Btrfs acceptance case' }
$diskStudioProject = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..\..'))
$previousFlags = $env:RUSTFLAGS
$previousHash = $env:DISKSTUDIO_ACCEPTANCE_DESKTOP_SHA256
$previousFixture = $env:DISKSTUDIO_BTRFS_MOVE_SOURCE_SHA256
$previousSnapshot = $env:DISKSTUDIO_BTRFS_SNAPSHOT_SOURCE_SHA256
$previousRetained = $env:DISKSTUDIO_BTRFS_RETAINED_SOURCE_SHA256
$previousSha256 = $env:DISKSTUDIO_BTRFS_SHA256_SOURCE_SHA256
$previousMember1 = $env:DISKSTUDIO_BTRFS_MEMBER_1_SHA256
$previousMember2 = $env:DISKSTUDIO_BTRFS_MEMBER_2_SHA256
$previousParity = $env:DISKSTUDIO_BTRFS_PARITY_SHA256
$previousAllocation = $env:DISKSTUDIO_BTRFS_ALLOCATION_SHA256
$previousLostPartition = $env:DISKSTUDIO_BTRFS_LOST_SHA256
Push-Location $diskStudioProject
try {
    $fixturePayload = $null
    $memberFiles = @()
    $desktop = (Get-FileHash -LiteralPath 'target/release/diskstudio.exe').Hash.ToLowerInvariant()
    if ((Get-FileHash -LiteralPath '.vm/artifacts/DiskStudio.exe').Hash.ToLowerInvariant() -ne $desktop) {
        throw 'Desktop artifact mismatch'
    }
    $payload = Get-Item -LiteralPath '.vm/payload'
    if (!$payload.PSIsContainer -or ($payload.Attributes -band [IO.FileAttributes]::ReparsePoint)) {
        throw 'Invalid payload directory'
    }
    if ($LostPartition) {
        $fixtureDir = Get-Item -LiteralPath '.vm/btrfs-native-checksum'
        if (!$fixtureDir.PSIsContainer -or ($fixtureDir.Attributes -band [IO.FileAttributes]::ReparsePoint)) {
            throw 'Expected ordinary Linux fixture directory'
        }
        $memberFiles = @(foreach ($algorithm in 'xxhash','blake2') {
            $source = Get-Item -LiteralPath (Join-Path $fixtureDir.FullName "$algorithm.raw")
            $manifestPath = Join-Path $fixtureDir.FullName "$algorithm-manifest.json"
            $manifest = Get-Content -LiteralPath $manifestPath -Raw | ConvertFrom-Json
            $leaf = if ($algorithm -eq 'xxhash') { 'Btrfs-Lost-Primary.raw' } else { 'Btrfs-Lost-Mirror.raw' }
            $destination = Join-Path $payload.FullName $leaf
            if ($source.PSIsContainer -or ($source.Attributes -band [IO.FileAttributes]::ReparsePoint) -or
                $source.Length -ne 256MB -or $manifest.algorithm -ne $algorithm -or
                $manifest.imageSha256 -cnotmatch '^[a-f0-9]{64}$' -or
                (Get-FileHash -LiteralPath $source.FullName).Hash -ne $manifest.imageSha256 -or
                (Test-Path -LiteralPath $destination)) {
                throw "Expected a verified, unstaged $algorithm Btrfs image"
            }
            if ($algorithm -eq 'blake2') {
                $stage = New-Item -ItemType Directory -Path (Join-Path '.vm' ('btrfs-lost-media-' + [guid]::NewGuid().ToString('N')))
                $ordinaryCopy = Join-Path $stage.FullName $leaf
                Copy-Item -LiteralPath $source.FullName -Destination $ordinaryCopy
                $stream = [IO.File]::Open($ordinaryCopy, [IO.FileMode]::Open, [IO.FileAccess]::Write, [IO.FileShare]::None)
                try {
                    $stream.Position = 64KB
                    $stream.Write([byte[]]::new(4096), 0, 4096)
                    $stream.Flush($true)
                } finally { $stream.Dispose() }
                $source = Get-Item -LiteralPath $ordinaryCopy
            }
            @{ source = $source.FullName; destination = $destination; sha256 = (Get-FileHash -LiteralPath $source.FullName).Hash.ToLowerInvariant() }
        })
        $env:DISKSTUDIO_BTRFS_LOST_SHA256 = $memberFiles.sha256 -join ','
    } elseif ($Parity) {
        $memberFiles = @(foreach ($member in @(
            @{ profile = 'raid5'; index = 1; name = 'Btrfs-Raid5-1.raw' },
            @{ profile = 'raid6'; index = 1; name = 'Btrfs-Raid6-1.raw' },
            @{ profile = 'raid6'; index = 2; name = 'Btrfs-Raid6-2.raw' }
        )) {
            $path = ".vm/btrfs-native-parity/$($member.profile)-$($member.index)"
            $source = Get-Item -LiteralPath "$path.raw"
            $manifest = Get-Content -LiteralPath "$path-manifest.json" -Raw | ConvertFrom-Json
            $destination = Join-Path $payload.FullName $member.name
            if ($source.PSIsContainer -or ($source.Attributes -band [IO.FileAttributes]::ReparsePoint) -or
                $source.Length -ne 256MB -or $manifest.profile -ne $member.profile -or
                $manifest.member -ne $member.index -or $manifest.imageSha256 -cnotmatch '^[a-f0-9]{64}$' -or
                (Get-FileHash -LiteralPath $source.FullName).Hash -ne $manifest.imageSha256 -or
                (Test-Path -LiteralPath $destination)) {
                throw "Expected verified and unstaged parity image $path"
            }
            @{ source = $source.FullName; destination = $destination; sha256 = $manifest.imageSha256 }
        })
        $env:DISKSTUDIO_BTRFS_PARITY_SHA256 = $memberFiles.sha256 -join ','
    } elseif ($Members -or $LostMembers) {
        $memberFiles = @(foreach ($index in 1,2) {
            $source = Get-Item -LiteralPath ".vm/btrfs-native-members/member-$index.raw"
            $manifest = Get-Content -LiteralPath ".vm/btrfs-native-members/member-$index-manifest.json" -Raw | ConvertFrom-Json
            $leaf = if ($LostMembers) { "Btrfs-Lost-Member-$index.raw" } else { "Btrfs-Member-$index.raw" }
            $destination = Join-Path $payload.FullName $leaf
            if ($source.PSIsContainer -or ($source.Attributes -band [IO.FileAttributes]::ReparsePoint) -or
                $source.Length -ne 256MB -or $manifest.member -ne $index -or
                $manifest.imageSha256 -cnotmatch '^[a-f0-9]{64}$' -or
                (Get-FileHash -LiteralPath $source.FullName).Hash -ne $manifest.imageSha256 -or
                (Test-Path -LiteralPath $destination)) {
                throw "Expected a verified and unstaged Btrfs member $index"
            }
            @{ source = $source.FullName; destination = $destination; sha256 = $manifest.imageSha256 }
        })
        $env:DISKSTUDIO_BTRFS_MEMBER_1_SHA256 = $memberFiles[0].sha256
        $env:DISKSTUDIO_BTRFS_MEMBER_2_SHA256 = $memberFiles[1].sha256
    } elseif ($Snapshot -or $Retained -or $Sha256 -or $Allocation) {
        $kind = if ($Allocation) { 'allocation' } elseif ($Sha256) { 'sha256' } elseif ($Retained) { 'retained' } else { 'snapshot' }
        $fixtureDir = if ($Sha256) { '.vm/btrfs-native-checksum' } else { ".vm/btrfs-native-$kind" }
        $fixture = Get-Item -LiteralPath "$fixtureDir/$kind.raw"
        $manifest = Get-Content -LiteralPath "$fixtureDir/$kind-manifest.json" -Raw | ConvertFrom-Json
        $expected = $manifest.imageSha256
        if ($fixture.PSIsContainer -or ($fixture.Attributes -band [IO.FileAttributes]::ReparsePoint) -or
            $fixture.Length -ne 256MB -or $expected -cnotmatch '^[a-f0-9]{64}$' -or
            (Get-FileHash -LiteralPath $fixture.FullName).Hash -ne $expected) {
            throw 'Expected the verified Linux Btrfs image'
        }
        if ($Allocation) {
            if ($manifest.kind -ne 'allocation') { throw 'Missing Linux allocation fixture proof' }
            $fixturePayload = Join-Path $payload.FullName 'Btrfs-Allocation-Source.raw'
            $env:DISKSTUDIO_BTRFS_ALLOCATION_SHA256 = $expected
        } elseif ($Sha256) {
            if ($manifest.algorithm -ne 'sha256') { throw 'Missing Linux SHA-256 test fixture proof' }
            $fixturePayload = Join-Path $payload.FullName 'Btrfs-Sha256-Source.raw'
            $env:DISKSTUDIO_BTRFS_SHA256_SOURCE_SHA256 = $expected
        } elseif ($Retained) {
            if ($manifest.kind -ne 'retained' -or !$manifest.oldTree) { throw 'Missing retained transaction proof' }
            $fixturePayload = Join-Path $payload.FullName 'Btrfs-Retained-Source.raw'
            $env:DISKSTUDIO_BTRFS_RETAINED_SOURCE_SHA256 = $expected
        } else {
            $fixturePayload = Join-Path $payload.FullName 'Btrfs-Snapshot-Source.raw'
            $env:DISKSTUDIO_BTRFS_SNAPSHOT_SOURCE_SHA256 = $expected
        }
    } elseif ($Relocation) {
        $sizeMiB = if ($ReuseFixture) { 768 } else { 384 }
        $fixture = Get-Item -LiteralPath ".vm/btrfs-upstream-869058445ace/relocation-$sizeMiB-labelled.raw"
        if ($fixture.PSIsContainer -or ($fixture.Attributes -band [IO.FileAttributes]::ReparsePoint) -or $fixture.Length -ne ($sizeMiB * 1MB)) {
            throw "Expected the prepared $sizeMiB MiB Btrfs relocation image"
        }
        $fixtureName = if ($ReuseFixture) { 'Btrfs-Move-Reuse.raw' } else { 'Btrfs-Move-Source.raw' }
        $fixturePayload = Join-Path $payload.FullName $fixtureName
        $expected = (Get-FileHash -LiteralPath $fixture.FullName).Hash.ToLowerInvariant()
        $env:DISKSTUDIO_BTRFS_MOVE_SOURCE_SHA256 = $expected
    }
    if ($fixturePayload -and (Test-Path -LiteralPath $fixturePayload)) {
        throw 'Preserve the existing fixture payload'
    }
    $names = if ($LostPartition) { @('Lost-Prepare', 'Lost-Inspect') }
        elseif ($LostMembers) { @('Lost-Group-Prepare', 'Lost-Group-Inspect') }
        elseif ($Allocation) { @('Allocation-Prepare', 'Allocation-Archive', 'Allocation-Inspect') }
        elseif ($Parity) { @('Parity-Prepare', 'Parity-Inspect') }
        elseif ($Members) { @('Members-Prepare', 'Members-Inspect') }
        elseif ($Sha256) { @('Sha256-Prepare', 'Sha256-Inspect') }
        elseif ($Retained) { @('Retained-Prepare', 'Retained-Inspect') }
        elseif ($Snapshot) { @('Snapshot-Prepare', 'Snapshot-Inspect') }
        elseif ($ReuseFixture) { @('Move-Reuse', 'Move-Reuse-Inspect') }
        elseif ($Relocation) { @('Move-Prepare', 'Move-Inspect') }
        else { @('Create', 'Inspect', 'Online') }
    $entries = foreach ($name in $names) {
        $entry = Join-Path $payload.FullName "Btrfs-Lab-4Kn-$name.exe"
        if (Test-Path -LiteralPath $entry) { throw "Preserve existing guest entry: $entry" }
        $entry
    }
    $env:DISKSTUDIO_ACCEPTANCE_DESKTOP_SHA256 = $desktop
    $env:RUSTFLAGS = '-C target-feature=+crt-static'
    cargo build --locked --release --manifest-path scripts/vm/guest-checks/Cargo.toml --target-dir target/guest-checks
    if ($LASTEXITCODE -ne 0) { throw 'Btrfs guest tools failed to build' }
    foreach ($entry in $entries) {
        Copy-Item -LiteralPath 'target/guest-checks/release/diskstudio-guest-checks.exe' -Destination $entry
    }
    Get-FileHash -LiteralPath $entries -Algorithm SHA256
    if ($fixturePayload) {
        Copy-Item -LiteralPath $fixture.FullName -Destination $fixturePayload
        $copied = Get-FileHash -LiteralPath $fixturePayload -Algorithm SHA256
        if ($copied.Hash.ToLowerInvariant() -ne $expected) { throw 'Fixture payload mismatch' }
        $copied
    }
    foreach ($member in $memberFiles) {
        Copy-Item -LiteralPath $member.source -Destination $member.destination
        $copied = Get-FileHash -LiteralPath $member.destination
        if ($copied.Hash -ne $member.sha256) { throw 'Btrfs member payload mismatch' }
        $copied
    }
} finally {
    $env:RUSTFLAGS = $previousFlags
    $env:DISKSTUDIO_ACCEPTANCE_DESKTOP_SHA256 = $previousHash
    $env:DISKSTUDIO_BTRFS_MOVE_SOURCE_SHA256 = $previousFixture
    $env:DISKSTUDIO_BTRFS_SNAPSHOT_SOURCE_SHA256 = $previousSnapshot
    $env:DISKSTUDIO_BTRFS_RETAINED_SOURCE_SHA256 = $previousRetained
    $env:DISKSTUDIO_BTRFS_SHA256_SOURCE_SHA256 = $previousSha256
    $env:DISKSTUDIO_BTRFS_MEMBER_1_SHA256 = $previousMember1
    $env:DISKSTUDIO_BTRFS_MEMBER_2_SHA256 = $previousMember2
    $env:DISKSTUDIO_BTRFS_PARITY_SHA256 = $previousParity
    $env:DISKSTUDIO_BTRFS_ALLOCATION_SHA256 = $previousAllocation
    $env:DISKSTUDIO_BTRFS_LOST_SHA256 = $previousLostPartition
    Pop-Location
}
