# Desktop GUI

ScreenSense includes an optional Slint desktop control window in `crates/gui`. It provides local status and privacy controls while preserving the existing CLI and MCP workflows. It is not a recorder front end and it does not replace the daemon.

## Build and launch

Use the repository's Rust 1.89 toolchain and the locked dependency graph:

```sh
cargo run --locked -j1 -p screensense-gui
cargo run --locked -j1 -p screensense-gui -- --lang en
```

Traditional Chinese is the default. The second command selects English for that launch. The GUI uses Slint 1.15.1; the project workspace MSRV remains Rust 1.89.

When the window opens, the service reads the local config, opens the local store, reads control/heartbeat state, and queries platform capability flags. It does not start a daemon, inspect the foreground window, capture pixels, request OCR, call an analyzer/model, or manage a network connection. A configured analyzer endpoint is shown as configuration only. The GUI itself is subject to ScreenSense's self-application title-only privacy rule.

The GUI does not install or start a daemon, configure an agent, add shell hooks, or enable autostart. Use the existing CLI explicitly for those workflows. Saving configuration can affect an already-running daemon; resuming can allow an already-running daemon to continue according to the saved configuration.

## The five pages

1. **Overview** shows whether privacy settings are usable, daemon/stop/pause state, capture and analysis configuration, backend capability, and real history counts. It has explicit pause and resume controls. It never displays captured text or image content. When config/privacy rules are invalid, counts are shown as zero and capture/analysis are blocked.
2. **Capture & analysis** displays backend and capture capability, the OCR engine reported by the sensor, image-retention configuration, and analyzer model configuration. It is a configuration/status view, not a capture test, and does not show screen contents.
3. **Preferences** edits the actual TOML file. Validate checks the draft without writing it; Save opens a consequence/activation confirmation and writes only a valid, usable configuration. The editor keeps invalid source text editable. It detects unreadable or externally changed config, refuses to overwrite a symlink, and requires reload before saving after an external change. Saving invalidates privacy state and purges data no longer allowed by the new rules.
4. **Diagnostics** reports version, sensor heartbeat/process status, stop request, configuration validity, local paths, and known errors. Its checks do not inspect the foreground window, capture a screen, or probe an analyzer.
5. **Stored data** shows aggregate counts and a confirmed **Clear all history** action. The wipe removes history and related stored artifacts while retaining configuration, sensitive-window marks, pause, and stop controls. It is irreversible; this action does not promise forensic erasure from backups or storage media.

The overview is refreshed every two seconds while the window is idle. Database and filesystem operations run on one worker thread. Each user action receives a monotonically increasing epoch; a reply from an older request is discarded so it cannot overwrite newer UI state. Failed or unusable snapshots clear displayed counts rather than leaving older counts visible.

## License scope

`crates/gui` is GPL-3.0-only, with its license text in [`crates/gui/LICENSE`](../crates/gui/LICENSE). The existing `crates/cli`, `crates/core`, and `crates/platform` packages remain MIT licensed. The GUI pins Slint and `slint-build` to 1.15.1 and uses Slint under GPL-3.0-only. Upstream offers distinct royalty-free and commercial license options; this GUI selects GPL-3.0-only; this work does not accept royalty-free or commercial terms. See [THIRD-PARTY-NOTICES.md](../THIRD-PARTY-NOTICES.md), [Slint 1.15.1 package metadata](https://raw.githubusercontent.com/slint-ui/slint/v1.15.1/Cargo.toml), and [Slint licensing options](https://slint.dev/pricing).

## Verification workflow

Verification is isolated and synthetic. Both verification commands require `SCREENSENSE_NO_SCREEN=1` and a brand-new output directory; the program refuses an existing output directory. They never open the user's normal data store. The verification fixture inserts synthetic data, exercises the same internal action dispatcher and UI result sink, drives Slint key events, and writes rendered PNGs plus `verification.json`. This is internal UI input, not physical mouse or keyboard automation.

On PowerShell:

```powershell
$env:SCREENSENSE_NO_SCREEN = "1"
cargo run --locked -j1 -p screensense-gui -- --verify-ui .\gui-fixture
```

For a native operating-system window using the same software renderer:

```powershell
$env:SCREENSENSE_NO_SCREEN = "1"
cargo run --locked -j1 -p screensense-gui -- --verify-native .\gui-native-fixture
```

The output directory must not exist yet. The native mode displays the app's own GUI window only; it does not capture the desktop. On shells other than PowerShell, set `SCREENSENSE_NO_SCREEN=1` using that shell's environment-variable syntax before running the same Cargo arguments.

The scripted checks cover Chinese and English rendering, synthetic status, a 60-minute pause followed by a 5-minute extension attempt, stale-response rejection, delete confirmation safety (Return does not confirm; Escape cancels), invalid-regex count clearing, repair/save, wipe preserving pause and stop, and a 1024×600 render. These checks exercise the internal dispatcher and Slint key events; they are not end-to-end physical input tests. At the time of this documentation update, a successful build and visual review have not yet been recorded. Do not treat the commands or listed checks as passed until their run results are available.