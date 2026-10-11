# Changelog

All notable changes to this project are documented in this file. The format
follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the
project uses [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## 0.1.0 — unreleased

First version. Verification status at the time of writing:

- Linux/X11: tests in this repository run the daemon against a real X server in
  a virtual display (Xvfb) and check the stored window span, frame, image and
  error row, a clean stop, and `doctor --capture`. The digest, MCP server and
  CLI views are tested on stores that the tests fill themselves.
- Windows: the backend is type-checked and linted for the Windows target and
  has not been run on real hardware; see `docs/WINDOWS-VERIFICATION.md`.
- PowerShell shell integration: not run.
- The CI and release workflows are written but have not run yet.

### Added

- One binary, `screensense`, in a three-crate workspace: `screensense-core`
  (policy and storage, no OS code), `screensense-platform` (OS backends) and
  the `screensense` CLI.
- Background daemon (`daemon run`, `start`, `stop`, `restart`, `status`) with a
  sensor, a capture and a housekeeping thread. It tracks the foreground window,
  looks at it with an adaptive interval (3 s backing off to 30 s while nothing
  changes), runs OCR and stores redacted text and an optional pixel-redacted
  JPEG. A user with no keyboard or mouse input for 2 minutes counts as idle.
  Without an OCR engine nothing is captured.
- The daemon re-reads the config file about every 10 seconds and applies new
  settings and privacy rules without a restart (a new OCR engine setting needs
  `daemon restart`). `daemon start` writes `daemon.log` in the data directory;
  the log holds no screen text.
- Command line: `status` (exit code 3 when the sensor is not running), `now`,
  `recent`, `errors`, `commands`, `search`, `text`, `shot`, `stream`, `hook`,
  `mcp`, `setup`, `shell-init`, `pause`, `resume`, `purge`, `sweep`, `analyze`,
  `insights`, `config path|show|init|check`, `autostart on|off|status`,
  `doctor [--capture]`, and the hidden `record` command that the shell snippet
  calls after each command. Global options: `--data-dir`, `--config`, `--lang`.
- `doctor --capture` captures the foreground window once and reports how the
  privacy rules classify it and what the OCR read, without storing anything. It
  prints the window title only for windows recorded in full.
- Platform backends: Windows (window capture, `Windows.Media.Ocr`, UI
  Automation private-browsing probe, toast notifications, sign-in autostart),
  Linux/X11 (pure-Rust X11 connection, the `tesseract` command for OCR,
  `notify-send`, XDG autostart) and a null backend for everything else,
  including Wayland sessions without XWayland.
- SQLite storage in WAL mode with FTS5 trigram search over recognised text,
  and retention for images, frame text, timeline and insights.
- Privacy rules: three tiers (full, title only, hidden); built-in app and
  title lists; content checks that drop a frame (API keys and tokens, private
  keys, JWTs, card numbers with Luhn, ROC national IDs with checksum,
  sensitive keywords); soft redaction of e-mail addresses, phone numbers,
  database URLs and secret command-line flags in both text and pixels;
  additive user rules; unknown browser private-mode state means title only.
- Retroactive purge: when the rule set changes (at daemon start, within about
  10 seconds of a config change, or on `screensense purge`), stored history is
  re-checked and what would no longer be recorded is deleted, image files
  included. A window found sensitive from its content is remembered across
  restarts; `purge --all` keeps those marks.
- Error detection from screen text and from shell exit codes, deduplicated and
  scored.
- Digest for AI agents inside a random per-call fence, with sanitised content,
  an estimated-token budget and per-conversation delta mode (after the first
  full block only what changed is sent, nothing when nothing changed).
- Stdio MCP server (`screensense mcp`) with the tools `screen_now`,
  `screen_timeline`, `screen_errors`, `screen_search`, `screen_text`,
  `screen_commands`, `screen_pause` and `screen_resume`.
- `screensense setup` for Claude Code (prompt hook and `/screen` skill), Codex
  CLI and Gemini CLI (prompt hooks) and OpenCode (MCP entry), with backups and
  atomic writes.
- Opt-in shell integration for bash, zsh, fish and PowerShell that records
  each command with exit code, directory and duration.
- Optional analyzer that sends recent context to a model server on the same
  machine (Ollama or OpenAI-compatible). Off by default; loopback only unless
  `analyzer.allow_remote` is set.
- English and Traditional Chinese output.
- A CI workflow (Linux: format, clippy, tests and the three X11 tests under
  Xvfb; Windows: clippy, tests and smoke steps; a minimum-Rust-version check),
  a release workflow that builds archives with a `SHA256SUMS` file, and
  `install.sh` / `install.ps1`. None of them has run yet.
