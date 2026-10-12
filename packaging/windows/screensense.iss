#ifndef PayloadDir
  #error PayloadDir is required
#endif
#ifndef OutputDir
  #error OutputDir is required
#endif
#ifndef AppVersion
  #define AppVersion "0.1.0-preview"
#endif
#ifndef VersionNumber
  #define VersionNumber "0.1.0.0"
#endif
#ifndef AppIdentifier
  #define AppIdentifier "ScreenSense.917fbe92-1d22-4f87-af47-e143e7eca3dd"
#endif
#ifndef InstallerStem
  #define InstallerStem "ScreenSense-0.1.0-preview-windows-x64-setup"
#endif

[Setup]
AppId={#AppIdentifier}
AppName=ScreenSense
AppVersion={#AppVersion}
AppVerName=ScreenSense {#AppVersion}
AppPublisher=ScreenSense contributors
AppPublisherURL=https://github.com/stevenke1981/screensense
AppSupportURL=https://github.com/stevenke1981/screensense/issues
DefaultDirName={localappdata}\Programs\ScreenSense
DefaultGroupName=ScreenSense
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible and not arm64
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0
OutputDir={#OutputDir}
OutputBaseFilename={#InstallerStem}
VersionInfoVersion={#VersionNumber}
VersionInfoProductName=ScreenSense
VersionInfoProductVersion={#VersionNumber}
VersionInfoProductTextVersion={#AppVersion}
UninstallDisplayIcon={app}\screensense-gui.exe
UninstallDisplayName=ScreenSense {#AppVersion}
LicenseFile={#PayloadDir}\LICENSE-GUI-GPL-3.0.txt
InfoBeforeFile={#PayloadDir}\INSTALL-README.txt
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
DisableProgramGroupPage=no
AllowNoIcons=yes
CloseApplications=no
RestartApplications=no
SignedUninstaller=no
SetupLogging=yes

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "Create a desktop shortcut"; Flags: unchecked

[Files]
Source: "{#PayloadDir}\screensense.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#PayloadDir}\screensense-gui.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#PayloadDir}\LICENSE-GUI-GPL-3.0.txt"; DestDir: "{app}\licenses"; Flags: ignoreversion
Source: "{#PayloadDir}\LICENSE-CORE-MIT.txt"; DestDir: "{app}\licenses"; Flags: ignoreversion
Source: "{#PayloadDir}\THIRD-PARTY-NOTICES.md"; DestDir: "{app}\licenses"; Flags: ignoreversion
Source: "{#PayloadDir}\INSTALL-README.txt"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#PayloadDir}\BUILD-INFO.json"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#PayloadDir}\GUI.md"; DestDir: "{app}\docs"; Flags: ignoreversion
Source: "{#PayloadDir}\README.md"; DestDir: "{app}\docs"; Flags: ignoreversion
Source: "{#PayloadDir}\README.zh-TW.md"; DestDir: "{app}\docs"; Flags: ignoreversion
Source: "{#PayloadDir}\source.tar"; DestDir: "{app}\source"; Flags: ignoreversion
Source: "{#PayloadDir}\dependency-sources.zip"; DestDir: "{app}\source"; Flags: ignoreversion
Source: "{#PayloadDir}\LICENSES\*"; DestDir: "{app}\licenses\dependencies"; Flags: ignoreversion recursesubdirs createallsubdirs

[Icons]
Name: "{group}\ScreenSense"; Filename: "{app}\screensense-gui.exe"
Name: "{group}\ScreenSense usage"; Filename: "{app}\INSTALL-README.txt"
Name: "{autodesktop}\ScreenSense"; Filename: "{app}\screensense-gui.exe"; Tasks: desktopicon

; Intentionally no [Run], [Registry], [UninstallRun] or [UninstallDelete].
; Inno creates only its standard per-user uninstall registration. User config,
; databases, images, hooks, PATH, MCP, autostart and services are never managed.
