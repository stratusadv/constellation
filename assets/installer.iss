#ifndef MyAppVersion
  #define MyAppVersion "0.0.1"
#endif

#define MyAppName "constellation"
#define MyAppPublisher "Stratus Advanced Technologies"
#define MyAppURL "https://stratusadv.com/"
#define MyAppExeName "constellation.exe"
#define MyAppId "{{B7E4B0A2-9C3D-4F1E-A6D8-2F5C1E9A4D3B}"

[Setup]
AppId={#MyAppId}
AppName={#MyAppName}
AppVersion={#MyAppVersion}
AppPublisher={#MyAppPublisher}
AppPublisherURL={#MyAppURL}
AppSupportURL={#MyAppURL}
AppUpdatesURL={#MyAppURL}
DefaultDirName={autopf}\{#MyAppName}
DisableDirPage=yes
DisableProgramGroupPage=yes
UninstallDisplayIcon={app}\{#MyAppExeName}
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
OutputDir=Output
OutputBaseFilename=constellation-setup
SolidCompression=yes
WizardStyle=modern
PrivilegesRequired=lowest
ChangesEnvironment=yes

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Files]
Source: "..\target\release\constellation.exe"; DestDir: "{app}"; Flags: ignoreversion

[Registry]
Root: HKCU; Subkey: "Environment"; ValueType: expandsz; ValueName: "Path"; ValueData: "{olddata};{app}"; Check: NeedsAddPath; Flags: preservestringtype

[Run]
Filename: "{app}\{#MyAppExeName}"; Parameters: "install"; StatusMsg: "Registering constellation with your AI agents..."; Flags: runhidden

[UninstallRun]
Filename: "{app}\{#MyAppExeName}"; Parameters: "uninstall"; Flags: runhidden; RunOnceId: "ConstellationUnregister"

[Code]
const
  EnvironmentKey = 'Environment';

function NeedsAddPath: Boolean;
var
  OrigPath: string;
  AppDir: string;
begin
  if not RegQueryStringValue(HKEY_CURRENT_USER, EnvironmentKey, 'Path', OrigPath) then
  begin
    Result := True;
    Exit;
  end;

  AppDir := ExpandConstant('{app}');
  Result := Pos(';' + Uppercase(AppDir) + ';', ';' + Uppercase(OrigPath) + ';') = 0;
end;

procedure RemovePath(AppDir: string);
var
  Paths: string;
  Index: Integer;
begin
  if not RegQueryStringValue(HKEY_CURRENT_USER, EnvironmentKey, 'Path', Paths) then
    Exit;

  Index := Pos(';' + Uppercase(AppDir) + ';', ';' + Uppercase(Paths) + ';');

  if Index = 0 then
    Exit;

  Delete(Paths, Index - 1, Length(AppDir) + 1);
  RegWriteExpandStringValue(HKEY_CURRENT_USER, EnvironmentKey, 'Path', Paths);
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if CurUninstallStep = usUninstall then
    RemovePath(ExpandConstant('{app}'));
end;
