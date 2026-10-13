# Installs the newest agentdesk release into %LOCALAPPDATA%\Programs\agentdesk and puts that folder on the user PATH.
# After that, `agentdesk update --apply` keeps it current.
#   irm https://raw.githubusercontent.com/SystemsClarke/agentdesk/main/scripts/install.ps1 | iex
$ErrorActionPreference = 'Stop'
$repo = 'SystemsClarke/agentdesk'
$arch = if ($env:PROCESSOR_ARCHITECTURE -eq 'ARM64') { 'aarch64' } else { 'x86_64' }
if ($arch -ne 'x86_64') { throw 'only x86_64 Windows builds are published' }
$target = 'x86_64-pc-windows-msvc'
$dest = Join-Path $env:LOCALAPPDATA 'Programs\agentdesk'
$releases = Invoke-RestMethod "https://api.github.com/repos/$repo/releases?per_page=30"
$asset = $releases.assets | Where-Object { $_.name -eq "agentdesk-$target.zip" } | Select-Object -First 1
if (-not $asset) { throw "no release with a build for $target yet" }
New-Item -ItemType Directory -Force $dest | Out-Null
$zip = Join-Path $env:TEMP 'agentdesk.zip'
Invoke-WebRequest $asset.browser_download_url -OutFile $zip
# a running copy can't be overwritten, but it can be renamed out of the way
$exe = Join-Path $dest 'agentdesk.exe'
if (Test-Path $exe) { Move-Item $exe "$exe.old" -Force }
Expand-Archive $zip -DestinationPath $dest -Force
Remove-Item $zip
Remove-Item "$exe.old" -ErrorAction SilentlyContinue
$path = [Environment]::GetEnvironmentVariable('Path', 'User')
if (($path -split ';') -notcontains $dest) { [Environment]::SetEnvironmentVariable('Path', "$path;$dest", 'User'); Write-Host "added $dest to your PATH (open a new terminal)" }
Write-Host "installed $(& $exe --version) to $exe"
Write-Host 'coming from the .NET AgentDesk? run: agentdesk migrate     (then agentdesk migrate --apply)'
