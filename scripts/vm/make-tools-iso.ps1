param(
    [switch]$Live,
    [string[]]$GuestTools,
    [switch]$BtrfsMoveFixture,
    [switch]$BtrfsSnapshotFixture,
    [switch]$BtrfsRetainedFixture,
    [switch]$BtrfsSha256Fixture,
    [switch]$BtrfsMembersFixture,
    [switch]$BtrfsParityFixture,
    [switch]$BtrfsAllocationFixture,
    [switch]$BtrfsLostFixture,
    [switch]$BtrfsLostMembersFixture,
    [switch]$LvmFixture
)
$ErrorActionPreference = 'Stop'
$diskStudioProject = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..\..'))
$diskStudioVmRoot = Join-Path $diskStudioProject '.vm'
$diskStudioPayload = Join-Path $diskStudioVmRoot 'payload'
$diskStudioStage = $diskStudioPayload
$diskStudioIso = Join-Path $diskStudioVmRoot 'media\diskstudio-tools.iso'
if ($Live) { $diskStudioIso = Join-Path $diskStudioVmRoot ('media\diskstudio-tools-' + [guid]::NewGuid().ToString() + '.iso') }
$diskStudioExe = Join-Path $diskStudioProject 'target\release\diskstudio.exe'
if (!(Test-Path -LiteralPath $diskStudioExe)) { throw 'Build the release desktop app first' }
if (!$Live -and (Test-Path -LiteralPath (Join-Path $diskStudioVmRoot 'run.json'))) {
    $diskStudioRun = Get-Content -LiteralPath (Join-Path $diskStudioVmRoot 'run.json') -Raw | ConvertFrom-Json
    if (Get-Process -Id $diskStudioRun.pid -ErrorAction SilentlyContinue) { throw 'Shut down the VM before replacing its optical media' }
}
if ($GuestTools -and !$Live) { throw 'A selected tool set requires an immutable -Live ISO' }
$diskStudioFixtures = @($BtrfsMoveFixture.IsPresent, $BtrfsSnapshotFixture.IsPresent, $BtrfsRetainedFixture.IsPresent, $BtrfsSha256Fixture.IsPresent, $BtrfsMembersFixture.IsPresent, $BtrfsParityFixture.IsPresent, $BtrfsAllocationFixture.IsPresent, $BtrfsLostFixture.IsPresent, $BtrfsLostMembersFixture.IsPresent) | Where-Object { $_ }
if (@($diskStudioFixtures).Count -gt 1) { throw 'Choose one Btrfs fixture for this ISO' }
$diskStudioLvmMedia = @{
    'Lvm-Lab-4Kn-Prepare.exe' = @{ MemberFiles = $true }
    'Lvm-Images-4Kn-Prepare.exe' = @{ MemberFiles = $true; Container = 'Lvm-Group.vhdx'; Cap = 64MB }
    'Lvm-Raid-4Kn-Prepare.exe' = @{ Container = 'Lvm-Raid-Group.vhdx'; Cap = 128MB }
    'Lvm-Thin-4Kn-Prepare.exe' = @{ Container = 'Lvm-Thin-Group.vhdx'; Cap = 64MB }
    'Lvm-Cow-4Kn-Prepare.exe' = @{ Container = 'Lvm-Cow-Group.vhdx'; Cap = 64MB }
    'Lvm-Degraded-4Kn-Prepare.exe' = @{ Container = 'Lvm-Raid-Group.vhdx'; Cap = 128MB }
    'Lvm-Cross-4Kn-Prepare.exe' = @{ MemberFiles = 'Lvm-Cross' }
    'Lvm-Raid10-4Kn-Prepare.exe' = @{ Container = 'Lvm-Raid10-Group.vhdx'; Cap = 128MB }
}
$diskStudioLvmPreparers = @($GuestTools | Where-Object { $diskStudioLvmMedia.ContainsKey($_) })
if ($LvmFixture -and (!$Live -or $diskStudioFixtures -or $diskStudioLvmPreparers.Count -ne 1)) {
    throw 'LVM2 fixtures require their preparation helper and exclusive immutable optical media'
}
$diskStudioMoveFresh = 'Btrfs-Lab-4Kn-Move-Prepare.exe' -in $GuestTools
$diskStudioMoveReuse = 'Btrfs-Lab-4Kn-Move-Reuse.exe' -in $GuestTools
if ($BtrfsMoveFixture -and (!$Live -or $diskStudioMoveFresh -eq $diskStudioMoveReuse)) {
    throw 'A Btrfs relocation fixture requires exactly one selected preparation helper on an immutable ISO'
}
if ($BtrfsSnapshotFixture -and (!$Live -or 'Btrfs-Lab-4Kn-Snapshot-Prepare.exe' -notin $GuestTools)) {
    throw 'A Btrfs snapshot fixture requires its selected guest preparation helper on an immutable ISO'
}
if ($BtrfsRetainedFixture -and (!$Live -or 'Btrfs-Lab-4Kn-Retained-Prepare.exe' -notin $GuestTools)) {
    throw 'A Btrfs retained fixture requires its selected guest preparation helper on an immutable ISO'
}
if ($BtrfsSha256Fixture -and (!$Live -or 'Btrfs-Lab-4Kn-Sha256-Prepare.exe' -notin $GuestTools)) {
    throw 'A Btrfs SHA-256 fixture requires its selected guest preparation helper on an immutable ISO'
}
if ($BtrfsMembersFixture -and (!$Live -or 'Btrfs-Lab-4Kn-Members-Prepare.exe' -notin $GuestTools)) {
    throw 'A Btrfs member fixture requires its selected guest preparation helper on an immutable ISO'
}
if ($BtrfsParityFixture -and (!$Live -or 'Btrfs-Lab-4Kn-Parity-Prepare.exe' -notin $GuestTools)) {
    throw 'A Btrfs parity fixture requires its selected guest preparation helper on an immutable ISO'
}
if ($BtrfsAllocationFixture -and (!$Live -or 'Btrfs-Lab-4Kn-Allocation-Prepare.exe' -notin $GuestTools)) {
    throw 'A Btrfs allocation fixture requires its selected guest preparation helper on an immutable ISO'
}
if ($BtrfsLostFixture -and (!$Live -or 'Btrfs-Lab-4Kn-Lost-Prepare.exe' -notin $GuestTools)) {
    throw 'A Btrfs lost fixture requires its selected guest preparation helper on an immutable ISO'
}
if ($BtrfsLostMembersFixture -and (!$Live -or 'Btrfs-Lab-4Kn-Lost-Group-Prepare.exe' -notin $GuestTools)) {
    throw 'A Btrfs lost member fixture requires its selected guest preparation helper on an immutable ISO'
}
foreach ($diskStudioDirectory in @($diskStudioVmRoot, $diskStudioPayload, (Join-Path $diskStudioVmRoot 'media'))) {
    if (Test-Path -LiteralPath $diskStudioDirectory) {
        if ((Get-Item -LiteralPath $diskStudioDirectory).Attributes -band [IO.FileAttributes]::ReparsePoint) { throw 'VM folders must not be links or junctions' }
    } else { New-Item -ItemType Directory -Path $diskStudioDirectory | Out-Null }
}
if (Test-Path -LiteralPath $diskStudioIso) { throw 'Tools ISO already exists; preserve it or remove that single file after shutting down the VM' }
if ($GuestTools) {
    # Use a fresh staging directory. Older payloads and ISO files may still be
    # needed for failure diagnosis and must not be changed.
    $diskStudioSelected = @{}
    foreach ($diskStudioName in $GuestTools) {
        if ($diskStudioName -notmatch '^[A-Za-z0-9][A-Za-z0-9_-]{0,110}\.exe$' -or
            $diskStudioName -ieq 'DiskStudio.exe' -or $diskStudioSelected.ContainsKey($diskStudioName)) {
            throw "Invalid or duplicate guest driver name: $diskStudioName"
        }
        $diskStudioItem = Get-Item -LiteralPath (Join-Path $diskStudioPayload $diskStudioName)
        if ($diskStudioItem.PSIsContainer -or ($diskStudioItem.Attributes -band [IO.FileAttributes]::ReparsePoint)) {
            throw "Guest driver must be a regular file: $diskStudioName"
        }
        $diskStudioSelected.Add($diskStudioName, $diskStudioItem.FullName)
    }
    if ($diskStudioFixtures) {
        $diskStudioFixtureNames = if ($BtrfsLostFixture) { @('Btrfs-Lost-Primary.raw', 'Btrfs-Lost-Mirror.raw') }
            elseif ($BtrfsLostMembersFixture) { @('Btrfs-Lost-Member-1.raw', 'Btrfs-Lost-Member-2.raw') }
            elseif ($BtrfsAllocationFixture) { 'Btrfs-Allocation-Source.raw' }
            elseif ($BtrfsParityFixture) { @('Btrfs-Raid5-1.raw', 'Btrfs-Raid6-1.raw', 'Btrfs-Raid6-2.raw') }
            elseif ($BtrfsMembersFixture) { @('Btrfs-Member-1.raw', 'Btrfs-Member-2.raw') }
            elseif ($BtrfsSha256Fixture) { 'Btrfs-Sha256-Source.raw' }
            elseif ($BtrfsRetainedFixture) { 'Btrfs-Retained-Source.raw' }
            elseif ($BtrfsSnapshotFixture) { 'Btrfs-Snapshot-Source.raw' }
            elseif ($diskStudioMoveReuse) { 'Btrfs-Move-Reuse.raw' } else { 'Btrfs-Move-Source.raw' }
        $diskStudioExpectedBytes = if ($BtrfsSnapshotFixture -or $BtrfsRetainedFixture -or $BtrfsSha256Fixture -or $BtrfsMembersFixture -or $BtrfsParityFixture -or $BtrfsAllocationFixture -or $BtrfsLostFixture -or $BtrfsLostMembersFixture) { 256MB }
            elseif ($diskStudioMoveReuse) { 768MB } else { 384MB }
        foreach ($diskStudioName in $diskStudioFixtureNames) {
            $diskStudioItem = Get-Item -LiteralPath (Join-Path $diskStudioPayload $diskStudioName)
            if ($diskStudioItem.PSIsContainer -or ($diskStudioItem.Attributes -band [IO.FileAttributes]::ReparsePoint) -or $diskStudioItem.Length -ne $diskStudioExpectedBytes) {
                throw 'Invalid Btrfs image for the immutable CD'
            }
            $diskStudioSelected.Add($diskStudioName, $diskStudioItem.FullName)
        }
    }
    if ($LvmFixture) {
        $diskStudioLvmSelection = $diskStudioLvmMedia[$diskStudioLvmPreparers[0]]
        $diskStudioLvmFiles = if ($diskStudioLvmSelection.MemberFiles) {
            $diskStudioMemberPrefix = if ($diskStudioLvmSelection.MemberFiles -eq 'Lvm-Cross') { 'Lvm-Cross' } else { 'Lvm' }
            foreach ($diskStudioMember in 1,2) {
                foreach ($diskStudioExtension in 'raw','json') { "$diskStudioMemberPrefix-PV-$diskStudioMember.$diskStudioExtension" }
            }
        }
        foreach ($diskStudioName in $diskStudioLvmFiles) {
            $diskStudioItem = Get-Item -LiteralPath (Join-Path $diskStudioPayload $diskStudioName)
            if ($diskStudioItem.PSIsContainer -or ($diskStudioItem.Attributes -band [IO.FileAttributes]::ReparsePoint) -or
                ($diskStudioName.EndsWith('.raw') -and $diskStudioItem.Length -ne 256MB) -or
                ($diskStudioName.EndsWith('.json') -and ($diskStudioItem.Length -le 0 -or $diskStudioItem.Length -gt 16KB))) {
                throw "Invalid immutable LVM2 media file $diskStudioName"
            }
            $diskStudioSelected.Add($diskStudioName, $diskStudioItem.FullName)
        }
        if ($diskStudioLvmSelection.Container) {
            $diskStudioItem = Get-Item -LiteralPath (Join-Path $diskStudioPayload $diskStudioLvmSelection.Container)
            if ($diskStudioItem.PSIsContainer -or ($diskStudioItem.Attributes -band [IO.FileAttributes]::ReparsePoint) -or
                $diskStudioItem.Length -le 0 -or $diskStudioItem.Length -gt $diskStudioLvmSelection.Cap) {
                throw 'Invalid immutable LVM2 VHDX fixture'
            }
            $diskStudioSelected.Add($diskStudioItem.Name, $diskStudioItem.FullName)
        }
    }
    $diskStudioStage = Join-Path $diskStudioVmRoot ('payload-selected-' + [guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory -Path $diskStudioStage | Out-Null
    foreach ($diskStudioName in $diskStudioSelected.Keys) {
        Copy-Item -LiteralPath $diskStudioSelected[$diskStudioName] -Destination (Join-Path $diskStudioStage $diskStudioName)
    }
}
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
