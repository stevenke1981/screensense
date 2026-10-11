# ScreenSense — working notes for Claude Code

Local-first screen context for AI coding agents: one Rust binary (`screensense`)
that records a privacy-filtered history of the foreground window and serves it
to agents through a prompt hook and a stdio MCP server. `docs/DESIGN.md`
explains how it is put together and why; if it and the code disagree, the code
is right and the document needs fixing.

## Layout

- `crates/core` — all policy, no OS code: `config`, `privacy`, `purge`, `store`
  (SQLite), `detect`, `gate`, `budget`, `digest`, `mcp`, `integrations`,
  `shell`, `analyzer`, plus `i18n`, `paths`, `types`, `error`.
- `crates/platform` — `Platform` / `OcrEngine` traits; `win*.rs` (Windows),
  `x11.rs` + `tesseract.rs` (Linux), `null.rs`, `autostart.rs`, `process.rs`.
  No C dependencies. `examples/demo_window.rs` is the window the X11 tests
  look at.
- `crates/cli` — the binary (clap). `main.rs` (argument parsing, dispatch, exit
  codes), `app.rs` (data directory, config, store, privacy rules for one
  invocation), `commands.rs` (every command except the daemon and doctor),
  `daemon.rs` (sensor, capture and housekeeping threads, config hot-reload),
  `pipeline.rs` (one look: capture, OCR, content checks, redaction, storage),
  `imaging.rs` (thumbnail and image redaction), `doctor.rs`. The tests are in
  `tests/cli.rs` and in the modules.
- `record` is a hidden command (not in `--help`); the shell snippets call it.

## Commands

```
cargo test --workspace
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo clippy -p screensense-platform --target x86_64-pc-windows-msvc --tests -- -D warnings   # from Linux
```

The Windows cross-lint covers the platform crate only. The core and cli crates
use a bundled SQLite that needs a C toolchain for the MSVC target, which a
Linux host does not have, so those two are checked only by the Windows CI job
once it has run.

Three tests need a real X server (Linux, needs `xvfb` and `tesseract-ocr`).
`cargo test --workspace` lets two of them return early without
`SCREENSENSE_X11_TEST=1`, and the third skips itself when Xvfb or the demo
window is missing, so a plain run does not prove them. To run all three:

```
cargo build -p screensense-platform --example demo_window
SCREENSENSE_X11_TEST=1 xvfb-run -a -s "-screen 0 1280x800x24" cargo test -p screensense-platform --test x11_e2e -- --nocapture --test-threads=1
SCREENSENSE_X11_TEST=1 xvfb-run -a -s "-screen 0 1280x800x24" cargo test -p screensense daemon::tests::x11_end_to_end -- --nocapture --test-threads=1
xvfb-run -a -s "-screen 0 1280x800x24" cargo test -p screensense --test cli x11:: -- --nocapture --test-threads=1
```

`.github/workflows/ci.yml` runs these in the same order and fails the step if a
test is reported as skipped.

## Rules that must not be broken

- Raw (unredacted) OCR text and raw pixels live only in memory inside the
  capture pipeline. Only redacted text and pixel-redacted JPEGs reach the data
  directory. One exception: on Linux the captured window is written
  unredacted to a `0600` temporary PPM so that `tesseract` can read it, and
  deleted right after (`crates/platform/src/tesseract.rs`). Keep that file
  short-lived and private.
- Nothing screen-derived is logged (`daemon.log`, `hook-errors.log`), and
  nothing screen-derived appears outside the `<<<DATA …>>>` fence in any output
  handed to a model.
- Unknown means "do not record": no OCR engine → no capture; unknown browser
  private-mode state → title only; a row with an unknown tier reads as Hidden.
- User privacy rules are additive; nothing in a config file can disable a
  built-in rule.
- `hook` and `record` always exit 0 and never print anything except (for
  `hook`) one line of JSON.
- No `unwrap()`/`expect()` outside tests; no `unsafe` outside `crates/platform/src/win*.rs`.

## Verification status

- Linux/X11: the daemon, the platform backend and `doctor --capture` are
  exercised end to end under Xvfb by the three tests above. The digest, MCP
  and CLI views are tested on stores the tests fill by hand, not on rows an
  Xvfb daemon wrote.
- Windows backend (`win.rs`, `win_ocr.rs`, `win_uia.rs`, `win_misc.rs`):
  type-checked and linted for `x86_64-pc-windows-msvc`, **never executed**.
  On a Windows machine, start with `cargo test --workspace` and
  `cargo run -p screensense -- doctor --capture`; the list of unverified
  assumptions is in `docs/WINDOWS-VERIFICATION.md`.
- PowerShell shell integration (`shell.rs`): written, never run.
- `.github/workflows/ci.yml` and `release.yml`: written, never run on GitHub.
