# Windows installer

From v0.2.0 the GitHub Release carries an unsigned Windows x64 Inno Setup
installer, `ScreenSense-<version>-<commit>-windows-x64-setup.exe`, with matching
`screensense.exe` (CLI, MIT) and `screensense-gui.exe` (GUI, GPL-3.0-only). The
release also attaches `<installer>.sha256`, `<installer>.build.json` provenance,
and lists the installer in `SHA256SUMS`. The `.zip` / `.tar.gz` archives remain
CLI-only packages. Windows 10 or later on x64 is required; ARM64 is not declared
supported. The installer is built and tested by the same CI run that validates
the tagged commit (artifact `windows-installer`).

## Install

The installer is **unsigned**. Windows SmartScreen or a reputation prompt may
show "unknown publisher"; choose "More info" only after you have checked the
SHA-256 below and your own policy allows it. Nothing here disables or bypasses
Windows protection.

Verify the download in PowerShell, then compare with the `.sha256` file (and the
release's `SHA256SUMS`):

```powershell
(Get-FileHash .\ScreenSense-0.2.0-<commit>-windows-x64-setup.exe -Algorithm SHA256).Hash.ToLowerInvariant()
Get-Content .\ScreenSense-0.2.0-<commit>-windows-x64-setup.sha256
```

Installation is per user (`PrivilegesRequired=lowest`, no administrator prompt)
into `%LOCALAPPDATA%\Programs\ScreenSense` by default, with a Start Menu entry
and the standard per-user uninstaller. Wizard languages: English (the official
Inno Setup distribution has no Traditional Chinese message file, so none is
bundled; the GUI itself has its own language setting).

Optional wizard items, all off by default except where noted:

- **Create a desktop shortcut** (unchecked).
- **Start ScreenSense in the notification area when I sign in** (unchecked).
  Writes one value, `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`
  `ScreenSenseGui` = `"<install dir>\screensense-gui.exe" --minimized`. This only
  opens the GUI as a tray icon. It never starts the recording daemon; recording
  remains a separate, explicit step. Uninstall removes the value. The name is
  shared by every ScreenSense install of the current user.
- **Launch ScreenSense** on the last page (checked, not run in silent installs).
  Opening the GUI does not start recording.

The setup and the uninstaller use the GUI's named mutex
(`ScreenSenseGui-917fbe92-1d22-4f87-af47-e143e7eca3dd`, held while the GUI runs,
including when it sits in the tray). If it is held, they ask you to close
ScreenSense first (its window or notification-area icon) and then retry; a silent run exits with a
non-zero code instead. Setup never force-closes applications
(`CloseApplications=no`). Close a separately started CLI daemon yourself too.

It does not start recording or a daemon, install a service, enable daemon
autostart, modify PATH or shell profiles, or configure MCP. Exactly what setup
manages: program files, shortcuts, the uninstall registration and the optional
Run value above.

Configuration and data live outside the installation directory
(`%APPDATA%\screensense\config.toml`, `%LOCALAPPDATA%\screensense`). Upgrade and
uninstall keep them. Uninstall removes the files setup installed, the shortcuts,
the uninstall registration and the Run value, and leaves unrelated files in the
installation directory. It is not a data wipe and cannot erase backups; the
GUI's explicit history controls (or `screensense purge`) are separate.

## Contents and verification

The installed `BUILD-INFO.json` records the source commit and both binary hashes.
Both binaries are built in release mode with a static CRT; packaging rejects
regular imports of the Visual C++ runtime DLLs instead of installing an elevated
runtime bootstrapper. This import check is not a claim that every Windows system
dependency or device driver has been tested.

The package includes the GUI GPL-3.0 text, core MIT text, third-party notices,
original dependency license files, the exact ScreenSense source archive with
Cargo.lock, and a ZIP of the original dependency sources. The ZIP includes
build/development dependencies and is not an exact linked-code inventory or a
configured Cargo vendor directory. Build instructions below use the locked
registry versions. Slint is used under GPL-3.0-only; no commercial or royalty-free
Slint license is accepted by this workflow.

Where a published crate omits its upstream notice, the collection includes files
from its exact recorded upstream revision and verifies their tracked SHA-256.
The index identifies the single supplemental SPDX MIT reference for the
`simd_helpers` build helper, whose upstream manifest declares MIT but supplies no
complete original license file. This supplement does not invent an author's
copyright notice.

CI verifies the release GUI through internal Slint key events with screen
observation disabled. It then builds two installers with a new, fixture-only
AppId, installs into a new fixture directory with the default tasks, checks the
installed icon and that no sign-in Run value was created, runs the installed CLI
and GUI, seeds synthetic SQLite history, upgrades the fixture version, and
uninstalls. It checks retention of configuration, frame/insight rows, pause/stop
controls and an unrelated file. The optional sign-in task is not exercised in CI
because its registry value name is shared with real installs; it was checked by
hand with a fixture AppId (value written, removed on uninstall). The version transition uses identical-source binaries; it does
not test migration from historical database schemas. This coverage does not
establish physical keyboard/mouse focus behavior, real-app capture, private
browsing, notification delivery or every Windows configuration.

## Build an installer

Use Rust 1.89 or later and an official Inno Setup 6.3 or later installation.
The Windows CI runner provides Inno Setup; the workflow does not install it on
the user's machine. Obtain local tooling only from
[Inno Setup's official download page](https://jrsoftware.org/isdl.php) and verify
its publisher signature using the
[official verification instructions](https://jrsoftware.org/isdl-verify.php).
The repository does not change Windows security or execution policy.

Commit all source changes first. From the repository root in PowerShell:

```powershell
$sha = (git rev-parse HEAD).Trim()
$env:CARGO_TARGET_DIR = 'target/installer'
$env:RUSTFLAGS = '-C target-feature=+crt-static'
cargo build --release --locked -j1 --target x86_64-pc-windows-msvc -p screensense -p screensense-gui
Remove-Item Env:RUSTFLAGS
$assets = Join-Path $env:TEMP ('screensense-dependencies-'+[guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $assets | Out-Null
cargo metadata --locked --format-version 1 --filter-platform x86_64-pc-windows-msvc > "$assets/metadata.json"
python scripts/collect_licenses.py "$assets/metadata.json" "$assets/LICENSES"
python scripts/collect_dependency_sources.py "$assets/metadata.json" "$assets/dependency-sources.zip"
$output = Join-Path $env:TEMP ('screensense-installer-'+[guid]::NewGuid().ToString('N'))
./scripts/package-windows.ps1 -BinaryDir 'target/installer/x86_64-pc-windows-msvc/release' -DependencyDir $assets -OutputDir $output -ExpectedCommit $sha
```

Check each command's exit status before proceeding. Keep build output outside
tracked files. The script verifies a clean source tree and matching commit, but cannot
independently prove an arbitrary externally supplied binary was built from that
commit. CI either builds both binaries in the checked-out job or verifies the
recovery path described below.

For packaging-only corrections, CI may reuse its verified release payload after
checking that Cargo manifests/lock, toolchain configuration, `.cargo` and the
complete `crates` tree are identical. It verifies the recovery artifact's pinned
SHA-256 and runs the recovered CLI/GUI checks again. `binary_source_commit` records
the original compiler commit separately from the current packaging `commit`.
Runtime source changes require a fresh build.

The script embeds `packaging/windows/screensense.ico` (setup icon, shortcuts) and
defaults to version `0.2.0` / `0.2.0.0`; pass `-Version` and `-VersionNumber` for
another release. A normal build uses the production AppId; test installs must
use a fixture AppId (see below).

The repeatable fixture test is `scripts/verify-windows-installer.ps1`. It requires
fixture-only installers and matching `.build.json` files, refuses an existing
fixture registration, uses a new directory and retains its logs. It never points
at the normal ScreenSense AppId or normal user data. Any remaining failed fixture
is named in the error instead of force-killing a process or recursively deleting
its directory.
