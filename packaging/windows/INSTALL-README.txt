ScreenSense 0.2.0 for Windows x64

This installer contains matching CLI (screensense.exe) and GUI
(screensense-gui.exe) binaries built from the source commit recorded in
BUILD-INFO.json. It is unsigned; see the note on Windows warnings below.

Installation is per user and does not request administrator privileges. It
does not start recording, a daemon or a model connection. It does not add
shell hooks, PATH entries, services, daemon autostart or persistent MCP.
Two optional items are offered: a desktop shortcut, and "Start ScreenSense in
the notification area when I sign in" (off by default; it writes one
HKCU Run value, ScreenSenseGui, that only opens the GUI as a tray icon and
never starts recording; uninstall removes it). The last page can launch the
GUI; opening the GUI does not start recording.

If the GUI is running (it can sit in the notification area), setup and
uninstall ask you to close it first. Neither force-closes any process.

Start ScreenSense from the Start Menu or run screensense-gui.exe. The CLI is
screensense.exe. Start recording only when you explicitly choose to use the
existing CLI workflow and have reviewed your privacy configuration.

Uninstall removes installed program files, shortcuts and the optional Run value. It preserves user
configuration and recorded data. Default data: %LOCALAPPDATA%\screensense.
Default configuration: %APPDATA%\screensense\config.toml. Uninstall is not a
data wipe. Close any separately started ScreenSense CLI daemon before an upgrade or
uninstall; this installer does not force-close processes.

GUI and Slint: GPL-3.0-only. CLI/core/platform: MIT. Original per-package
dependency license texts and notices are in licenses/dependencies.
Corresponding project source is in source/source.tar and at the commit URL recorded
in BUILD-INFO.json. Locked dependency sources are included as
source/dependency-sources.zip.

The installer and executables are unsigned. Windows may show an unknown
publisher or reputation warning. Verify the installer's companion .sha256 file
and .build.json provenance, including the source commit; follow your
organization's policy. Do not disable Windows security features to run this
installer. Real-application capture, private browsing, notifications, physical
input and every Windows configuration are not verified.

Installer generated with Inno Setup: https://jrsoftware.org/
