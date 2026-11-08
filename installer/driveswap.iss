; DriveSwap installer (Inno Setup 6) - build with:  ISCC installer\driveswap.iss
#define AppVer "1.1.0"
[Setup]
AppId={{6F2B8C1E-4D7A-4E0B-9C55-D21A7B0C3E11}
AppName=DriveSwap
AppVersion={#AppVer}
AppPublisher=Michaelunkai
AppPublisherURL=https://github.com/Michaelunkai/driveswap
DefaultDirName={autopf}\DriveSwap
DefaultGroupName=DriveSwap
PrivilegesRequired=admin
ArchitecturesInstallIn64BitMode=x64compatible
ArchitecturesAllowed=x64compatible
OutputDir=..\dist
OutputBaseFilename=DriveSwap-Setup-{#AppVer}
Compression=lzma2/ultra
SolidCompression=yes
WizardStyle=modern
ChangesEnvironment=yes
UninstallDisplayName=DriveSwap
LicenseFile=..\LICENSE
DisableProgramGroupPage=yes

[Files]
Source: "..\dist\driveswap.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\README.md"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\LICENSE"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{group}\DriveSwap"; Filename: "{app}\driveswap.exe"; WorkingDir: "{app}"
Name: "{group}\DriveSwap help"; Filename: "{cmd}"; Parameters: "/k ""{app}\driveswap.exe"" --help"
Name: "{group}\Uninstall DriveSwap"; Filename: "{uninstallexe}"

[Registry]
; adds the install folder to the system PATH so "driveswap e f" works in any terminal
Root: HKLM; Subkey: "SYSTEM\CurrentControlSet\Control\Session Manager\Environment"; ValueType: expandsz; ValueName: "Path"; ValueData: "{olddata};{app}"; Check: NeedsAddPath(ExpandConstant('{app}'))

[Run]
Filename: "{cmd}"; Parameters: "/k ""{app}\driveswap.exe"" --help"; Description: "Show usage"; Flags: postinstall nowait skipifsilent unchecked

[Code]
function NeedsAddPath(Param: string): boolean;
var OrigPath: string;
begin
  if not RegQueryStringValue(HKEY_LOCAL_MACHINE, 'SYSTEM\CurrentControlSet\Control\Session Manager\Environment', 'Path', OrigPath) then begin Result := True; exit; end;
  Result := Pos(';' + Lowercase(Param) + ';', ';' + Lowercase(OrigPath) + ';') = 0;
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
var P: string; I: Integer; A: string;
begin
  if CurUninstallStep = usPostUninstall then begin
    if RegQueryStringValue(HKEY_LOCAL_MACHINE, 'SYSTEM\CurrentControlSet\Control\Session Manager\Environment', 'Path', P) then begin
      A := ';' + ExpandConstant('{app}');
      I := Pos(Lowercase(A), Lowercase(P));
      if I > 0 then begin
        Delete(P, I, Length(A));
        RegWriteExpandStringValue(HKEY_LOCAL_MACHINE, 'SYSTEM\CurrentControlSet\Control\Session Manager\Environment', 'Path', P);
      end;
    end;
  end;
end;
