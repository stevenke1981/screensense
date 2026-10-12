ScreenSense 0.1.0 GUI preview for Windows x64

This installer contains matching CLI and GUI binaries from the development
branch. The published v0.1.0 release archives contain only the CLI. This
preview installer is not a new GitHub Release and has not been merged to main.
Installer validation is still pending; this preview note does not confirm that
install, upgrade, or uninstall checks have passed.

Installation is per user and does not request administrator privileges. It
does not start ScreenSense, recording, a daemon or a model connection. It does
not add autostart, shell hooks, PATH entries, services or persistent MCP.

Start ScreenSense from the Start Menu or run screensense-gui.exe. The CLI is
screensense.exe. Start recording only when you explicitly choose to use the
existing CLI workflow and have reviewed your privacy configuration.

Uninstall removes installed program files and shortcuts. It preserves user
configuration and recorded data. Default data: %LOCALAPPDATA%\screensense.
Default configuration: %APPDATA%\screensense\config.toml. Uninstall is not a
data wipe. Close your own ScreenSense applications before an upgrade or
uninstall; this installer does not force-close processes.

GUI and Slint: GPL-3.0-only. CLI/core/platform: MIT. License texts and notices
are in the licenses directory. Corresponding project source is in source.tar
and at the commit URL recorded in BUILD-INFO.json. Locked dependency sources
are available from the upstream locations recorded in Cargo.lock.

The installer and executables are unsigned. Windows may show an unknown
publisher or reputation warning. Verify the supplied SHA256SUMS and source
commit; follow your organization's policy. Do not disable Windows security
features to run this preview. Real-application capture, private browsing,
notifications, physical input and every Windows configuration are not verified.

Installer generated with Inno Setup: https://jrsoftware.org/
