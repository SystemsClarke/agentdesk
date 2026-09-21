# Open the AgentDesk window at logon, so the board is on screen without anyone
# having to remember to launch it.
#
#   powershell -NoProfile -File scripts\Install-AgentDeskStartup.ps1
#   powershell -NoProfile -File scripts\Install-AgentDeskStartup.ps1 -Uninstall
#
# A SEPARATE task from AgentDesk-Backup (Install-AgentDeskTask.ps1), and
# deliberately so. That one is a 30-minute batch run; this one is a window meant
# to stay open for the whole session, and the two want opposite settings for
# ExecutionTimeLimit. One script per task keeps each readable, and keeps a
# change to one from silently changing the other.
#
# WHY A SCHEDULED TASK AND NOT A STARTUP-FOLDER SHORTCUT. A shortcut in the
# Startup folder is a file in a folder nobody opens: nothing announces that it
# is there, nothing lets you read what it will run, and removing it means
# knowing which .lnk to delete. The task is inspectable (Get-ScheduledTask), is
# idempotent to register (-Force), and is undone by one command. The repo
# already manages its background work this way, so a second mechanism would be
# the odd one out.
#
# WHY InteractiveToken AND NOT A STORED PASSWORD. Same reason as the backup
# task: the database lives in %LOCALAPPDATA% and this is a GUI that needs a
# desktop to draw on. A machine with nobody logged on is not a machine where a
# notification app is useful, so there is nothing bought by storing a password.
#
# WHY ExecutionTimeLimit IS PT0S ("no limit"). This is the setting that must NOT
# be copied from the backup task. PT30M is right for a batch run; on this task
# it would kill the window half an hour after logon, which from the user's side
# is indistinguishable from the app crashing on its own.
#
# A second launch is safe. agentdesk/app.py holds a SingleInstance guard and
# exits 0 when one is already running, so MultipleInstancesPolicy=IgnoreNew is
# belt-and-braces rather than the only thing preventing two servers on one
# SQLite file.

[CmdletBinding()]
param(
    [string] $TaskName = 'AgentDesk-Startup',
    [string] $RepoRoot,
    [string] $Pythonw,
    [int]    $DelaySeconds = 20,
    [switch] $Uninstall,
    # For a packaged (Nuitka) install: the compiled AgentDesk.exe IS the
    # interpreter and needs no `-m agentdesk.app` argument. When given, this
    # replaces $Pythonw/$RepoRoot/args entirely rather than layering on top
    # of the dev-mode defaults, so a packaged install never accidentally
    # inherits a venv path that will not exist on the target machine.
    [string] $Exe,
    [string] $ExeArgs = ""
)

$ErrorActionPreference = 'Stop'

# Resolved HERE, not in the param block: in Windows PowerShell 5.1 $PSScriptRoot
# is not populated while param defaults are evaluated. Same trap the backup
# installer documents.
if (-not $PSScriptRoot) {
    $scriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
} else {
    $scriptDir = $PSScriptRoot
}
$repoDefault = Split-Path -Parent $scriptDir
if (-not $Exe) {
    if (-not $RepoRoot) { $RepoRoot = $repoDefault }
    if (-not $Pythonw)  { $Pythonw  = Join-Path $repoDefault '.venv\Scripts\pythonw.exe' }
}

if ($Uninstall) {
    $existing = Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue
    if (-not $existing) { "Task '$TaskName' is not registered. Nothing to do."; exit 0 }
    Unregister-ScheduledTask -TaskName $TaskName -Confirm:$false
    "Unregistered '$TaskName'."
    "A window already open is not closed by this - close it from the tray."
    exit 0
}

if ($Exe) {
    # Packaged install: the exe IS the app, no interpreter/module to find.
    if (-not (Test-Path -LiteralPath $Exe)) {
        throw "AgentDesk.exe not found at '$Exe'. Pass the correct -Exe path."
    }
    $Command   = (Resolve-Path -LiteralPath $Exe).Path
    $Arguments = $ExeArgs
    $RepoRoot  = Split-Path -Parent $Command
} else {
    # --- refuse loudly rather than register a task that cannot run ---------
    # A task whose command does not exist fails silently at logon, and looks
    # exactly like a task that ran and declined to open a window.
    if (-not (Test-Path -LiteralPath $Pythonw)) {
        throw "pythonw.exe not found at '$Pythonw'. Pass -Pythonw, or create the venv first."
    }
    if (-not (Test-Path -LiteralPath (Join-Path $RepoRoot 'agentdesk\app.py'))) {
        throw "agentdesk\app.py not found under '$RepoRoot'. Pass -RepoRoot."
    }
    $Command   = (Resolve-Path -LiteralPath $Pythonw).Path
    $Arguments = "-m agentdesk.app"
}

# The delay gives the shell time to come up. Without it the window can start
# before the desktop is ready and land behind the taskbar, leaving no sign it is
# running.
$UserId = "$env:USERDOMAIN\$env:USERNAME"
$Delay  = "PT${DelaySeconds}S"

$xml = @"
<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.4" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>Opens the AgentDesk window at logon so the board is on screen without being remembered.</Description>
    <URI>\$TaskName</URI>
  </RegistrationInfo>
  <Triggers>
    <LogonTrigger>
      <Enabled>true</Enabled>
      <UserId>$UserId</UserId>
      <Delay>$Delay</Delay>
    </LogonTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">
      <UserId>$UserId</UserId>
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>LeastPrivilege</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>false</AllowHardTerminate>
    <StartWhenAvailable>true</StartWhenAvailable>
    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>
    <IdleSettings>
      <StopOnIdleEnd>false</StopOnIdleEnd>
      <RestartOnIdle>false</RestartOnIdle>
    </IdleSettings>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Enabled>true</Enabled>
    <Hidden>false</Hidden>
    <RunOnlyIfIdle>false</RunOnlyIfIdle>
    <WakeToRun>false</WakeToRun>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>7</Priority>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>$Command</Command>
      <Arguments>$Arguments</Arguments>
      <WorkingDirectory>$RepoRoot</WorkingDirectory>
    </Exec>
  </Actions>
</Task>
"@

Register-ScheduledTask -TaskName $TaskName -Xml $xml -Force | Out-Null

# -Force does not reliably flip a previously-DISABLED task back to enabled,
# even though the XML above says <Enabled>true</Enabled> -- measured: a task
# disabled by an earlier session stayed Disabled through a -Force
# re-registration and needed this explicit call. A reinstall/upgrade must not
# silently leave the app un-launchable at logon because of state left over
# from before this run.
Enable-ScheduledTask -TaskName $TaskName | Out-Null

$task = Get-ScheduledTask -TaskName $TaskName

"Registered '$TaskName'."
"  command     : $Command $Arguments"
"  working dir : $RepoRoot"
"  state       : $($task.State)"
"  runs as     : $($task.Principal.UserId) ($($task.Principal.LogonType))"
"  trigger     : at logon, delayed $Delay"
"  time limit  : unlimited (it is a window, not a batch job)"
""
"  start it now: Start-ScheduledTask -TaskName '$TaskName'"
"  undo it     : powershell -NoProfile -File scripts\Install-AgentDeskStartup.ps1 -Uninstall"
