; Muxen's Windows installer (Inno Setup 6), made by scripts\package-windows.ps1
; from dist\windows\Muxen. Installs for this user (no administrator needed)
; in %LOCALAPPDATA%\Programs\Muxen, with a Start menu entry, and optionally a
; desktop shortcut and `muxen` on the PATH.

#ifndef AppVersion
  #define AppVersion "0.1.0"
#endif
; x64, or arm64 for Windows on Arm (which also runs the x64 one).
#ifndef Arch
  #define Arch "x64"
#endif

[Setup]
AppId={{6F0B8E52-3C8E-4C8C-9E5B-6C4D2A9B7E31}
AppName=Muxen
AppVersion={#AppVersion}
AppVerName=Muxen {#AppVersion}
AppPublisher=Lumary Labs
AppPublisherURL=https://muxin.dev
DefaultDirName={localappdata}\Programs\Muxen
DefaultGroupName=Muxen
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
OutputDir=..\dist
OutputBaseFilename=Muxen-Setup-{#Arch}
SetupIconFile=..\assets\brand\muxen.ico
UninstallDisplayIcon={app}\muxen.exe
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
Name: "addtopath"; Description: "Add the muxen command to the PATH (for terminals and AI agents)"; GroupDescription: "Command line:"

[Files]
Source: "..\dist\windows\Muxen\muxen.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\dist\windows\Muxen\agents\*"; DestDir: "{app}\agents"; Flags: ignoreversion
Source: "..\dist\windows\Muxen\README.md"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\dist\windows\Muxen\LICENSE"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\dist\windows\Muxen\THIRD_PARTY_NOTICES.md"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\Muxen"; Filename: "{app}\muxen.exe"
Name: "{autodesktop}\Muxen"; Filename: "{app}\muxen.exe"; Tasks: desktopicon

[Run]
Filename: "{app}\muxen.exe"; Description: "Open Muxen"; Flags: nowait postinstall skipifsilent

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
