# Independent .NET writer fixtures. Only in-memory ZIP construction and ordinary
# project fixture files; never opens a device, user document or guest disk.
$ErrorActionPreference = 'Stop'
$zipFixtureRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '../tests/fixtures'))
Add-Type -AssemblyName System.IO.Compression
foreach ($compression in @('NoCompression','Optimal')) {
    $zipFixtureMemory = [IO.MemoryStream]::new()
    $zipFixtureArchive = [IO.Compression.ZipArchive]::new($zipFixtureMemory, [IO.Compression.ZipArchiveMode]::Create, $true)
    $zipFixtureNames = @('readme.txt','子目录/中文.txt','empty.bin')
    $zipFixtureValues = @('Independent .NET ZIP fixture', ('数据' * 10000), '')
    for ($zipFixtureIndex = 0; $zipFixtureIndex -lt $zipFixtureNames.Count; $zipFixtureIndex++) {
        $zipFixtureEntry = $zipFixtureArchive.CreateEntry($zipFixtureNames[$zipFixtureIndex], [IO.Compression.CompressionLevel]::$compression)
        $zipFixtureEntry.LastWriteTime = [DateTimeOffset]::new(2026, 1, 1, 0, 0, 0, [TimeSpan]::Zero)
        $zipFixtureStream = $zipFixtureEntry.Open()
        $zipFixtureBytes = [Text.Encoding]::UTF8.GetBytes($zipFixtureValues[$zipFixtureIndex])
        $zipFixtureStream.Write($zipFixtureBytes, 0, $zipFixtureBytes.Length)
        $zipFixtureStream.Dispose()
    }
    $zipFixtureArchive.Dispose()
    [IO.File]::WriteAllBytes((Join-Path $zipFixtureRoot "zip-dotnet-$compression.zip"), $zipFixtureMemory.ToArray())
    $zipFixtureMemory.Dispose()
}
