$ErrorActionPreference = 'Stop'
$fixtureRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
if (!(Test-Path -LiteralPath (Join-Path $fixtureRoot 'src/vhdImage.ts'))) { throw 'Not in DiskStudio workspace' }
$env:DOTNET_CLI_HOME = Join-Path $fixtureRoot '.vm/tools/dotnet-home'
$env:DOTNET_CLI_TELEMETRY_OPTOUT = '1'
$env:DOTNET_GENERATE_ASPNET_CERTIFICATE = 'false'
$env:NUGET_HTTP_CACHE_PATH = Join-Path $fixtureRoot '.vm/tools/nuget-http-cache'
Push-Location $fixtureRoot
try {
    dotnet run --project scripts/vm/vhd-fixtures/VhdFixtures.csproj
    if ($LASTEXITCODE -ne 0) { throw 'VHD fixture generation failed' }
} finally { Pop-Location }
