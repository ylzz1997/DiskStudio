# Extract the amd64 WinPE 26100.1 image and Storage WMI prerequisites.
# Source: the Windows PE add-on for ADK 10.1.26100.9457.
# https://learn.microsoft.com/en-us/windows-hardware/get-started/adk-install
$ErrorActionPreference = 'Stop'
$project = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..\..'))
$vm = Join-Path $project '.vm'
$cache = Join-Path $vm 'winpe-adk-input'
$output = Join-Path $cache 'amd64'
foreach ($directory in @($vm, $cache, $output)) {
    if (!(Test-Path -LiteralPath $directory)) {
        New-Item -ItemType Directory -Path $directory | Out-Null
    }
    $item = Get-Item -LiteralPath $directory
    if (!$item.PSIsContainer -or ($item.Attributes -band [IO.FileAttributes]::ReparsePoint)) {
        throw "Expected an ordinary VM directory: $directory"
    }
}

# File names and cabinet entries come from the read-only MSI tables of the
# Microsoft-signed add-on. Pin the outer and extracted file digests.
$base = 'https://download.microsoft.com/download/a4a79e7a-f085-41c4-aebf-2538fd000790/adkwinpeaddons/Installers/'
$cabinets = @{
    '3ffff92a2c7e8b2f1a6e85afeaa027ca.cab' = 'c357fd789c2c6d74a403ee37ee4959bb8c3f1f78ef320464741aa898eb6c0e87'
    '9722214af0ab8aa9dffb6cfdafd937b7.cab' = '4c613a18788a9c860ec7cbe6a58e1a842731b02ecc99189568f68c036cb5a4e1'
    'a6296c5a6eec9ca02b431ed0461b952d.cab' = 'ab8b67280b8183165f659ffbeb2ddf729b1a2e8666953ad808abfc51d49b7529'
}
$members = @'
name,entry,bytes,cab,sha256
winpe.wim,filad61cf00c94775125d186add4ac43371,340134390,a6296c5a6eec9ca02b431ed0461b952d.cab,fbcbdb1c6651ab3a69384e9d4f95f2c02321318603849453b252e21e827c8197
WinPE-WMI.cab,filb8eaee8243f0d40642def467974eb651,6492661,9722214af0ab8aa9dffb6cfdafd937b7.cab,d3cafe5afb7a847889bc6770f11aa3b9c6046b4c46249f50a892b795d4e73401
WinPE-WMI_en-us.cab,filbb678484bf755f7c705cd3d64de8650a,349227,3ffff92a2c7e8b2f1a6e85afeaa027ca.cab,c7d21cb57b57b3db78cbea7b39b334cdbcb9cf591b5273c4ad0bb16496b38f92
WinPE-NetFx.cab,filacfdff6b0ca55226d12bdcb8583fe7f7,95327346,9722214af0ab8aa9dffb6cfdafd937b7.cab,ce42bae126df62012abe3c48e8dcdc299c675d25c9b59885bf25d20f59f6a464
WinPE-NetFx_en-us.cab,file9a936e5ede3992f52f1a5bab157c350,128100,3ffff92a2c7e8b2f1a6e85afeaa027ca.cab,c2d8b09f97e868eaa2c8725f7b57635a03eceb353401f6638310d868b121c18d
WinPE-Scripting.cab,fil38098938ce091840eaa341b03e695fe0,17117978,9722214af0ab8aa9dffb6cfdafd937b7.cab,0466e98cfcb76720ce65eda089c0d290f6c925406a01ffff03a2542ff04b5cdd
WinPE-Scripting_en-us.cab,fil53ba6e3aae1caf7ad59f796e5629a415,32799,3ffff92a2c7e8b2f1a6e85afeaa027ca.cab,9fe4f769369cee4d23a9434c5d99e7952595cbb9678fcb59208ac8192a2b8836
WinPE-PowerShell.cab,filc2d1b5aa46ec426c1a1d58d2441ec593,3363546,9722214af0ab8aa9dffb6cfdafd937b7.cab,76e6612389c15fb046cfd17020590da51decdeb94c8d7c584d35f93bc7e20bb7
WinPE-PowerShell_en-us.cab,fild000a71ea3deb1b3cfd0d5f8d30bd549,180230,3ffff92a2c7e8b2f1a6e85afeaa027ca.cab,64da49a0bb63825c62ef62d99a16d62fdef66697cfab27c70c55c10224de31a9
WinPE-StorageWMI.cab,fil3c9a79652b54e08001530bb3ab236f1b,948238,9722214af0ab8aa9dffb6cfdafd937b7.cab,6517f5f546fa3d70f43bc578ef667e37209c42fa555fcfaa112ad001e7b4455b
WinPE-StorageWMI_en-us.cab,fild582127f65b38627635816c60c6e14b0,123875,3ffff92a2c7e8b2f1a6e85afeaa027ca.cab,5f8211095471fe5b076d6e1b0645dfd2318a1a6baa143738cd67cecc0989062f
'@ | ConvertFrom-Csv

