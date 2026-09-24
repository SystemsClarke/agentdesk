<#
.SYNOPSIS
  Build, test and install AgentDesk.
.EXAMPLE
  ./build.ps1            # build (Debug)
  ./build.ps1 -Test      # build and run the tests
  ./build.ps1 -Install   # publish Native AOT exes to %LOCALAPPDATA%\AgentDesk\bin and report their size
#>
param([switch]$Test, [switch]$Install)
$ErrorActionPreference = 'Stop'
Set-Location $PSScriptRoot

# Native AOT links with MSVC and finds it through vswhere, which the VS installer doesn't put on PATH.
$vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer"
if ((Test-Path $vswhere) -and ($env:PATH -notlike "*$vswhere*")) { $env:PATH = "$vswhere;$env:PATH" }

dotnet build -v q --nologo
if ($LASTEXITCODE) { exit $LASTEXITCODE }
if ($Test) { dotnet test --no-build -v q --nologo; if ($LASTEXITCODE) { exit $LASTEXITCODE } }

if ($Install) {
    $bin = Join-Path $env:LOCALAPPDATA 'AgentDesk\bin'
    foreach ($p in 'AgentDesk.Core', 'AgentDesk.Mcp') {
        dotnet publish "src/$p" -c Release -r win-x64 -o $bin -v q --nologo
        if ($LASTEXITCODE) { exit $LASTEXITCODE }
    }
    Get-ChildItem $bin -Filter *.exe | ForEach-Object { '{0,-24} {1,6:N1} MB' -f $_.Name, ($_.Length / 1MB) }
}
