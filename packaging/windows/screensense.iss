#ifndef PayloadDir
  #error PayloadDir is required
#endif
#ifndef OutputDir
  #error OutputDir is required
#endif
#ifndef AppVersion
  #define AppVersion "0.2.0"
#endif
#ifndef VersionNumber
  #define VersionNumber "0.2.0.0"
#endif
#ifndef AppIdentifier
  #define AppIdentifier "ScreenSense.917fbe92-1d22-4f87-af47-e143e7eca3dd"
#endif
#ifndef InstallerStem
  #define InstallerStem "ScreenSense-0.2.0-windows-x64-setup"
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
; The GUI keeps a named mutex while it runs (it lives in the notification
; area). Setup and uninstall ask the user to close it; nothing is force-closed.
AppMutex=ScreenSenseGui-917fbe92-1d22-4f87-af47-e143e7eca3dd
SetupIconFile={#PayloadDir}\screensense.ico
RestartApplications=no
SignedUninstaller=no
SetupLogging=yes

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "Create a desktop shortcut"; Flags: unchecked
Name: "startgui"; Description: "Start ScreenSense in the notification area when I sign in (window and tray only; does not start recording)"; Flags: unchecked

[Files]
Source: "{#PayloadDir}\screensense.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#PayloadDir}\screensense-gui.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#PayloadDir}\LICENSE-GUI-GPL-3.0.txt"; DestDir: "{app}\licenses"; Flags: ignoreversion
Source: "{#PayloadDir}\LICENSE-CORE-MIT.txt"; DestDir: "{app}\licenses"; Flags: ignoreversion
Source: "{#PayloadDir}\THIRD-PARTY-NOTICES.md"; DestDir: "{app}\licenses"; Flags: ignoreversion
Source: "{#PayloadDir}\screensense.ico"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#PayloadDir}\INSTALL-README.txt"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#PayloadDir}\BUILD-INFO.json"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#PayloadDir}\GUI.md"; DestDir: "{app}\docs"; Flags: ignoreversion
Source: "{#PayloadDir}\README.md"; DestDir: "{app}\docs"; Flags: ignoreversion
Source: "{#PayloadDir}\README.zh-TW.md"; DestDir: "{app}\docs"; Flags: ignoreversion
Source: "{#PayloadDir}\source.tar"; DestDir: "{app}\source"; Flags: ignoreversion
Source: "{#PayloadDir}\dependency-sources.zip"; DestDir: "{app}\source"; Flags: ignoreversion
Source: "{#PayloadDir}\LICENSES\*"; DestDir: "{app}\licenses\dependencies"; Flags: ignoreversion recursesubdirs createallsubdirs

[Icons]
Name: "{group}\ScreenSense"; Filename: "{app}\screensense-gui.exe"; IconFilename: "{app}\screensense.ico"
Name: "{group}\ScreenSense usage"; Filename: "{app}\INSTALL-README.txt"
Name: "{autodesktop}\ScreenSense"; Filename: "{app}\screensense-gui.exe"; IconFilename: "{app}\screensense.ico"; Tasks: desktopicon

[Registry]
; Only when the optional "startgui" task is selected. This starts the GUI
; (tray icon only), never the recording daemon.
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: string; ValueName: "ScreenSenseGui"; ValueData: """{app}\screensense-gui.exe"" --minimized"; Flags: uninsdeletevalue; Tasks: startgui

[Run]
; Opening the GUI does not start recording.
Filename: "{app}\screensense-gui.exe"; Description: "Launch ScreenSense"; Flags: nowait postinstall skipifsilent

; What setup manages, exactly:
;  - program files under {app}, shortcuts, and Inno's standard per-user
;    uninstall registration;
;  - one optional HKCU Run value (ScreenSenseGui), only if the "startgui" task
;    is chosen; it launches the GUI with --minimized (tray only) and is removed
;    on uninstall;
;  - an optional post-install launch of the GUI (skipped when silent).
; Never managed: PATH, shell profiles, services, a recording daemon or its
; autostart, MCP configuration, user config, databases, images or hooks.
; There is no [UninstallRun] or [UninstallDelete].
