param([ValidateSet('probe','populate','verify','ext-populate','ext-verify')][string]$Phase = 'probe')
$ErrorActionPreference = 'Stop'
$diskStudioRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '../../.vm/ubuntu'))
$diskStudioStage = Join-Path $diskStudioRoot $(if ($Phase -eq 'probe') {'seed'} else {"seed-$Phase"})
$diskStudioOutput = Join-Path $diskStudioRoot $(if ($Phase -eq 'probe') {'seed.iso'} else {"seed-$Phase.iso"})
foreach ($diskStudioPath in @($diskStudioRoot, $diskStudioStage)) {
    if ((Get-Item -LiteralPath $diskStudioPath).Attributes -band [IO.FileAttributes]::ReparsePoint) { throw 'Seed directories cannot be links' }
}
if (Test-Path -LiteralPath $diskStudioOutput) { throw 'Existing immutable seed ISO must be preserved' }
Add-Type -TypeDefinition @'
using System;
using System.IO;
using System.Runtime.InteropServices;
using System.Runtime.InteropServices.ComTypes;
public static class DiskStudioSeedWriter {
  public static void Save(object source, string filename) {
    var stream = (IStream)source;
    var count = Marshal.AllocHGlobal(4);
    try {
      using (var output = new FileStream(filename, FileMode.CreateNew, FileAccess.Write)) {
        var data = new byte[1024 * 1024];
        for (;;) {
          stream.Read(data, data.Length, count);
          int n = Marshal.ReadInt32(count);
          if (n == 0) break;
          output.Write(data, 0, n);
        }
      }
    } finally { Marshal.FreeHGlobal(count); }
  }
}
'@
$diskStudioImage = New-Object -ComObject IMAPI2FS.MsftFileSystemImage
try {
    $diskStudioImage.FileSystemsToCreate = 3
    $diskStudioImage.VolumeName = 'CIDATA'
    $diskStudioImage.Root.AddTree($diskStudioStage, $false)
    [DiskStudioSeedWriter]::Save($diskStudioImage.CreateResultImage().ImageStream, $diskStudioOutput)
} finally { [Runtime.InteropServices.Marshal]::ReleaseComObject($diskStudioImage) | Out-Null }
