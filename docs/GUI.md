# Desktop GUI

ScreenSense includes an optional Slint desktop control window in `crates/gui`. It provides local status and privacy controls while preserving the existing CLI and MCP workflows. It is not a recorder front end and it does not replace the daemon.

## Build and launch

Use the repository's Rust 1.89 toolchain and the locked dependency graph:

```sh
cargo run --locked -j1 -p screensense-gui
cargo run --locked -j1 -p screensense-gui -- --lang en
```

Traditional Chinese is the default. The second command selects English for that launch. The GUI uses Slint 1.15.1; the project workspace MSRV remains Rust 1.89. On Debian/Ubuntu, install the GUI build, X11 window-system, and CJK font dependencies with `sudo apt-get install libfontconfig1-dev libxkbcommon-x11-0 fonts-noto-cjk`. `libfontconfig1-dev` supplies the fontconfig development metadata needed by the build; `libxkbcommon-x11-0` is needed by the Linux GUI runtime; `fonts-noto-cjk` provides Traditional Chinese glyph fallback.

When the window opens, the service reads the local config, opens the local store, reads control/heartbeat state, and queries platform capability flags. It does not start a daemon, inspect the foreground window, capture pixels, request OCR, call an analyzer/model, or manage a network connection. A configured analyzer endpoint is shown as configuration only. The GUI itself is subject to ScreenSense's self-application title-only privacy rule.

The GUI does not install or start a daemon, configure an agent, add shell hooks, or enable autostart. Use the existing CLI explicitly for those workflows. Saving configuration can affect an already-running daemon; resuming can allow an already-running daemon to continue according to the saved configuration.

## The five pages

1. **Overview** shows whether privacy settings are usable, daemon/stop/pause state, capture and analysis configuration, backend capability, and real history counts. It has explicit Pause and Clear pause controls. It never displays captured text or image content. Invalid config/privacy rules block capture and analysis and hide counts. If stored history has a different privacy-rule hash, the snapshot reports `policy_pending`, hides history counts and cached heartbeat notes, but still reports daemon state accurately. Refresh does not purge this data.
2. **Capture & analysis** displays backend and capture capability, the OCR engine reported by the sensor, image-retention configuration, and analyzer model configuration. It is a configuration/status view, not a capture test, and does not show screen contents.
3. **Preferences** edits the actual TOML file. Validate checks the draft without writing it; Save opens a consequence/activation confirmation and writes only a valid, usable configuration. The editor keeps invalid source text editable. It detects unreadable or externally changed config, refuses to overwrite a symlink, and requires reload before saving after an external change. A language-only or other non-privacy save does not purge history or insights. The save compares the edited privacy policy with the current config policy; only a confirmed privacy-policy change triggers cleanup, and only after atomic config replacement succeeds. If the previous TOML cannot be parsed, save does not infer a policy change or automatically delete data; pending cleanup remains a separate, confirmed action.
4. **Diagnostics** reports version, sensor heartbeat/process status, stop request, configuration validity, local paths, and known errors. Its checks do not inspect the foreground window, capture a screen, or probe an analyzer.
5. **Stored data** shows aggregate counts and a confirmed **Clear all history** action. When `policy_pending` is true, it also offers **Review cleanup**. That confirmation names conflicting history and all stored analysis results for permanent deletion; explicit confirmation is required. The cleanup preserves pause and stop controls. **Clear all history** remains a separate action that also retains configuration and sensitive-window marks. Neither action promises forensic erasure from backups or storage media.

The overview is refreshed every two seconds while the window is idle. Snapshot reads compare the current privacy rules with the stored rules hash; they never run cleanup. When cleanup is pending, history counts and cached heartbeat notes are withheld while daemon, pause, and stop state remain independently visible. Cleanup requires the explicit **Review cleanup** confirmation. Database and filesystem operations run on one worker thread. Each user action receives a monotonically increasing epoch; a reply from an older request is discarded so it cannot overwrite newer UI state. Failed or unusable snapshots clear displayed counts rather than leaving older counts visible. Save uses a SQLite immediate-transaction privacy fence to advance the work generation without deleting rows; a failed config-file replacement rolls back the fence and preserves the previous history. A successful privacy-changing save replaces the config first, then performs the confirmed cleanup. If cleanup fails, views remain hidden until cleanup succeeds.

## License scope

