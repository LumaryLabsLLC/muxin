; Muxin's Windows installer (Inno Setup 6), made by scripts\package-windows.ps1
; from dist\windows\Muxin. Installs for this user (no administrator needed)
; in %LOCALAPPDATA%\Programs\Muxin, with a Start menu entry, and optionally a
; desktop shortcut and `muxin` on the PATH. The AppId is the one from when it
; was called Muxen, so it updates that install in its folder.

#ifndef AppVersion
  #define AppVersion "0.1.0"
#endif
; x64, or arm64 for Windows on Arm (which also runs the x64 one).
#ifndef Arch
  #define Arch "x64"
#endif

[Setup]
AppId={{6F0B8E52-3C8E-4C8C-9E5B-6C4D2A9B7E31}
AppName=Muxin
AppVersion={#AppVersion}
AppVerName=Muxin {#AppVersion}
AppPublisher=Lumary Labs
AppPublisherURL=https://muxin.dev
DefaultDirName={localappdata}\Programs\Muxin
DefaultGroupName=Muxin
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
OutputDir=..\dist
OutputBaseFilename=Muxin-Setup-{#Arch}
SetupIconFile=..\assets\brand\muxin.ico
UninstallDisplayIcon={app}\muxin.exe
Compression=lzma2/max
SolidCompression=yes
#if Arch == "arm64"
ArchitecturesAllowed=arm64
ArchitecturesInstallIn64BitMode=arm64
#else
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
#endif
WizardStyle=modern
ChangesEnvironment=yes
CloseApplications=yes

[Tasks]
Name: "desktopicon"; Description: "Add a desktop shortcut"; GroupDescription: "Shortcuts:"; Flags: unchecked
Name: "addtopath"; Description: "Add the muxin command to the PATH (for terminals and AI agents)"; GroupDescription: "Command line:"

[Files]
Source: "..\dist\windows\Muxin\muxin.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\dist\windows\Muxin\muxen.cmd"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\dist\windows\Muxin\agents\*"; DestDir: "{app}\agents"; Flags: ignoreversion
Source: "..\dist\windows\Muxin\README.md"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\dist\windows\Muxin\LICENSE"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\dist\windows\Muxin\NOTICE"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\dist\windows\Muxin\THIRD_PARTY_NOTICES.md"; DestDir: "{app}"; Flags: ignoreversion

; From when it was called Muxen: its program and shortcuts.
[InstallDelete]
Type: files; Name: "{app}\muxen.exe"
Type: files; Name: "{autoprograms}\Muxen.lnk"
Type: files; Name: "{autodesktop}\Muxen.lnk"

; Share links (muxin://join...) open in Muxin.
[Registry]
Root: HKCU; Subkey: "Software\Classes\muxin"; ValueType: string; ValueName: ""; ValueData: "URL:Muxin share link"; Flags: uninsdeletekey
Root: HKCU; Subkey: "Software\Classes\muxin"; ValueType: string; ValueName: "URL Protocol"; ValueData: ""
Root: HKCU; Subkey: "Software\Classes\muxin\DefaultIcon"; ValueType: string; ValueName: ""; ValueData: "{app}\muxin.exe,0"
Root: HKCU; Subkey: "Software\Classes\muxin\shell\open\command"; ValueType: string; ValueName: ""; ValueData: """{app}\muxin.exe"" ""%1"""

[Icons]
Name: "{autoprograms}\Muxin"; Filename: "{app}\muxin.exe"
Name: "{autodesktop}\Muxin"; Filename: "{app}\muxin.exe"; Tasks: desktopicon

[Run]
Filename: "{app}\muxin.exe"; Description: "Open Muxin"; Flags: nowait postinstall skipifsilent

[Code]
const
  EnvKey = 'Environment';

function PathHas(Dir: string): Boolean;
var
  Path: string;
begin
  if not RegQueryStringValue(HKEY_CURRENT_USER, EnvKey, 'Path', Path) then
    Path := '';
  Result := Pos(';' + Uppercase(Dir) + ';', ';' + Uppercase(Path) + ';') > 0;
end;

procedure AddToPath(Dir: string);
var
  Path: string;
begin
  if PathHas(Dir) then
    exit;
  if not RegQueryStringValue(HKEY_CURRENT_USER, EnvKey, 'Path', Path) then
    Path := '';
  if (Path <> '') and (Copy(Path, Length(Path), 1) <> ';') then
    Path := Path + ';';
  RegWriteExpandStringValue(HKEY_CURRENT_USER, EnvKey, 'Path', Path + Dir);
end;

procedure RemoveFromPath(Dir: string);
var
  Path: string;
  P: Integer;
begin
  if not RegQueryStringValue(HKEY_CURRENT_USER, EnvKey, 'Path', Path) then
    exit;
  P := Pos(';' + Uppercase(Dir) + ';', ';' + Uppercase(Path) + ';');
  if P = 0 then
    exit;
  Delete(Path, P, Length(Dir) + 1);
  if (Length(Path) > 0) and (Copy(Path, Length(Path), 1) = ';') then
    Delete(Path, Length(Path), 1);
  RegWriteExpandStringValue(HKEY_CURRENT_USER, EnvKey, 'Path', Path);
end;

procedure CurStepChanged(CurStep: TSetupStep);
begin
  if (CurStep = ssPostInstall) and WizardIsTaskSelected('addtopath') then
    AddToPath(ExpandConstant('{app}'));
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if CurUninstallStep = usPostUninstall then
    RemoveFromPath(ExpandConstant('{app}'));
end;
