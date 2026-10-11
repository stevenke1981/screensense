# ScreenSense — working notes for Claude Code

Local-first screen context for AI coding agents: one Rust binary (`screensense`)
that records a privacy-filtered history of the foreground window and serves it
to agents through a prompt hook and a stdio MCP server. Read `docs/DESIGN.md`
first; it is the source of truth for behaviour.

## Layout

- `crates/core` — all policy, no OS code: `config`, `privacy`, `purge`, `store`
  (SQLite), `detect`, `gate`, `budget`, `digest`, `mcp`, `integrations`,
  `shell`, `analyzer`.
- `crates/platform` — `Platform` / `OcrEngine` traits; `win*.rs` (Windows),
  `x11.rs` + `tesseract.rs` (Linux), `null.rs`. No C dependencies.
- `crates/cli` — the binary: `app.rs`, `daemon.rs`, `pipeline.rs`, `imaging.rs`,
  `main.rs`.

## Commands

```
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo clippy -p screensense-platform --target x86_64-pc-windows-msvc --tests -- -D warnings   # from Linux
# real X11 end-to-end (Linux, needs xvfb + tesseract-ocr):
cargo build -p screensense-platform --example demo_window
SCREENSENSE_X11_TEST=1 xvfb-run -a -s "-screen 0 1280x800x24" cargo test -p screensense-platform --test x11_e2e -- --nocapture
```

## Rules that must not be broken

- Raw (unredacted) OCR text and raw pixels live only in memory inside the
  capture pipeline. Only redacted text and pixel-redacted JPEGs reach disk.
- Nothing screen-derived is logged, and nothing screen-derived appears outside
  the `<<<DATA …>>>` fence in any output handed to a model.
- Unknown means "do not record": no OCR engine → no capture; unknown browser
  private-mode state → title only; a row with an unknown tier reads as Hidden.
- User privacy rules are additive; nothing in a config file can disable a
  built-in rule.
- `hook` and `record` always exit 0 and never print anything except (for
  `hook`) one line of JSON.
- No `unwrap()`/`expect()` outside tests; no `unsafe` outside `crates/platform/src/win*.rs`.

## Verification status

- Linux/X11: exercised end to end under Xvfb by the tests.
- Windows backend (`win.rs`, `win_ocr.rs`, `win_uia.rs`, `win_misc.rs`):
  type-checked and linted for `x86_64-pc-windows-msvc`, **never executed**.
  On a Windows machine, start with `cargo test --workspace` and
  `cargo run -p screensense -- doctor --capture`; the list of unverified
  assumptions is in `docs/WINDOWS-VERIFICATION.md`.
- PowerShell shell integration (`shell.rs`): written, never run.
