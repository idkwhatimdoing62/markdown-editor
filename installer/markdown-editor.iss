#define MyAppName "Markdown 编辑器与预览器"
#define MyAppVersion "0.1.22"
#define MyAppPublisher "idkwhatimdoing62"
#define MyAppURL "https://github.com/idkwhatimdoing62/markdown-editor"
#define MyAppExeName "markdown-editor.exe"

#ifndef BuildDir
  #define BuildDir "..\target\release"
#endif

[Setup]
AppId={{9C823C09-02D2-4C96-B75A-B7E01197CD8A}
AppName={#MyAppName}
AppVersion={#MyAppVersion}
AppPublisher={#MyAppPublisher}
AppPublisherURL={#MyAppURL}
AppSupportURL={#MyAppURL}/issues
AppUpdatesURL={#MyAppURL}/releases
DefaultDirName={localappdata}\Programs\Markdown Editor
DefaultGroupName={#MyAppName}
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
OutputDir=..\dist
OutputBaseFilename=markdown-editor-v{#MyAppVersion}-windows-x86_64-setup
Compression=lzma2/ultra64
SolidCompression=yes
WizardStyle=modern
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
SetupIconFile=..\assets\app-icon.ico
UninstallDisplayIcon={app}\app-icon.ico
VersionInfoVersion={#MyAppVersion}.0
VersionInfoCompany={#MyAppPublisher}
VersionInfoDescription={#MyAppName} 安装程序
VersionInfoProductName={#MyAppName}
VersionInfoProductVersion={#MyAppVersion}
SetupLogging=yes
CloseApplications=yes
RestartApplications=no

[Languages]
Name: "chinesesimplified"; MessagesFile: "languages\ChineseSimplified.isl"

[Tasks]
Name: "desktopicon"; Description: "创建桌面快捷方式"; GroupDescription: "附加快捷方式："; Flags: unchecked

[Files]
Source: "{#BuildDir}\{#MyAppExeName}"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\README.md"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\fonts\JetBrainsMono-OFL.txt"; DestDir: "{app}\licenses"; Flags: ignoreversion
Source: "..\fonts\LXGWWenKaiLite-OFL.txt"; DestDir: "{app}\licenses"; Flags: ignoreversion
Source: "..\assets\Mermaid-MIT.txt"; DestDir: "{app}\licenses"; Flags: ignoreversion
Source: "..\assets\app-icon.ico"; DestDir: "{app}"; Flags: ignoreversion

[Registry]
Root: HKCU; Subkey: "Software\Classes\MarkdownEditor.Markdown"; ValueType: string; ValueName: ""; ValueData: "Markdown 文档"; Flags: uninsdeletekey
Root: HKCU; Subkey: "Software\Classes\MarkdownEditor.Markdown\DefaultIcon"; ValueType: string; ValueName: ""; ValueData: "{app}\{#MyAppExeName},0"
Root: HKCU; Subkey: "Software\Classes\MarkdownEditor.Markdown\shell\open\command"; ValueType: string; ValueName: ""; ValueData: """{app}\{#MyAppExeName}"" ""%1"""
Root: HKCU; Subkey: "Software\Classes\.md\OpenWithProgids"; ValueType: none; ValueName: "MarkdownEditor.Markdown"; Flags: uninsdeletevalue
Root: HKCU; Subkey: "Software\Classes\.markdown\OpenWithProgids"; ValueType: none; ValueName: "MarkdownEditor.Markdown"; Flags: uninsdeletevalue
Root: HKCU; Subkey: "Software\MarkdownEditor\Capabilities"; ValueType: string; ValueName: "ApplicationName"; ValueData: "Markdown Editor"; Flags: uninsdeletekey
Root: HKCU; Subkey: "Software\MarkdownEditor\Capabilities"; ValueType: string; ValueName: "ApplicationDescription"; ValueData: "Markdown 编辑器与预览器"
Root: HKCU; Subkey: "Software\MarkdownEditor\Capabilities"; ValueType: string; ValueName: "ApplicationIcon"; ValueData: "{app}\{#MyAppExeName},0"
Root: HKCU; Subkey: "Software\MarkdownEditor\Capabilities\FileAssociations"; ValueType: string; ValueName: ".md"; ValueData: "MarkdownEditor.Markdown"
Root: HKCU; Subkey: "Software\MarkdownEditor\Capabilities\FileAssociations"; ValueType: string; ValueName: ".markdown"; ValueData: "MarkdownEditor.Markdown"
Root: HKCU; Subkey: "Software\RegisteredApplications"; ValueType: string; ValueName: "Markdown Editor"; ValueData: "Software\MarkdownEditor\Capabilities"; Flags: uninsdeletevalue

[Icons]
Name: "{group}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"; IconFilename: "{app}\app-icon.ico"
Name: "{group}\卸载 {#MyAppName}"; Filename: "{uninstallexe}"
Name: "{autodesktop}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"; IconFilename: "{app}\app-icon.ico"; Tasks: desktopicon

[Run]
Filename: "{app}\{#MyAppExeName}"; Description: "启动 {#MyAppName}"; Flags: nowait postinstall skipifsilent
