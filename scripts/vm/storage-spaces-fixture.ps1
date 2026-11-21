param([switch]$Verify)

$ErrorActionPreference = 'Stop'
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

if (-not $Verify) {
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
    $thin[0].OperationalStatus -notcontains 'Detached') {
    throw 'Windows did not report the expected mirror and thin stripe geometry.'
}
$attached = @(Get-Disk -VirtualDisk $mirror[0] -ErrorAction Stop)
$detached = @(Get-Disk -VirtualDisk $thin[0] -ErrorAction SilentlyContinue)
if ($attached.Count -ne 1 -or $attached[0].BusType -ne 'Spaces' -or $attached[0].Size -ne 1GB -or
    $attached[0].PartitionStyle -ne 'RAW' -or $attached[0].NumberOfPartitions -ne 0 -or $detached.Count -ne 0) {
    throw 'Expected an attached uninitialized mirror and a detached thin space.'
}
'VERIFIED: dedicated A/B membership, one attached RAW mirror and one detached thin space.'
$confirmed | Format-Table FriendlyName,HealthStatus,Size,AllocatedSize
$spaces | Format-Table FriendlyName,Size,ProvisioningType,ResiliencySettingName,NumberOfColumns,NumberOfDataCopies,Interleave
$attached | Format-Table Number,BusType,PartitionStyle,NumberOfPartitions
