; AgentDesk installer.
;
; Per-user install, no admin required (PrivilegesRequired=lowest) -- this is
; a personal productivity tool for one account, not a fleet deployment.
; Installs to %LocalAppData%\Programs\AgentDesk, registers the two
; Scheduled Tasks the app already relies on (logon launch, hourly backup)
; against the installed exe, and a normal uninstaller that removes both
; tasks but never touches %LocalAppData%\AgentDesk (the actual board data)
; or the vault mirror -- both must survive an uninstall/reinstall.
;
; Build: compile the exe first (see docs/IT-Deployment-Notes.md for the
; exact Nuitka command), then:
;   iscc scripts\AgentDesk.iss
; then sign the resulting installer exe the same way the app exe is signed.
;
; SourceExeDir below is a build-machine path, not something this script
; discovers on its own -- set it to wherever the signed AgentDesk.exe and
; its bundled dependency folder currently live.

#define SourceExeDir "C:\AgentDeskBuild\cli.dist"
#define MyAppName "AgentDesk"
#define MyAppVersion "1.0.0"
#define MyAppPublisher "palencharj"

[Setup]
AppId={{4C6C9B6B-6C9B-4D3A-9A9F-AD3D5C0001EE}
AppName={#MyAppName}
AppVersion={#MyAppVersion}
AppPublisher={#MyAppPublisher}
DefaultDirName={localappdata}\Programs\{#MyAppName}
DefaultGroupName={#MyAppName}
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
OutputBaseFilename=AgentDesk-Setup
OutputDir=..\build\installer
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
; The app exe is already self-signed separately; the installer exe itself
; should be signed too after ISCC produces it (see build notes above) --
; Inno's own SignTool= directive needs configuring in the IDE/compiler
; settings for a machine-portable path, so this repo signs the installer
; as a separate manual step instead, kept in one place in the build docs.

[Files]
Source: "{#SourceExeDir}\*"; DestDir: "{app}"; Flags: recursesubdirs ignoreversion
Source: "Install-AgentDeskStartup.ps1"; DestDir: "{app}\scripts"; Flags: ignoreversion
Source: "Install-AgentDeskTask.ps1"; DestDir: "{app}\scripts"; Flags: ignoreversion

[Icons]
; This is a fallback / uninstall-visibility shortcut. The app's own
; aumid.ensure_shortcut() (agentdesk/aumid.py) rewrites the AUMID-bearing
; Start Menu shortcut itself on first GUI launch, pointed at whichever exe
; sys.executable resolves to -- so this entry mainly guarantees something
; appears in the Start Menu even before the app has run once, and gives
; Windows an uninstall-time icon to remove.
Name: "{group}\{#MyAppName}"; Filename: "{app}\AgentDesk.exe"

[Run]
; Register both Scheduled Tasks against the installed exe. These call the
; SAME scripts the dev-mode setup uses (with the new -Exe override added
; specifically for this), not a reimplementation, so the task XML (delay,
; IgnoreNew policy, InteractiveToken, etc.) stays in exactly one place.
Filename: "powershell.exe"; Parameters: "-NoProfile -ExecutionPolicy Bypass -File ""{app}\scripts\Install-AgentDeskStartup.ps1"" -Exe ""{app}\AgentDesk.exe"""; Flags: runhidden; StatusMsg: "Registering the logon-launch task..."
Filename: "powershell.exe"; Parameters: "-NoProfile -ExecutionPolicy Bypass -File ""{app}\scripts\Install-AgentDeskTask.ps1"" -Exe ""{app}\AgentDesk.exe"""; Flags: runhidden; StatusMsg: "Registering the hourly backup task..."
; Launch once at the end of setup so the tray icon appears immediately
; rather than waiting for the next logon.
Filename: "{app}\AgentDesk.exe"; Description: "Launch AgentDesk now"; Flags: postinstall nowait skipifsilent

[UninstallRun]
; Must run BEFORE the files are removed (RunOnceId ties it to uninstall
; ordering), since both scripts live under {app}\scripts.
Filename: "powershell.exe"; Parameters: "-NoProfile -ExecutionPolicy Bypass -File ""{app}\scripts\Install-AgentDeskStartup.ps1"" -Uninstall"; Flags: runhidden; RunOnceId: "RemoveStartupTask"
Filename: "powershell.exe"; Parameters: "-NoProfile -ExecutionPolicy Bypass -File ""{app}\scripts\Install-AgentDeskTask.ps1"" -Uninstall"; Flags: runhidden; RunOnceId: "RemoveBackupTask"

; Deliberately no [UninstallDelete] entries beyond what Inno removes by
; default (the {app} install directory itself) -- %LocalAppData%\AgentDesk
; (the SQLite board, log, worker state) and the vault mirror are NEVER
; touched by install or uninstall, in either direction.
