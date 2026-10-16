# ScreenSense v0.2.0

ScreenSense is an early, opt-in local screen context tool for coding workflows. It records a privacy-filtered foreground-window history locally and can return a short digest through a user-configured hook or a seven-tool stdio MCP server. Recording, agent setup, autostart and shell command capture are separate user-controlled steps.

## What changed

- **Desktop window (`screensense-gui`, GPL-3.0-only).** A compact Slint window (400×660, works at 360×560) with its own title bar, original SVG icons and light/dark themes. It shows the sensor state, stored-history counts and five pages: overview, capture, settings (the real TOML file, validated, saved only after confirmation), diagnostics and stored data (confirmed wipe and privacy cleanup).
- **Notification area.** Minimise sends the window to the tray; the tray icon's colour follows the sensor state and its menu offers show, pause 5/60 minutes, clear pause (confirmed in the window) and quit. `--minimized` starts in the tray. A second start brings the running window back instead of opening another.
- **Opening the window starts nothing:** no daemon, capture, OCR, model request or autostart. Start the sensor with `screensense daemon start`.
- **Windows installer** (Inno Setup, per-user, unsigned) with the CLI and the GUI; see below.
- GUI now on Slint 1.18.1 (needs Rust 1.92 to build; the CLI, core and platform crates keep Rust 1.89). Dependency notices were re-collected for the new dependency graph.
- The CLI, MCP server and daemon behave as in v0.1.0.

## Verification

Before tagging, on the release branch: workspace format, clippy and tests on Windows; Rust 1.89 (all but the GUI) and 1.92 (GUI) checks; the GUI's isolated `--verify-ui` keyboard/confirmation checks at 400×660 and 360×560; a native run on Windows 10 covering the tray icon, minimise to tray, restore from the tray, single instance and quit; dependency notice collection for Windows and Linux; and a local fixture install/upgrade/uninstall of the installer. The release workflow then reruns CI on the tagged commit, builds the installer there, and smoke-tests the archives before publishing.

## Requirements and limits

- **Windows 10/11 x64 (ZIP):** the CLI in the ZIP requires the Microsoft Visual C++ v14 x64 runtime (`VCRUNTIME140.dll`). If missing, install the [official x64 Redistributable](https://aka.ms/vc14/vc_redist.x64.exe); see Microsoft's [supported versions](https://learn.microsoft.com/en-us/cpp/windows/latest-supported-vc-redist?view=msvc-170). The installer's binaries are built with a static CRT and need no runtime. Windows OCR also requires an installed OCR language. Archives and installer are unsigned.
- **Linux x86_64 / X11:** requires a compatible glibc runtime, an X11 session with `DISPLAY`, and `tesseract` with the desired language data for OCR. Wayland screen observation is unsupported.
- macOS screen observation is unsupported. Linux/X11 private-browsing state is unknown, so browsers are title-only by default.
- Each release archive includes `LICENSES/index.json` and available original `LICENSE`, `NOTICE`, `COPYING` and `COPYRIGHT` texts collected for that archive target dependency set, including build and development dependencies. This is not an exact runtime-linked dependency inventory.
- Ordinary Windows applications, private-browser UI Automation, notifications, autostart registry execution, PowerShell hooks, and live agent sessions have not been verified.
- SQLite and JPEG data are not encrypted. Regex and OCR privacy filters are best effort, not security guarantees. SQLite/FTS/WAL cleanup is not forensic erasure from SSDs or backups. Sensitive-window marks do not expire and have no UI clear control. Quoted executable paths remain a PowerShell hook syntax limitation.

Before publication, the release workflow requires same-commit CI and Windows/Linux archive smoke checks. The final commit SHA is recorded in `BUILD-INFO.txt`; review the final release body and evidence against that commit.

## Windows installer

The release attaches an unsigned per-user Inno Setup installer (`ScreenSense-<version>-<commit>-windows-x64-setup.exe`) that installs the CLI and the GUI together, with its `.sha256` and `.build.json` provenance; it is also listed in `SHA256SUMS`. Windows SmartScreen will warn about an unknown publisher; verify the SHA-256 first. Optional wizard items (both off by default except the post-install launch): a desktop shortcut and starting the GUI in the notification area at sign-in, which opens only the tray icon and never starts recording. Setup and uninstall ask you to close a running GUI and never force-close anything. Upgrade and uninstall keep configuration and history. CI installs, upgrades and uninstalls it only under a fixture AppId in an isolated directory; real-app capture and physical input on the installed GUI are not verified. See `docs/WINDOWS-INSTALLER.md`.
