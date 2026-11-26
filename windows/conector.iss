; Instalador de conector para Windows (Inno Setup 6).
; Lo compila .github/workflows/release.yml:  iscc /DAppVersion=0.2.0 windows\conector.iss
; Necesita target\release\conector.exe ya compilado.
;
; Se instala para el usuario (sin pedir permisos de administrador), en
; %LOCALAPPDATA%\Programs\conector.

#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif

[Setup]
; Identificador fijo: con él Windows reconoce las actualizaciones y el desinstalador.
AppId={{42C4A7D5-913A-4E3B-A429-F0335C9AABF1}
AppName=conector
AppVersion={#AppVersion}
AppVerName=conector {#AppVersion}
AppPublisher=airylp
AppPublisherURL=https://github.com/airylp/conector
AppSupportURL=https://github.com/airylp/conector/issues
AppUpdatesURL=https://github.com/airylp/conector/releases
DefaultDirName={autopf}\conector
DefaultGroupName=conector
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
OutputDir=..\target\instalador
OutputBaseFilename=conector-{#AppVersion}-setup
SetupIconFile=conector.ico
UninstallDisplayIcon={app}\conector.exe
UninstallDisplayName=conector
LicenseFile=..\LICENSE
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
; Si conector está abierto, el instalador ofrece cerrarlo (Restart Manager).
CloseApplications=yes
RestartApplications=no
ShowLanguageDialog=auto

[Languages]
Name: "es"; MessagesFile: "compiler:Languages\Spanish.isl"
Name: "en"; MessagesFile: "compiler:Default.isl"
Name: "fr"; MessagesFile: "compiler:Languages\French.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
Source: "..\target\release\conector.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\LICENSE"; DestDir: "{app}"; DestName: "LICENSE.txt"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\conector"; Filename: "{app}\conector.exe"
Name: "{autodesktop}\conector"; Filename: "{app}\conector.exe"; Tasks: desktopicon

[Run]
Filename: "{app}\conector.exe"; Description: "{cm:LaunchProgram,conector}"; Flags: nowait postinstall skipifsilent

[Code]
// Guarda el idioma elegido en el instalador como idioma de la app (si aún no
// hay uno elegido), igual que install.sh en macOS/Linux.
procedure CurStepChanged(CurStep: TSetupStep);
var
  Dir, F: String;
  Lines: TArrayOfString;
  I: Integer;
  Has: Boolean;
begin
  if CurStep <> ssPostInstall then
    Exit;
  Dir := ExpandConstant('{userappdata}\conector');
  F := Dir + '\ajustes.txt';
  Has := False;
  if LoadStringsFromFile(F, Lines) then
    for I := 0 to GetArrayLength(Lines) - 1 do
      if Pos('idioma=', Trim(Lines[I])) = 1 then
        Has := True;
  if not Has then
  begin
    ForceDirectories(Dir);
    SaveStringToFile(F, 'idioma=' + ActiveLanguage + #13#10, True);
  end;
end;
