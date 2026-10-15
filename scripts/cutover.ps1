# Moves this Windows machine from the .NET AgentDesk to the Rust one, in one go:
#   irm https://raw.githubusercontent.com/SystemsClarke/agentdesk/main/scripts/cutover.ps1 | iex
# Set $env:AGENTDESK_CUTOVER_DRY=1 first to see what it would do without doing it.
#
# This script only downloads the newest release into %LOCALAPPDATA%\Programs\agentdesk; everything else is
# `agentdesk install --from-dotnet`, in the binary (src/install_dotnet.rs), which in this order:
#   1. saves how to go back (%LOCALAPPDATA%\AgentDesk\cutover-rollback.json)
#   2. quits the .NET window, then the .NET core, waits, and quits them again if they came back (it stops if they stay)
#   3. takes the .NET core off logon, and runs the equivalent of `agentdesk migrate --apply`
#   4. points the AgentDesk-Backup task at `agentdesk backup`, the install folder first on your PATH, and AGENTDESK_PYTHON_BIN at the old venv
#   5. registers the core and the tray to start at logon, and starts the core now
# Nothing of the .NET install is deleted. Afterwards: restart your Claude Code sessions so they reconnect to the new MCP server.
#
# Going back: run   $env:AGENTDESK_ROLLBACK=1; irm <the same URL> | iex     (that is `agentdesk install --rollback`)

$ErrorActionPreference = 'Stop'
$log = Join-Path $env:LOCALAPPDATA 'AgentDesk\cutover.log'
New-Item -ItemType Directory -Force (Split-Path $log) | Out-Null
try { Start-Transcript -Path $log -Force | Out-Null } catch { }   # what happened, if it goes wrong: %LOCALAPPDATA%\AgentDesk\cutover.log
$dry = [bool]$env:AGENTDESK_CUTOVER_DRY
$repo = 'SystemsClarke/agentdesk'
$dest = Join-Path $env:LOCALAPPDATA 'Programs\agentdesk'
$exe = Join-Path $dest 'agentdesk.exe'

function Say($m) { Write-Host "==> $m" -ForegroundColor Cyan }
function Act($what, [scriptblock]$block) {
    if ($dry) { Write-Host "    (dry run) would: $what" } else { Write-Host "    $what"; & $block }
}

if ($env:AGENTDESK_ROLLBACK) {
    Say 'rolling back to the .NET AgentDesk'
    if (-not (Test-Path $exe)) { throw "$exe is missing: nothing to roll back with" }
    $a = @('install', '--rollback'); if ($dry) { $a += '--dry-run' }
    & $exe @a
    if ($LASTEXITCODE) { throw "agentdesk install --rollback failed (see $log)" }
    try { Stop-Transcript | Out-Null } catch { }
    return
}

# ---- 1. download the newest release
Say '1. installing the newest Rust release'
$arch = if ($env:PROCESSOR_ARCHITECTURE -eq 'ARM64') { 'aarch64' } else { 'x86_64' }
if ($arch -ne 'x86_64') { throw 'only x86_64 Windows builds are published' }
$releases = Invoke-RestMethod "https://api.github.com/repos/$repo/releases?per_page=30"
$asset = $releases.assets | Where-Object { $_.name -eq 'agentdesk-x86_64-pc-windows-msvc.zip' } | Select-Object -First 1
if (-not $asset) { throw 'no Rust release with a Windows build yet (see https://github.com/SystemsClarke/agentdesk/releases)' }
Act "download $($asset.browser_download_url) into $dest" {
    New-Item -ItemType Directory -Force $dest | Out-Null
    $zip = Join-Path $env:TEMP 'agentdesk-rust.zip'
    Invoke-WebRequest $asset.browser_download_url -OutFile $zip
    if (Test-Path $exe) { Move-Item $exe "$exe.old" -Force }   # a running copy cannot be overwritten, but it can be renamed
    Expand-Archive $zip -DestinationPath $dest -Force
    Remove-Item $zip
    Remove-Item "$exe.old" -ErrorAction SilentlyContinue
}
if (-not $dry) {
    if (-not (Test-Path $exe)) { throw "$exe is not there after installing" }
    Write-Host "    installed $(& $exe --version)"
}

# ---- 2. everything else is the binary's
Say '2. agentdesk install --from-dotnet'
if ($dry) {
    # The binary on disk may be an older one without `install`: a dry run does not run it.
    Write-Host '    (dry run) would: agentdesk install --from-dotnet   (after installing, `agentdesk install --from-dotnet --dry-run` lists its steps)'
} else {
    & $exe install --from-dotnet
    if ($LASTEXITCODE) { throw "agentdesk install --from-dotnet failed (see $log)" }
}
if (-not $dry -and -not (Test-Path $exe)) { throw "$exe is gone again at the end of the run" }
try { Stop-Transcript | Out-Null } catch { }
if ($dry) { Write-Host 'That was a dry run: nothing was changed.' }