`crates/gui` is GPL-3.0-only, with its license text in [`crates/gui/LICENSE`](../crates/gui/LICENSE). The existing `crates/cli`, `crates/core`, and `crates/platform` packages remain MIT licensed. The GUI pins Slint and `slint-build` to 1.15.1 and uses Slint under GPL-3.0-only. Upstream metadata offers GPL-3.0-only, royalty-free, and commercial license paths; this GUI selects GPL-3.0-only and does not accept Slint royalty-free or commercial license terms. See [THIRD-PARTY-NOTICES.md](../THIRD-PARTY-NOTICES.md), [Slint 1.15.1 package metadata](https://raw.githubusercontent.com/slint-ui/slint/v1.15.1/Cargo.toml), and [Slint licensing options](https://slint.dev/pricing).

## CI verification steps

The [CI workflow](../.github/workflows/ci.yml) defines these checks for the GUI:

- **Linux GUI job:** installs `tesseract-ocr`, `xvfb`, `fonts-noto-cjk`, `libfontconfig1-dev`, and `libxkbcommon-x11-0`; runs formatting, workspace clippy/tests, builds `screensense-gui`, then runs `--verify-native` under Xvfb with `SCREENSENSE_NO_SCREEN=1`. It uploads the generated PNGs and `verification.json`.
- **Windows job:** runs workspace clippy/tests, builds `screensense-gui`, then runs `--verify-ui` with `SCREENSENSE_NO_SCREEN=1` and uploads its rendered PNGs and `verification.json`.
- **MSRV job:** uses Rust 1.89, installs `libfontconfig1-dev`, and runs `cargo check --workspace --all-targets --locked -j1`.

Earlier runs [38162265291](https://github.com/stevenke1981/screensense/actions/runs/38162265291) and [38163982183](https://github.com/stevenke1981/screensense/actions/runs/38163982183) exposed a missing Linux `libxkbcommon-x11-0` dependency and GUI privacy/focus regressions. Candidate `747cc1525108a8fa744ac29f5765ff24de56de49` passed [CI run 38272721986](https://github.com/stevenke1981/screensense/actions/runs/38272721986): Windows, Linux, and Rust 1.89 MSRV jobs all succeeded. Linux Xvfb verification included the GUI plus backend, daemon, and doctor end-to-end checks. The downloaded `windows-gui-tested` artifact's `BUILD-INFO` and `SHA256SUMS` were verified; its executable SHA-256 was `2268f4e7993a3b668cfc2c1f44cab8eaa1edf0b7967a8ff098b7ceab564cf6dd`.

Follow-up commits `420a24c` (draft-baseline tracking and header layout) and `bbdae58` (accessible editor value binding) also passed CI. Commit `970d229`'s one-time Cancel-focus restoration passed [CI run 38281939681](https://github.com/stevenke1981/screensense/actions/runs/38281939681). A controlled native UI Automation fixture, restricted to ScreenSense's own PID/HWND, passed pause 60 minutes followed by a 5-minute request, language-only Save preserving config and insight, read-only modal behavior, confirmed cleanup and wipe, and clean worker exit. A separate real-OS foreground-focus check could not be confirmed because Windows denied foreground activation; no focus bypass was used.

The latest tested commit, `65cdbbc`, failed [CI run 38286292420](https://github.com/stevenke1981/screensense/actions/runs/38286292420) on both Windows software verification and Linux Xvfb: the second character entered in the configuration editor was lost. The current working-tree fix clears the one-time focus-return token before the Confirm action while preserving the Cancel path; it is not yet committed or tested. Do not treat the earlier green results as verification of this latest fix.

## Verification workflow

Verification is isolated and synthetic. Both verification commands require `SCREENSENSE_NO_SCREEN=1` and a brand-new output directory; the program refuses an existing output directory. They never open the user's normal data store. The verification fixture inserts synthetic data, exercises the same internal action dispatcher and UI result sink, drives Slint key events, and writes rendered PNGs plus `verification.json`. This is internal UI input, not physical mouse or keyboard automation.

On PowerShell:

```powershell
$env:SCREENSENSE_NO_SCREEN = "1"
cargo run --locked -j1 -p screensense-gui -- --verify-ui .\gui-fixture
```

For a native Winit operating-system window, still using Slint software rendering:

```powershell
$env:SCREENSENSE_NO_SCREEN = "1"
cargo run --locked -j1 -p screensense-gui -- --verify-native .\gui-native-fixture
```

The output directory must not exist yet. `--verify-ui` uses Slint's software renderer without a native OS window; `--verify-native` opens the app in a native Winit window and still uses the software renderer. It displays the app's own GUI window only and does not capture the desktop. On shells other than PowerShell, set `SCREENSENSE_NO_SCREEN=1` using that shell's environment-variable syntax before running the same Cargo arguments.

Candidate `747cc15`'s scripted `--verify-native` checks exited 0. They exercised Chinese and English rendering, synthetic status, pause extension behavior, stale-response rejection, pending cleanup without deletion, confirmed cleanup, invalid-regex count clearing, non-privacy save preservation, config replacement failure rollback, wipe preserving pause and stop, modal Tab/Shift+Tab and Escape behavior, focus return after Cancel, and a 1024×600 render. They use the internal dispatcher and Slint key events, not physical input or general desktop UI Automation. Commits `420a24c`, `bbdae58`, and `970d229` subsequently passed their stated CI checks, but the latest `65cdbbc` regression and its untested working-tree fix are described above.
