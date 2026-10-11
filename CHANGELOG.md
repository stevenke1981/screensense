# Changelog

All notable changes to this project are documented in this file. The format
follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the
project uses [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## 0.1.0 — unreleased

First version. The Linux/X11 backend is exercised against a real X server in a
virtual display by the test suite. The Windows backend and the PowerShell
shell integration compile but have not been run on real hardware; see
`docs/WINDOWS-VERIFICATION.md`.

### Added

- One binary, `screensense`, in a three-crate workspace: `screensense-core`
  (policy and storage, no OS code), `screensense-platform` (OS backends) and
  the `screensense` CLI.
- Background daemon that tracks the foreground window, looks at it with an
  adaptive interval (3 s backing off to 30 s while nothing changes), runs OCR
  and stores redacted text and an optional pixel-redacted JPEG.
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
- Retroactive purge: when the rule set changes, stored history is re-checked
  and what would no longer be recorded is deleted, image files included.
- Error detection from screen text and from shell exit codes, deduplicated and
  scored.
- Digest for AI agents inside a random per-call fence, with sanitised content,
  an estimated-token budget and per-conversation delta mode.
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
- CI on Linux and Windows, a release workflow, and `install.sh` /
  `install.ps1`.
