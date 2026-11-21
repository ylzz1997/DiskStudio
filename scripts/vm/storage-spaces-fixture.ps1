param([switch]$Verify, [switch]$SeedWitness, [switch]$InspectWitnessHandles)

$ErrorActionPreference = 'Stop'
$witnessMode = $SeedWitness -or $InspectWitnessHandles
if (($Verify -and $witnessMode) -or ($SeedWitness -and $InspectWitnessHandles)) { throw 'Choose a single fixture operation.' }
$machine = @(Get-CimInstance -Query 'SELECT UUID,Name,Vendor FROM Win32_ComputerSystemProduct')
if ($machine.Count -ne 1 -or $machine[0].UUID -ne 'e20adb9f-f665-432e-9a62-dd5d897b06a0' -or
    $machine[0].Name -ne 'DiskStudio-Test' -or $machine[0].Vendor -ne 'DiskStudio') {
    throw 'Only run this fixture in DiskStudio-Test after QMP preflight.'
}
$serials = @('DS-TEST-A-0001      _00000001.', 'DS-TEST-B-0001      _00000001.')
$subsystems = @(Get-StorageSubSystem | Where-Object {
    $_.Model -in @('Windows Storage', 'Storage Spaces') -and
    $_.Manufacturer -eq 'Microsoft Corporation' -and
    $_.ObjectId -match '[\\/]Storage[\\/]Providers_v2[\\/]SPACES_StorageSubsystem\.ObjectId='
})
if ($subsystems.Count -ne 1 -or [string]::IsNullOrWhiteSpace($subsystems[0].UniqueId)) {
    throw 'Expected one local Microsoft Storage Spaces subsystem.'
}
$subsystem = $subsystems[0]
$name = 'DS-TEST-POOL'
function Find-PhysicalTargets {
    $physical = @(Get-PhysicalDisk -StorageSubsystem $subsystem)
    foreach ($serial in $serials) {
        $rows = @($physical | Where-Object { ([string]$_.SerialNumber).Trim() -ceq $serial })
        if ($rows.Count -ne 1 -or $rows[0].Size -ne 16GB -or
            $rows[0].LogicalSectorSize -ne 4096 -or $rows[0].PhysicalSectorSize -ne 4096 -or
            [string]::IsNullOrWhiteSpace($rows[0].UniqueId)) {
            throw 'Physical disk identity or 4Kn geometry is not unique.'
        }
        $rows[0]
    }
}

function Confirm-FixturePool {
    $pools = @(Get-StoragePool -StorageSubSystem $subsystem -IsPrimordial $false)
    if ($pools.Count -ne 1 -or $pools[0].FriendlyName -cne $name -or $pools[0].IsClustered -ne $false -or
        $pools[0].IsReadOnly -ne $false -or [string]::IsNullOrWhiteSpace($pools[0].UniqueId)) {
        throw 'Expected one local writable fixture pool.'
    }
    $members = @(Get-PhysicalDisk -StoragePool $pools[0])
    if ($members.Count -ne 2) { throw 'The pool does not contain exactly two members.' }
    foreach ($disk in (Find-PhysicalTargets)) {
        if (@($members | Where-Object { $_.UniqueId -ceq $disk.UniqueId }).Count -ne 1) {
            throw 'The pool does not belong to the exact disposable members.'
        }
    }
    $pools[0]
}

