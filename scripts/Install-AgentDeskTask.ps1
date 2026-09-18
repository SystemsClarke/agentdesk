# Register the hourly AgentDesk backup as a Scheduled Task.
#
#   powershell -NoProfile -File scripts\Install-AgentDeskTask.ps1
#   powershell -NoProfile -File scripts\Install-AgentDeskTask.ps1 -Uninstall
#
# WHY THE TASK IS BUILT FROM XML rather than from New-ScheduledTaskTrigger.
# "Repeat every hour, indefinitely" is a TimeTrigger whose Repetition has an
# Interval and NO Duration. The cmdlet's -RepetitionDuration takes a TimeSpan,
# and the value that means "indefinitely" is [TimeSpan]::MaxValue, which some
# builds of that cmdlet reject as out of range. The XML form has no such
# problem and is what the Task Scheduler actually stores, so it is also easier
# to read back and check.
#
# WHY InteractiveToken AND NOT A STORED PASSWORD. The database lives in
# %LOCALAPPDATA% and the vault path is under the user's own profile, so the
# task is only ever useful as this user with this user's access. A stored
# password would be a second copy of the credential on disk, to buy runs while
# nobody is logged on -- and a machine with nobody logged on is not a machine
# where a notification app matters. It runs at next logon instead: StartWhenAvailable
# catches up a missed hour rather than skipping it.

[CmdletBinding()]
param(
    [string] $TaskName = 'AgentDesk-Backup',
    [string] $RepoRoot = (Split-Path -Parent $PSScriptRoot),
    [string] $Pythonw  = (Join-Path (Split-Path -Parent $PSScriptRoot) '.venv\Scripts\pythonw.exe'),
    [switch] $Uninstall
)

$ErrorActionPreference = 'Stop'

if ($Uninstall) {
    $existing = Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue
    if (-not $existing) { "Task '$TaskName' is not registered. Nothing to do."; exit 0 }
    Unregister-ScheduledTask -TaskName $TaskName -Confirm:$false
    "Unregistered '$TaskName'."
    exit 0
}

# --- refuse loudly rather than register a task that cannot run --------------
# A scheduled task whose Command does not exist fails silently at 3am and looks
# exactly like a task that ran and found nothing to do.
if (-not (Test-Path -LiteralPath $Pythonw)) {
    throw "pythonw.exe not found at '$Pythonw'. Pass -Pythonw, or create the venv first."
}
if (-not (Test-Path -LiteralPath (Join-Path $RepoRoot 'agentdesk\backup.py'))) {
    throw "agentdesk\backup.py not found under '$RepoRoot'. Pass -RepoRoot."
}

$Pythonw = (Resolve-Path -LiteralPath $Pythonw).Path
$RepoRoot = (Resolve-Path -LiteralPath $RepoRoot).Path

# A boundary in the past is fine -- the scheduler computes the next occurrence
# from the interval -- but it must be a fixed literal, because -Xml has to be
# byte-identical run to run for the install to be idempotent.
$StartBoundary = '2026-01-01T00:05:00'

$xml = @"
<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.4" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>Snapshots the AgentDesk database and writes the day's messages into the memory vault. Every hour.</Description>
    <URI>\$TaskName</URI>
  </RegistrationInfo>
  <Triggers>
    <TimeTrigger>
      <Repetition>
        <Interval>PT1H</Interval>
        <StopAtDurationEnd>false</StopAtDurationEnd>
      </Repetition>
      <StartBoundary>$StartBoundary</StartBoundary>
      <Enabled>true</Enabled>
    </TimeTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">
      <UserId>$env:USERDOMAIN\$env:USERNAME</UserId>
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>LeastPrivilege</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>true</AllowHardTerminate>
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
    <ExecutionTimeLimit>PT30M</ExecutionTimeLimit>
    <Priority>7</Priority>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>$Pythonw</Command>
      <Arguments>-m agentdesk.backup</Arguments>
      <WorkingDirectory>$RepoRoot</WorkingDirectory>
    </Exec>
  </Actions>
</Task>
"@

Register-ScheduledTask -TaskName $TaskName -Xml $xml -Force | Out-Null

$task = Get-ScheduledTask -TaskName $TaskName
$info = Get-ScheduledTaskInfo -TaskName $TaskName

"Registered '$TaskName'."
"  command        : $Pythonw -m agentdesk.backup"
"  working dir    : $RepoRoot"
"  state          : $($task.State)"
"  runs as        : $($task.Principal.UserId) ($($task.Principal.LogonType))"
"  next run       : $($info.NextRunTime)"
""
"Run it once by hand to prove it works without waiting for the hour:"
"  Start-ScheduledTask -TaskName '$TaskName'"
"  Get-ScheduledTaskInfo -TaskName '$TaskName' | Select LastRunTime,LastTaskResult"
