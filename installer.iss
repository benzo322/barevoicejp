; BAREVoiceJP 社内配布用インストーラー
; 一般ユーザー権限で {localappdata}\Programs\BAREVoiceJP へインストールする。

#define MyAppName "BAREVoiceJP"
#define MyAppVersion "0.1.5"
#define MyAppExeName "BAREVoiceJP.exe"

[Setup]
AppId={{94422DF8-2CDD-4A03-AC9E-A1AA1088BB0A}
AppName={#MyAppName}
AppVersion={#MyAppVersion}
AppVerName={#MyAppName} {#MyAppVersion}
AppPublisher={#MyAppName}
DefaultDirName={localappdata}\Programs\BAREVoiceJP
DefaultGroupName={#MyAppName}
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
OutputDir=Output
OutputBaseFilename=BAREVoiceJP_Setup
SetupIconFile=app.ico
UninstallDisplayIcon={app}\{#MyAppExeName}
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
VersionInfoVersion=0.1.5.0
VersionInfoCompany={#MyAppName}
VersionInfoDescription={#MyAppName} Setup
VersionInfoProductName={#MyAppName}
VersionInfoProductVersion={#MyAppVersion}

[Languages]
Name: "japanese"; MessagesFile: "compiler:Languages\Japanese.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"

[Files]
; 実行ファイルと sherpa-onnx / onnxruntime の DLL 一式
Source: "target\release\{#MyAppExeName}"; DestDir: "{app}"; Flags: ignoreversion
Source: "target\release\*.dll"; DestDir: "{app}"; Flags: ignoreversion

; ユーザーが編集する辞書・置換設定 (アップグレード時は既存ファイルを残す)
Source: "words.txt"; DestDir: "{app}"; Flags: onlyifdoesntexist
Source: "replacements.json"; DestDir: "{app}"; Flags: onlyifdoesntexist

[Dirs]
; モデル本体は同梱せず、初回起動後にアプリがダウンロードする。
Name: "{app}\models"

[Icons]
Name: "{userprograms}\{#MyAppName}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"; WorkingDir: "{app}"
Name: "{userprograms}\{#MyAppName}\Uninstall {#MyAppName}"; Filename: "{uninstallexe}"
Name: "{userdesktop}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"; WorkingDir: "{app}"; Tasks: desktopicon

[Run]
Filename: "{app}\{#MyAppExeName}"; Description: "{cm:LaunchProgram,{#StringChange(MyAppName, '&', '&&')}}"; Flags: nowait postinstall skipifsilent
