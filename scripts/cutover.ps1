# Moves this Windows machine from the .NET AgentDesk to the Rust one, in one go:
#   irm https://raw.githubusercontent.com/SystemsClarke/agentdesk/main/scripts/cutover.ps1 | iex
# Set $env:AGENTDESK_CUTOVER_DRY=1 first to see what it would do without doing it.
#
# What it does, in order (nothing is deleted: the .NET app stays installed, so going back is one command, see the end of this comment):
#   1. installs the newest Rust release into %LOCALAPPDATA%\Programs\agentdesk
#   2. quits the .NET app and core, and stops it starting at logon (its Run entry is saved for a rollback)
#   3. `agentdesk migrate --apply`: backs up the board, points Claude Code's MCP server and hooks at the new binary
#   4. points the Slack bridge / vault search at a Python that has slack_bolt, numpy and Pillow
#   5. points the AgentDesk-Backup task at `agentdesk backup`
#   6. puts the new folder first on your PATH (and the old one off it), so `agentdesk` is the Rust one
#   7. starts the daemon now and at every logon (hidden), then prints the state
# Afterwards: restart your Claude Code sessions so they reconnect to the new MCP server, and `agentdesk tui` is the window.
#
# Going back: run   $env:AGENTDESK_ROLLBACK=1; irm <the same URL> | iex

$ErrorActionPreference = 'Stop'
$log = Join-Path $env:LOCALAPPDATA 'AgentDesk\cutover.log'
New-Item -ItemType Directory -Force (Split-Path $log) | Out-Null
try { Start-Transcript -Path $log -Force | Out-Null } catch { }   # what happened, if it goes wrong: %LOCALAPPDATA%\AgentDesk\cutover.log
$dry = [bool]$env:AGENTDESK_CUTOVER_DRY
$repo = 'SystemsClarke/agentdesk'
$data = Join-Path $env:LOCALAPPDATA 'AgentDesk'
$dest = Join-Path $env:LOCALAPPDATA 'Programs\agentdesk'
$oldApp = Join-Path $env:LOCALAPPDATA 'AgentDeskApp'
$exe = Join-Path $dest 'agentdesk.exe'
$runKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run'
$saved = Join-Path $data 'cutover-rollback.json'

function Say($m) { Write-Host "==> $m" -ForegroundColor Cyan }
function Act($what, [scriptblock]$block) {
    if ($dry) { Write-Host "    (dry run) would: $what" } else { Write-Host "    $what"; & $block }
}

