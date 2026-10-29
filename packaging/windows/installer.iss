; Inno Setup script – builds a per-user installer (no admin rights needed).
; iscc /DAppVersion=0.1.0 packaging\windows\installer.iss

#ifndef AppVersion
  #define AppVersion "0.1.0"
#endif

[Setup]
; same AppId as "Better LibreLinkUp" (the former name), so its setup updates that installation
AppId={{7B8E5C2A-3D41-4E8B-9F0A-BE77A1D0C0DE}
AppName=Genius LibreLinkUp
AppVersion={#AppVersion}
AppPublisher=Lukas Kulmer
DefaultDirName={localappdata}\Programs\Genius LibreLinkUp
DefaultGroupName=Genius LibreLinkUp
PrivilegesRequired=lowest
OutputDir=..\..\target\installer
OutputBaseFilename=genius-librelinkup-{#AppVersion}-setup
SetupIconFile=..\..\assets\app.ico
UninstallDisplayIcon={app}\genius-librelinkup.exe
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
CloseApplications=yes
RestartApplications=no
; move installations of the former name to the new folder
UsePreviousAppDir=no
UsePreviousGroup=no

[Files]
Source: "..\..\target\release\genius-librelinkup.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\assets\app.ico"; DestDir: "{app}"; DestName: "genius-librelinkup.ico"; Flags: ignoreversion

[Icons]
Name: "{group}\Genius LibreLinkUp"; Filename: "{app}\genius-librelinkup.exe"; AppUserModelID: "io.genius-librelinkup"
Name: "{userdesktop}\Genius LibreLinkUp"; Filename: "{app}\genius-librelinkup.exe"; Tasks: desktopicon

[Tasks]
Name: "desktopicon"; Description: "Create a desktop shortcut"; Flags: unchecked

[Run]
Filename: "{app}\genius-librelinkup.exe"; Description: "Start Genius LibreLinkUp"; Flags: nowait postinstall skipifsilent
; in-app updates run the setup with /SILENT – start the new version afterwards
Filename: "{app}\genius-librelinkup.exe"; Parameters: "--updated"; Flags: nowait; Check: WizardSilent

[InstallDelete]
; leftovers of the former name "Better LibreLinkUp"
Type: filesandordirs; Name: "{localappdata}\Programs\Better LibreLinkUp"
Type: filesandordirs; Name: "{userprograms}\Better LibreLinkUp"
Type: files; Name: "{userdesktop}\Better LibreLinkUp.lnk"

[UninstallRun]
Filename: "taskkill"; Parameters: "/IM genius-librelinkup.exe /F"; Flags: runhidden; RunOnceId: "KillApp"

[Registry]
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueName: "GeniusLibreLinkUp"; Flags: uninsdeletevalue dontcreatekey

[Code]
procedure CurStepChanged(CurStep: TSetupStep);
var
  ResultCode: Integer;
begin
  // the former exe may still be shutting down after starting this setup as an update
  if CurStep = ssInstall then
    Exec('taskkill', '/IM better-librelinkup.exe /F', '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
end;
