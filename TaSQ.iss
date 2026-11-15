
#define MyAppName "Theatre Show Cue"
#define MyAppVersion "0.9.0"
#define MyAppPublisher "Alo Production"
#define MyAppExeName "TaSQ.exe"

[Setup]
AppId={{22140366-A035-4ACE-9858-CE32DE462A3B}
AppName={#MyAppName}
AppVersion={#MyAppVersion}
AppPublisher={#MyAppPublisher}

DefaultDirName={autopf}\TaSQ
DefaultGroupName=TaSQ
PrivilegesRequired=lowest

SetupIconFile=assets\TaSQ_multi.ico
UninstallDisplayIcon={app}\TaSQ.exe

ArchitecturesAllowed=x64
ArchitecturesInstallIn64BitMode=x64

OutputDir=installer
OutputBaseFilename=TaSQ_Setup_{#MyAppVersion}_x64

Compression=lzma2
SolidCompression=yes
WizardStyle=modern

[Tasks]
Name: "desktopicon"; Description: "Создать ярлык на рабочем столе"; GroupDescription: "Дополнительные задачи:"; Flags: unchecked

[Files]
Source: "target\release\TaSQ.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "target\release\SDL2.dll"; DestDir: "{app}"; Flags: ignoreversion

; Microsoft Visual C++ Runtime
Source: "redist\VC_redist.x64.exe"; DestDir: "{tmp}"; Flags: deleteafterinstall ignoreversion

[Icons]
Name: "{autoprograms}\TaSQ"; Filename: "{app}\TaSQ.exe"; WorkingDir: "{app}"
Name: "{autodesktop}\TaSQ"; Filename: "{app}\TaSQ.exe"; WorkingDir: "{app}"; Tasks: desktopicon

[Run]
Filename: "{tmp}\VC_redist.x64.exe"; Parameters: "/install /quiet /norestart"; StatusMsg: "Проверка Microsoft Visual C++ Runtime..."; Flags: runhidden waituntilterminated; Check: not IsVCRedistCurrent

Filename: "{app}\TaSQ.exe"; Description: "Запустить Theatre Show Cue"; Flags: postinstall nowait skipifsilent


[Code]

function IsVCRedistCurrent: Boolean;
var
  Installed, Major, Minor, Bld, Rbld: Cardinal;
  BundledMajor, BundledMinor, BundledRevision, BundledBuild: Word;
  InstalledVersion, BundledVersion: Int64;
begin
  Result := False;

  if not RegQueryDWordValue(HKEY_LOCAL_MACHINE_64,
    'SOFTWARE\Microsoft\VisualStudio\14.0\VC\Runtimes\x64',
    'Installed', Installed) then
    Exit;

  if Installed <> 1 then
    Exit;

  if not RegQueryDWordValue(HKEY_LOCAL_MACHINE_64,
    'SOFTWARE\Microsoft\VisualStudio\14.0\VC\Runtimes\x64',
    'Major', Major) then Exit;

  if not RegQueryDWordValue(HKEY_LOCAL_MACHINE_64,
    'SOFTWARE\Microsoft\VisualStudio\14.0\VC\Runtimes\x64',
    'Minor', Minor) then Exit;

  if not RegQueryDWordValue(HKEY_LOCAL_MACHINE_64,
    'SOFTWARE\Microsoft\VisualStudio\14.0\VC\Runtimes\x64',
    'Bld', Bld) then Exit;

  if not RegQueryDWordValue(HKEY_LOCAL_MACHINE_64,
    'SOFTWARE\Microsoft\VisualStudio\14.0\VC\Runtimes\x64',
    'Rbld', Rbld) then Exit;

  if not GetVersionComponents(
    ExpandConstant('{tmp}\VC_redist.x64.exe'),
    BundledMajor, BundledMinor,
    BundledRevision, BundledBuild) then Exit;

  InstalledVersion := PackVersionComponents(
    Word(Major), Word(Minor), Word(Bld), Word(Rbld));

  BundledVersion := PackVersionComponents(
    BundledMajor, BundledMinor,
    BundledRevision, BundledBuild);

  Result := ComparePackedVersion(
    InstalledVersion, BundledVersion) >= 0;
end;