if ($env:AGENTDESK_ROLLBACK) {
    Say 'rolling back to the .NET AgentDesk'
    if (-not (Test-Path $saved)) { throw "nothing to roll back: $saved is missing" }
    $s = Get-Content $saved -Raw | ConvertFrom-Json
    Get-Process agentdesk -ErrorAction SilentlyContinue | Where-Object { $_.Path -like "$dest*" } | Stop-Process -Force
    Remove-ItemProperty $runKey -Name AgentDeskRust -ErrorAction SilentlyContinue
    if ($s.run) { Set-ItemProperty $runKey -Name AgentDesk -Value $s.run }
    if ($s.path) { [Environment]::SetEnvironmentVariable('Path', $s.path, 'User') }
    if ($s.backupExecute) {
        $a = New-ScheduledTaskAction -Execute $s.backupExecute -Argument $s.backupArguments -WorkingDirectory $s.backupDir
        Set-ScheduledTask -TaskName AgentDesk-Backup -Action $a | Out-Null
    }
    Write-Host 'Rolled back the autostart, PATH and backup task. Put Claude Code back on the old binary with the *.agentdesk-bak files next to ~\.claude.json and ~\.claude\settings.json, then start the old app:'
    Write-Host "    `"$oldApp\current\AgentDesk.App.exe`""
    return
}

# ---- 1. install
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

# ---- 2. quit the .NET app
Say '2. quitting the .NET app and core'
$run = (Get-ItemProperty $runKey -ErrorAction SilentlyContinue).AgentDesk
$userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
$task = Get-ScheduledTask -TaskName AgentDesk-Backup -ErrorAction SilentlyContinue
$act = if ($task) { $task.Actions | Select-Object -First 1 } else { $null }
Act "save how to go back to $saved" {
    New-Item -ItemType Directory -Force $data | Out-Null
    @{ run = $run; path = $userPath; backupExecute = $act.Execute; backupArguments = $act.Arguments; backupDir = $act.WorkingDirectory } | ConvertTo-Json | Set-Content $saved -Encoding UTF8
}
Act 'stop AgentDesk.App first, then AgentDesk.Core (the window relaunches the core if it is left running); their hosted agent sessions end, and the daemon resumes the ones the board says were running' {
    Get-Process AgentDesk.App -ErrorAction SilentlyContinue | Stop-Process -Force
    Start-Sleep -Seconds 2
    Get-Process AgentDesk.Core -ErrorAction SilentlyContinue | Stop-Process -Force
    Start-Sleep -Seconds 20   # if something relaunches them, stop them again
    Get-Process AgentDesk.App, AgentDesk.Core -ErrorAction SilentlyContinue | Stop-Process -Force
    if (Get-Process AgentDesk.App, AgentDesk.Core -ErrorAction SilentlyContinue) { throw 'the .NET app or core is still running: quit it from its tray icon and run this again' }
}
Act 'remove the .NET core from logon' { Remove-ItemProperty $runKey -Name AgentDesk -ErrorAction SilentlyContinue }

# ---- 3. migrate
Say '3. migrating the board and Claude Code'
if ($dry) { Write-Host '    (dry run) would: agentdesk migrate --apply   (run `agentdesk migrate` after installing for its full dry run)' } else { & $exe migrate --apply | ForEach-Object { Write-Host "    $_" } }

# ---- 4. python for the Slack bridge, vault search and dictation
Say '4. Python for the Slack bridge'
$py = $null
if ($act -and $act.Execute -like '*python*.exe') {   # the old backup task's own venv has slack_bolt and numpy already
    $cand = Join-Path (Split-Path $act.Execute) 'python.exe'
    if (Test-Path $cand) { $py = $cand }
}
if (-not $py) {
    $venv = Join-Path $data 'venv'
    $py = Join-Path $venv 'Scripts\python.exe'
    if (-not (Test-Path $py)) {
        Act "create $venv and install slack_bolt, numpy and Pillow" {
            python -m venv $venv
            & $py -m pip install --quiet 'slack_bolt>=1.30.0' 'numpy>=1.26' 'Pillow>=12.3.0'
        }
    }
}
Act "use $py for the Python half (AGENTDESK_PYTHON_BIN)" { [Environment]::SetEnvironmentVariable('AGENTDESK_PYTHON_BIN', $py, 'User'); $env:AGENTDESK_PYTHON_BIN = $py }

# ---- 5. the backup task
Say '5. the hourly backup'
if ($task) {
    Act "point AgentDesk-Backup at `"$exe`" backup" {
        $a = New-ScheduledTaskAction -Execute $exe -Argument 'backup'
        Set-ScheduledTask -TaskName AgentDesk-Backup -Action $a | Out-Null
    }
} else { Write-Host '    no AgentDesk-Backup task: nothing to change (run `agentdesk backup` on a schedule yourself)' }

# ---- 6. PATH
Say '6. PATH'
Act "put $dest first on the user PATH, drop $oldApp\current" {
    $parts = @($dest) + ($userPath -split ';' | Where-Object { $_ -and $_ -ne $dest -and $_ -notlike "$oldApp*" })
    [Environment]::SetEnvironmentVariable('Path', ($parts -join ';'), 'User')
}

# ---- 7. the daemon
Say '7. starting the daemon, now and at every logon'
$cmd = "conhost.exe --headless `"$exe`" daemon"
Act 'register the daemon in the logon Run key' { Set-ItemProperty $runKey -Name AgentDeskRust -Value $cmd }
Act 'start it now, detached from this shell (a one-shot scheduled task)' {
    if (-not (Test-Path $exe)) { throw "$exe vanished before the daemon could start (see $log)" }
    $a = New-ScheduledTaskAction -Execute 'conhost.exe' -Argument "--headless `"$exe`" daemon"
    $t = New-ScheduledTaskTrigger -Once -At (Get-Date).AddYears(10)
    Register-ScheduledTask -TaskName AgentDesk-Daemon -Action $a -Trigger $t -Force | Out-Null
    Start-ScheduledTask -TaskName AgentDesk-Daemon
    Start-Sleep -Seconds 8
    if (-not (Get-Process agentdesk -ErrorAction SilentlyContinue | Where-Object { $_.Path -eq $exe })) { throw "the daemon is not running (see $log)" }
}
if (-not $dry) {
    Say 'state'
    & $exe ui status | Out-String | ConvertFrom-Json | ForEach-Object { Write-Host "    slack bridge: $($_.bridge.state)   concierge: $(if ($_.concierge.on) {'on'} else {'off'})   sessions: $($_.sessions.summary)" }
}
if (-not $dry -and -not (Test-Path $exe)) { throw "$exe is gone again at the end of the run" }
try { Stop-Transcript | Out-Null } catch { }
Write-Host ''
Write-Host 'Done. Restart your Claude Code sessions so they reconnect to the new MCP server. `agentdesk tui` is the window.' -ForegroundColor Green
if ($dry) { Write-Host 'That was a dry run: nothing was changed.' }