if (-not $Verify -and -not $witnessMode) {
    $legacy = @(Get-CimInstance -Query 'SELECT Index,SerialNumber,BytesPerSector FROM Win32_DiskDrive')
    $pools = @(Get-StoragePool)
    if ($legacy.Count -ne 3 -or $pools.Count -eq 0 -or
        @($pools | Where-Object { $_.IsPrimordial -ne $true }).Count -ne 0) {
        throw 'Fixture creation requires the original three-device inventory and no concrete pools.'
    }
    $basic = @(
        foreach ($serial in $serials) {
            $rows = @($legacy | Where-Object { ([string]$_.SerialNumber).Trim() -ceq $serial })
            if ($rows.Count -ne 1 -or $rows[0].BytesPerSector -ne 4096) {
                throw 'Legacy guest device identity or geometry did not match.'
            }
            $disk = Get-Disk -Number $rows[0].Index
            if ($disk.SerialNumber.Trim() -cne $serial -or $disk.Size -ne 16GB -or
                $disk.LogicalSectorSize -ne 4096 -or $disk.PhysicalSectorSize -ne 4096 -or
                $disk.PartitionStyle -ne 'RAW' -or $disk.NumberOfPartitions -ne 0 -or
                $disk.IsBoot -ne $false -or $disk.IsSystem -ne $false -or
                $disk.IsClustered -ne $false -or $disk.IsReadOnly -ne $false) {
                throw 'Only the two blank unprotected 4Kn disks may form the fixture.'
            }
            $disk
        }
    )
    $initial = @(Find-PhysicalTargets)
    if ($initial.Count -ne 2 -or $initial[0].UniqueId -ceq $initial[1].UniqueId) {
        throw 'The two physical objects must remain distinct.'
    }
    foreach ($disk in $basic) {
        if ($disk.IsOffline) { $disk | Set-Disk -IsOffline $false -ErrorAction Stop }
    }
    $targets = @(Find-PhysicalTargets)
    if ($targets.Count -ne 2 -or
        $targets[0].UniqueId -cne $initial[0].UniqueId -or $targets[1].UniqueId -cne $initial[1].UniqueId -or
        @($targets | Where-Object { $_.CanPool -ne $true }).Count -ne 0) {
        throw 'Windows does not report the same disposable disks as eligible pool members.'
    }
    $poolOptions = @{
        FriendlyName = $name; StorageSubSystemUniqueId = $subsystem.UniqueId
        PhysicalDisks = $targets; LogicalSectorSizeDefault = 4096
    }
    New-StoragePool @poolOptions -ErrorAction Stop | Out-Null
    $pool = Confirm-FixturePool
    $mirrorOptions = @{
        StoragePoolUniqueId = $pool.UniqueId; FriendlyName = 'DS-TEST-MIRROR'
        ResiliencySettingName = 'Mirror'; NumberOfDataCopies = 2; NumberOfColumns = 1
        Interleave = 262144; ProvisioningType = 'Fixed'; Size = 1GB
    }
    New-VirtualDisk @mirrorOptions -ErrorAction Stop | Out-Null
    $pool = Confirm-FixturePool
    $thinOptions = @{
        StoragePoolUniqueId = $pool.UniqueId; FriendlyName = 'DS-TEST-THIN'
        ResiliencySettingName = 'Simple'; NumberOfColumns = 2; Interleave = 65536
        ProvisioningType = 'Thin'; IsManualAttach = $true; Size = 1GB
    }
    New-VirtualDisk @thinOptions -ErrorAction Stop | Out-Null
}

