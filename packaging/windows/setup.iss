; SEE THE DOCUMENTATION FOR DETAILS ON CREATING INNO SETUP SCRIPT FILES!

#define MyAppVersion 1
#define MyAppPublisher "Gephyra OU"
#define MyAppURL "https://geph.io/"
#define MyAppExeName "geph5-client-gui.exe"

[Setup]
; NOTE: The value of AppId uniquely identifies this application.
; Do not use the same AppId value in installers for other applications.
; (To generate a new GUID, click Tools | Generate GUID inside the IDE.)
AppId={{09220679-1AE0-43B6-A263-AAE2CC36B9E3}
AppName={cm:MyAppName}
AppVersion={#MyAppVersion}
;AppVerName={#MyAppName} {#MyAppVersion}
AppPublisher={#MyAppPublisher}
AppPublisherURL={#MyAppURL} 
AppSupportURL={#MyAppURL}
AppUpdatesURL={#MyAppURL}
DefaultDirName={pf}\{cm:MyAppName}
DefaultGroupName={cm:MyAppName}
OutputBaseFilename=geph5-setup
Compression=lzma2
SolidCompression=yes
WizardStyle=modern

[Languages]
Name: "en"; MessagesFile: "compiler:Default.isl"
Name: "zht"; MessagesFile: "ChineseTraditional.isl"
Name: "zhs"; MessagesFile: "ChineseSimplified.isl"

[CustomMessages]
en.MyAppName=Geph
zht.MyAppName=迷霧通
zhs.MyAppName=迷雾通

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[InstallDelete]
Type: filesandordirs; Name: "{app}\*"

[Files]
Source: "geph5-client-gui.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "geph5-client.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "vulkan-1.dll"; DestDir: "{app}"; Flags: ignoreversion
Source: "WinDivert.dll"; DestDir: "{app}"; Flags: ignoreversion
Source: "WinDivert32.sys"; DestDir: "{app}"; Flags: ignoreversion
Source: "WinDivert64.sys"; DestDir: "{app}"; Flags: ignoreversion


[Icons]
Name: "{group}\{cm:MyAppName}"; Filename: "{app}\{#MyAppExeName}"
Name: "{group}\{cm:UninstallProgram,{cm:MyAppName}}"; Filename: "{uninstallexe}"
Name: "{commondesktop}\{cm:MyAppName}"; Filename: "{app}\{#MyAppExeName}"; Tasks: desktopicon
