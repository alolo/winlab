
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
PrivilegesRequired=admin
UninstallDisplayIcon={app}\TaSQ.exe

ArchitecturesAllowed=x64
ArchitecturesInstallIn64BitMode=x64
PrivilegesRequired=lowest

OutputDir=installer
OutputBaseFilename=TaSQ_Setup_{#MyAppVersion}_x64

Compression=lzma2
SolidCompression=yes
WizardStyle=modern

[Tasks]
Name: "desktopicon"; Description: "Создать ярлык на рабочем столе"; GroupDescription: "Дополнительные задачи:"; Flags: unchecked

[Files]
Source: "target\release\TaSQ.exe"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\TaSQ"; Filename: "{app}\TaSQ.exe"; WorkingDir: "{app}"
Name: "{autodesktop}\TaSQ"; Filename: "{app}\TaSQ.exe"; WorkingDir: "{app}"; Tasks: desktopicon

[Run]
Filename: "{app}\TaSQ.exe"; Description: "Запустить Theatre Show Cue"; Flags: postinstall nowait skipifsilent
