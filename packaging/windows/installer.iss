; Inno Setup script – builds a per-user installer (no admin rights needed).
; iscc /DAppVersion=0.1.1 packaging\windows\installer.iss

#ifndef AppVersion
  #define AppVersion "0.1.1"
#endif

[Setup]
AppId={{7B8E5C2A-3D41-4E8B-9F0A-BE77A1D0C0DE}
AppName=Better LibreLinkUp
AppVersion={#AppVersion}
AppPublisher=Lukas Kulmer
DefaultDirName={localappdata}\Programs\Better LibreLinkUp
DefaultGroupName=Better LibreLinkUp
PrivilegesRequired=lowest
OutputDir=..\..\target\installer
OutputBaseFilename=better-librelinkup-{#AppVersion}-setup
SetupIconFile=..\..\assets\app.ico
UninstallDisplayIcon={app}\better-librelinkup.exe
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
CloseApplications=yes
RestartApplications=no

[Files]
Source: "..\..\target\release\better-librelinkup.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\assets\app.ico"; DestDir: "{app}"; DestName: "better-librelinkup.ico"; Flags: ignoreversion

[Icons]
Name: "{group}\Better LibreLinkUp"; Filename: "{app}\better-librelinkup.exe"; AppUserModelID: "io.better-librelinkup"
Name: "{userdesktop}\Better LibreLinkUp"; Filename: "{app}\better-librelinkup.exe"; Tasks: desktopicon

[Tasks]
Name: "desktopicon"; Description: "Create a desktop shortcut"; Flags: unchecked

[Run]
Filename: "{app}\better-librelinkup.exe"; Description: "Start Better LibreLinkUp"; Flags: nowait postinstall skipifsilent
; in-app updates run the setup with /SILENT – start the new version afterwards
Filename: "{app}\better-librelinkup.exe"; Parameters: "--updated"; Flags: nowait; Check: WizardSilent

[UninstallRun]
Filename: "taskkill"; Parameters: "/IM better-librelinkup.exe /F"; Flags: runhidden; RunOnceId: "KillApp"

[Registry]
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueName: "BetterLibreLinkUp"; Flags: uninsdeletevalue dontcreatekey
