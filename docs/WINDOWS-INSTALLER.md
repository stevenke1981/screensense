# Windows installer preview

The `feature/slint-gnome-gui` branch builds an unsigned Windows x64 Inno Setup
preview containing matching `screensense.exe` and `screensense-gui.exe`. It is a
CI artifact, not a new GitHub Release. The published v0.1.0 archives remain CLI
packages. Windows 10 or later on x64 is required; ARM64 is not declared supported.

The installer defaults to `%LOCALAPPDATA%\Programs\ScreenSense` with
`PrivilegesRequired=lowest`. It installs for the current user, offers an optional
desktop shortcut, and registers the standard per-user uninstaller. It does not
launch either application, start recording or a daemon, install a service, enable
autostart, modify PATH or shell profiles, or configure persistent MCP. Close your
own ScreenSense processes before upgrading; setup does not force-close them.

Configuration and data live outside the installation directory. Upgrade and
uninstall preserve them. Uninstall removes the files that setup installed and
leaves unrelated files in the installation directory. The installer cannot erase
backups or guarantee forensic deletion. The GUI's explicit history controls are
separate from uninstall.

## Contents and verification

When the installer job finishes successfully, its workflow artifact is named `windows-installer`. The artifact contains the Inno Setup `.exe`, a matching `.sha256` file, and `.build.json` provenance. CI artifacts are temporary workflow outputs, not GitHub Release assets. The separate `windows-installer-fixtures` artifact is for CI upgrade testing and is not the preview installer for users. The workflow also uploads `windows-installer-verification` test evidence.

Each artifact contains the setup EXE, a SHA-256 file and `.build.json` provenance.
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
AppId, installs into a new fixture directory, runs the installed CLI and GUI,
seeds synthetic SQLite history, upgrades the fixture version, and uninstalls.
It checks retention of configuration, frame/insight rows, pause/stop controls and
an unrelated file. The version transition uses identical-source binaries; it does
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
commit. CI builds and packages both binaries in the same checked-out job.

Windows may show an unknown-publisher or reputation warning for this unsigned
preview. Compare its SHA-256 with the matching artifact provenance and follow
your organization's policy. This workflow does not disable or bypass protection.

The repeatable fixture test is `scripts/verify-windows-installer.ps1`. It requires
fixture-only installers and matching `.build.json` files, refuses an existing
fixture registration, uses a new directory and retains its logs. It never points
at the normal ScreenSense AppId or normal user data. Any remaining failed fixture
is named in the error instead of force-killing a process or recursively deleting
its directory.