$confirmed = Confirm-FixturePool
$spaces = @(Get-VirtualDisk -StoragePool $confirmed)
$mirror = @($spaces | Where-Object FriendlyName -ceq 'DS-TEST-MIRROR')
$thin = @($spaces | Where-Object FriendlyName -ceq 'DS-TEST-THIN')
if ($spaces.Count -ne 2 -or $mirror.Count -ne 1 -or $thin.Count -ne 1 -or
    $mirror[0].ResiliencySettingName -ne 'Mirror' -or $mirror[0].NumberOfDataCopies -ne 2 -or
    $mirror[0].NumberOfColumns -ne 1 -or $mirror[0].Interleave -ne 262144 -or
    $mirror[0].Size -ne 1GB -or $mirror[0].ProvisioningType -ne 'Fixed' -or
    $mirror[0].IsManualAttach -ne $false -or
    $thin[0].ResiliencySettingName -ne 'Simple' -or $thin[0].NumberOfColumns -ne 2 -or
    $thin[0].NumberOfDataCopies -ne 1 -or $thin[0].Interleave -ne 65536 -or $thin[0].Size -ne 1GB -or
    $thin[0].ProvisioningType -ne 'Thin' -or $thin[0].IsManualAttach -ne $true -or
    (-not $witnessMode -and $thin[0].OperationalStatus -notcontains 'Detached')) {
    throw 'Windows did not report the expected mirror and thin stripe geometry.'
}
$attached = @(Get-Disk -VirtualDisk $mirror[0] -ErrorAction Stop)
$thinDisks = @(Get-Disk -VirtualDisk $thin[0] -ErrorAction SilentlyContinue)
if ($attached.Count -ne 1 -or $attached[0].BusType -ne 'Spaces' -or $attached[0].Size -ne 1GB -or
    $attached[0].PartitionStyle -ne 'RAW' -or $attached[0].NumberOfPartitions -ne 0 -or
    $thinDisks.Count -gt 1 -or (-not $witnessMode -and $thinDisks.Count -ne 0)) {
    throw 'Expected an attached uninitialized mirror and a detached thin space.'
}
if ($witnessMode) {
    $source = @'
using System;
using System.ComponentModel;
using System.IO;
using System.Runtime.InteropServices;
using System.Security.Cryptography;
using System.Text;
using Microsoft.Win32.SafeHandles;

public static class DiskStudioSpacesWitness {
    const int Sector = 4096;
    const long MiB = 1048576;
    const long Capacity = 1024 * MiB;
    static readonly long[] Offsets = {
        0, Sector, 60 * 1024, 64 * 1024, 68 * 1024, 128 * 1024, 192 * 1024, 256 * 1024,
        256 * MiB - Sector, 256 * MiB, 256 * MiB + 64 * 1024,
        512 * MiB - Sector, 512 * MiB, 512 * MiB + 64 * 1024, 512 * MiB + 128 * 1024,
        768 * MiB, 768 * MiB + 64 * 1024, Capacity - Sector
    };

    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    static extern SafeFileHandle CreateFile(string name, uint access, uint shared, IntPtr security,
        uint disposition, uint flags, IntPtr template);

    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool DeviceIoControl(SafeFileHandle disk, uint command, byte[] input, int inputSize,
        byte[] output, int outputSize, out int count, IntPtr overlapped);

    static byte[] Query(SafeFileHandle disk, uint command, byte[] input, int minimum) {
        var bytes = new byte[4096];
        int count;
        if (!DeviceIoControl(disk, command, input, input == null ? 0 : input.Length,
                             bytes, bytes.Length, out count, IntPtr.Zero)) {
            throw new Win32Exception(Marshal.GetLastWin32Error());
        }
        if (count < minimum) throw new IOException("Truncated device descriptor");
        return bytes;
    }

    static void Verify(SafeFileHandle disk, uint index) {
        var number = Query(disk, 0x2d1080, null, 12);
        var geometry = Query(disk, 0x70000, null, 24);
        var length = Query(disk, 0x7405c, null, 8);
        var descriptor = Query(disk, 0x2d1400, new byte[12], 36);
        var partition = BitConverter.ToUInt32(number, 8);
        if (BitConverter.ToUInt32(number, 0) != 7 || BitConverter.ToUInt32(number, 4) != index ||
            (partition != 0 && partition != UInt32.MaxValue) ||
            BitConverter.ToUInt32(geometry, 20) != Sector || BitConverter.ToInt64(length, 0) != Capacity ||
            BitConverter.ToUInt32(descriptor, 28) != 16) {
            throw new IOException("Unexpected opened Storage Spaces disk identity or geometry");
        }
    }

    public static string Inspect(uint index) {
        if (index == 0 || index > 15) throw new IOException("Unexpected fixture target");
        using (var device = CreateFile(@"\\.\PhysicalDrive" + index, 0x80000000, 3, IntPtr.Zero, 3, 0, IntPtr.Zero)) {
            if (device.IsInvalid) throw new Win32Exception(Marshal.GetLastWin32Error());
            var number = Query(device, 0x2d1080, null, 12);
            var geometry = Query(device, 0x70000, null, 24);
            var length = Query(device, 0x7405c, null, 8);
            var descriptor = Query(device, 0x2d1400, new byte[12], 36);
            return String.Format("Type {0}, device {1}, partition {2}, sector {3}, length {4}, bus {5}",
                BitConverter.ToUInt32(number, 0), BitConverter.ToUInt32(number, 4), BitConverter.ToUInt32(number, 8),
                BitConverter.ToUInt32(geometry, 20), BitConverter.ToInt64(length, 0), BitConverter.ToUInt32(descriptor, 28));
        }
    }

    static byte[] Witness(byte kind, int index) {
        var bytes = new byte[Sector];
        for (int i = 0; i < bytes.Length; i++) bytes[i] = (byte)(i * 17 + index * 73 + kind * 31);
        Array.Copy(Encoding.ASCII.GetBytes("DS-SPACES-DATA1\0"), bytes, 16);
        bytes[16] = kind;
        Array.Copy(BitConverter.GetBytes(Offsets[index]), 0, bytes, 24, 8);
        return bytes;
    }

    static void ReadSector(FileStream stream, long offset, byte[] bytes) {
        stream.Seek(offset, SeekOrigin.Begin);
        if (stream.Read(bytes, 0, bytes.Length) != bytes.Length) throw new IOException("Short witness read");
    }

    public static string Seed(uint index, byte kind) {
        if (index == 0 || index > 15 || (kind != 1 && kind != 2)) throw new IOException("Unexpected fixture target");
        using (var device = CreateFile(@"\\.\PhysicalDrive" + index, 0xc0000000, 3, IntPtr.Zero, 3, 0x80000000, IntPtr.Zero)) {
            if (device.IsInvalid) throw new Win32Exception(Marshal.GetLastWin32Error());
            Verify(device, index);
            using (var stream = new FileStream(device, FileAccess.ReadWrite, Sector)) {
                var sector = new byte[Sector];
                foreach (long offset in Offsets) {
                    ReadSector(stream, offset, sector);
                    if (Array.Exists(sector, b => b != 0)) throw new IOException("This witness space is not blank");
                }
                Verify(device, index);
                for (int i = 0; i < Offsets.Length; i++) {
                    stream.Seek(Offsets[i], SeekOrigin.Begin);
                    stream.Write(Witness(kind, i), 0, Sector);
                }
                stream.Flush(true);
                Verify(device, index);
                using (var digest = SHA256.Create()) {
                    for (int i = 0; i < Offsets.Length; i++) {
                        ReadSector(stream, Offsets[i], sector);
                        var expected = Witness(kind, i);
                        for (int n = 0; n < Sector; n++) {
                            if (sector[n] != expected[n]) throw new IOException("Witness readback differs");
                        }
                        digest.TransformBlock(sector, 0, sector.Length, sector, 0);
                    }
                    digest.TransformFinalBlock(new byte[0], 0, 0);
                    return BitConverter.ToString(digest.Hash).Replace("-", "");
                }
            }
        }
    }
}
'@
    Add-Type -TypeDefinition $source -ErrorAction Stop

    function Get-WitnessDisk($space, [string]$expectedId = '') {
        $pool = Confirm-FixturePool
        $matches = @(Get-VirtualDisk -StoragePool $pool | Where-Object UniqueId -ceq $space.UniqueId)
        if ($matches.Count -ne 1 -or $matches[0].HealthStatus -ne 'Healthy' -or
            $matches[0].OperationalStatus -notcontains 'OK') { throw 'The witness space is no longer healthy and attached.' }
        $backing = @(Get-PhysicalDisk -VirtualDisk $matches[0])
        if ($backing.Count -ne 2 -or @($backing | Where-Object { ([string]$_.SerialNumber).Trim() -cin $serials }).Count -ne 2 -or
            @($backing.SerialNumber | Sort-Object -Unique).Count -ne 2) { throw 'Unexpected virtual disk members.' }
        $rows = @(Get-Disk -VirtualDisk $matches[0] -ErrorAction Stop)
        if ($rows.Count -ne 1) { throw 'Expected a uniquely associated disk.' }
        $disk = $rows[0]
        if ($disk.Number -eq 0 -or $disk.BusType -ne 'Spaces' -or $disk.Size -ne 1GB -or
            $disk.LogicalSectorSize -ne 4096 -or $disk.PhysicalSectorSize -ne 4096 -or
            $disk.IsBoot -ne $false -or $disk.IsSystem -ne $false -or $disk.IsClustered -ne $false -or
            $disk.PartitionStyle -ne 'RAW' -or $disk.NumberOfPartitions -ne 0 -or
            $null -eq $disk.IsOffline -or $null -eq $disk.IsReadOnly -or [string]::IsNullOrWhiteSpace($disk.UniqueId) -or
            ($expectedId -and $disk.UniqueId -cne $expectedId)) { throw 'Unexpected protected or changed witness disk.' }
        $disk
    }

    if ($thinDisks.Count -eq 0) {
        if ($InspectWitnessHandles) { throw 'The diagnostic requires two already attached spaces.' }
        $thin[0] | Connect-VirtualDisk -ErrorAction Stop | Out-Null
    }
    $targets = @($mirror[0], $thin[0])
    $initialDisks = @($targets | ForEach-Object { Get-WitnessDisk $_ })
    if ($initialDisks[0].Number -eq $initialDisks[1].Number -or $initialDisks[0].UniqueId -ceq $initialDisks[1].UniqueId) {
        throw 'The fixture spaces must map to distinct devices.'
    }
    if ($InspectWitnessHandles) {
        foreach ($disk in $initialDisks) { [DiskStudioSpacesWitness]::Inspect($disk.Number) }
        return
    }
    for ($i = 0; $i -lt 2; $i++) {
        $disk = Get-WitnessDisk $targets[$i] $initialDisks[$i].UniqueId
        if ($disk.IsOffline) { $disk | Set-Disk -IsOffline $false -ErrorAction Stop }
        $disk = Get-WitnessDisk $targets[$i] $initialDisks[$i].UniqueId
        if ($disk.IsReadOnly) { $disk | Set-Disk -IsReadOnly $false -ErrorAction Stop }
        $disk = Get-WitnessDisk $targets[$i] $initialDisks[$i].UniqueId
        if ($disk.IsOffline -ne $false -or $disk.IsReadOnly -ne $false) { throw 'Witness disk is not writable.' }
        $digest = [DiskStudioSpacesWitness]::Seed($disk.Number, $i + 1)
        $disk = Get-WitnessDisk $targets[$i] $initialDisks[$i].UniqueId
        $disk | Set-Disk -IsOffline $true -ErrorAction Stop
        if ((Get-WitnessDisk $targets[$i] $initialDisks[$i].UniqueId).IsOffline -ne $true) {
            throw 'Witness disk did not go offline.'
        }
        Write-Output "DS_SPACES_WITNESS $($i + 1) $digest"
    }
    $thin[0] | Disconnect-VirtualDisk -ErrorAction Stop | Out-Null
    if (@(Get-Disk -VirtualDisk $thin[0] -ErrorAction SilentlyContinue).Count -ne 0) {
        throw 'The thin witness disk is still associated.'
    }
    'VERIFIED: marker sectors on both logical spaces were written once and read back.'
    return
}
'VERIFIED: dedicated A/B membership, one attached RAW mirror and one detached thin space.'
$confirmed | Format-Table FriendlyName,HealthStatus,Size,AllocatedSize
$spaces | Format-Table FriendlyName,Size,ProvisioningType,ResiliencySettingName,NumberOfColumns,NumberOfDataCopies,Interleave
$attached | Format-Table Number,BusType,PartitionStyle,NumberOfPartitions