function Confirm-Input([string]$Path, [string]$Sha256, [long]$Bytes = -1) {
    $file = Get-Item -LiteralPath $Path
    if ($file.PSIsContainer -or ($file.Attributes -band [IO.FileAttributes]::ReparsePoint) -or
        ($Bytes -ge 0 -and $file.Length -ne $Bytes) -or
        (Get-FileHash -LiteralPath $file.FullName -Algorithm SHA256).Hash -ne $Sha256) {
        throw "WinPE input differs from the pinned Microsoft payload: $Path"
    }
}

$missing = foreach ($member in $members) {
    $target = Join-Path $output $member.name
    if (Test-Path -LiteralPath $target) {
        Confirm-Input $target $member.sha256 ([long]$member.bytes)
    } else {
        $member
    }
}
$extractor = Join-Path $vm 'tools\7zip\7z.exe'
if ($missing -and !(Test-Path -LiteralPath $extractor -PathType Leaf)) {
    throw "The VM input extractor is missing: $extractor"
}
foreach ($group in ($missing | Group-Object cab)) {
    $cab = Join-Path $cache $group.Name
    $hash = $cabinets[$group.Name]
    if (!(Test-Path -LiteralPath $cab)) {
        $partial = "$cab.partial"
        if (Test-Path -LiteralPath $partial) {
            $file = Get-Item -LiteralPath $partial
            if ($file.PSIsContainer -or ($file.Attributes -band [IO.FileAttributes]::ReparsePoint)) {
                throw "Unexpected partial download: $partial"
            }
        }
        & curl.exe --fail --show-error --silent --retry 2 --continue-at - --output $partial ($base + $group.Name)
        if ($LASTEXITCODE -ne 0) { throw "WinPE cabinet download failed: $($group.Name)" }
        Confirm-Input $partial $hash
        Move-Item -LiteralPath $partial -Destination $cab
    } else {
        Confirm-Input $cab $hash
    }
    $stage = Join-Path $output ("extract-" + [guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory -Path $stage | Out-Null
    $entries = @($group.Group | ForEach-Object { $_.entry })
    & $extractor e -y "-o$stage" $cab @entries | Out-Null
    if ($LASTEXITCODE -ne 0) { throw "WinPE cabinet extraction failed: $($group.Name)" }
    foreach ($member in $group.Group) {
        $file = Join-Path $stage $member.entry
        Confirm-Input $file $member.sha256 ([long]$member.bytes)
        Move-Item -LiteralPath $file -Destination (Join-Path $output $member.name)
    }
    [IO.Directory]::Delete($stage)
}

$members | Sort-Object name | Select-Object name,@{Name='bytes';Expression={[long]$_.bytes}},sha256 |
    ConvertTo-Json | Set-Content -LiteralPath (Join-Path $output 'inputs.json') -Encoding utf8
Write-Output $output
