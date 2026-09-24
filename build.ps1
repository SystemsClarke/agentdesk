<#
.SYNOPSIS
  Build, test and install AgentDesk.
.EXAMPLE
  ./build.ps1            # build (Debug)
  ./build.ps1 -Test      # build and run the tests
  ./build.ps1 -Install   # publish Native AOT, sign with the local dev cert, install to %LOCALAPPDATA%\AgentDesk\bin
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
    $stage = Join-Path $PSScriptRoot 'obj\install'
    foreach ($p in 'AgentDesk.Core', 'AgentDesk.Cli') {
        dotnet publish "src/$p" -c Release -r win-x64 -o $stage -v q --nologo
        if ($LASTEXITCODE) { exit $LASTEXITCODE }
    }

    # Sign with the local development cert (build/agentdesk-cert-thumbprint.txt), timestamped.
    $thumb = (Get-Content build\agentdesk-cert-thumbprint.txt -Raw).Trim()
    $signtool = Get-ChildItem "${env:ProgramFiles(x86)}\Windows Kits\10\bin\*\x64\signtool.exe" | Sort-Object FullName | Select-Object -Last 1
    & $signtool sign /sha1 $thumb /fd SHA256 /tr http://timestamp.digicert.com /td SHA256 /q (Get-ChildItem $stage -Filter *.exe).FullName
    if ($LASTEXITCODE) { exit $LASTEXITCODE }

    # The core is running whenever any agent session is; stop it just before the copy. The next hook restarts it.
    $bin = Join-Path $env:LOCALAPPDATA 'AgentDesk\bin'
    New-Item -ItemType Directory $bin -Force | Out-Null
    Get-Process AgentDesk.Core -ErrorAction SilentlyContinue | Where-Object Path -like "$bin\*" | Stop-Process -Force
    Copy-Item "$stage\*" $bin -Force
    Get-ChildItem $bin -Filter *.exe | ForEach-Object {
        '{0,-24} {1,6:N1} MB  {2}' -f $_.Name, ($_.Length / 1MB), (Get-AuthenticodeSignature $_.FullName).SignerCertificate.Subject
    }
}
