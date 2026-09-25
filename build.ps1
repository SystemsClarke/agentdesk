<#
.SYNOPSIS
  Build, test and package AgentDesk.
.EXAMPLE
  ./build.ps1            # build (Debug)
  ./build.ps1 -Test      # build and run the tests
  ./build.ps1 -Package   # publish Native AOT, pack a signed Setup.exe and update feed into releases\
#>
param([switch]$Test, [switch]$Package)
$ErrorActionPreference = 'Stop'
Set-Location $PSScriptRoot

# Native AOT links with MSVC and finds it through vswhere, which the VS installer doesn't put on PATH.
$vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer"
if ((Test-Path $vswhere) -and ($env:PATH -notlike "*$vswhere*")) { $env:PATH = "$vswhere;$env:PATH" }

dotnet build -v q --nologo
if ($LASTEXITCODE) { exit $LASTEXITCODE }
if ($Test) { dotnet test --no-build -v q --nologo; if ($LASTEXITCODE) { exit $LASTEXITCODE } }

if ($Package) {
    $stage = Join-Path $PSScriptRoot 'obj\package'
    Remove-Item $stage -Recurse -ErrorAction Ignore
    # The window (AgentDesk.App, WPF) publishes framework-dependent (the default with -r): ~0.4 MB against ~140 MB
    # self-contained, and --framework below has Setup install the .NET desktop runtime when it is missing.
    # The core stays Native AOT and Velopack's main exe.
    $version = "0.1.$(git rev-list --count HEAD)" # every commit is a new version; stamped into the exes and the package
    foreach ($p in 'AgentDesk.Core', 'AgentDesk.Cli', 'AgentDesk.App') {
        dotnet publish "src/$p" -c Release -r win-x64 -o $stage -v q --nologo "-p:Version=$version"
        if ($LASTEXITCODE) { exit $LASTEXITCODE }
    }

    # The package id is not "AgentDesk": Velopack installs to, and uninstall deletes, %LOCALAPPDATA%\<id>,
    # and %LOCALAPPDATA%\AgentDesk holds the board. Every commit is a new version, so updates always move forward.
    # Signed with the AgentDesk dev cert when it is in this machine's cert store (John's), unsigned otherwise (CI's public releases).
    $thumb = 'AE6661701A6D11E50423B66216D26410489953BD' # "AgentDesk (self-signed, palencharj)"; public, identifies the cert only
    $sign = if (Test-Path Cert:\CurrentUser\My\$thumb) { @('--signParams', "/sha1 $thumb /fd SHA256 /tr http://timestamp.digicert.com /td SHA256") } else { @() }
    dotnet tool restore | Out-Null
    dotnet vpk pack --packId AgentDeskApp --packTitle AgentDesk --packAuthors 'John Palenchar' `
        --packVersion $version --packDir $stage --mainExe AgentDesk.Core.exe `
        --runtime win-x64 --framework net10.0-x64-desktop --icon agentdesk\assets\agentdesk.ico `
        --shortcuts StartMenuRoot --outputDir releases @sign
    if ($LASTEXITCODE) { exit $LASTEXITCODE }
    Get-AuthenticodeSignature releases\*Setup.exe | Format-Table Status, @{ n = 'Signer'; e = { $_.SignerCertificate.Subject } }, Path -AutoSize
}
