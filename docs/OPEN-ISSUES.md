# Open issues before 0.1.0

Found by an independent review of the code on 2026-10-11. None is fixed yet.
Each needs a test that fails before the fix. Ordered by priority.

## Privacy

1. **A config file that fails to parse drops the user's privacy rules.**
   `Config::load` falls back to defaults, so `hidden_apps` and friends vanish
   while the daemon keeps recording (`crates/core/src/config.rs`, `daemon.rs`
   hot-reload). Fix: refuse to start on an invalid file; on reload keep the last
   good configuration; `record` stores nothing while the file is invalid.
2. **`pause` does not stop a capture already in flight.** The capturer only
   sees the pause on the sensor's next tick (`crates/cli/src/pipeline.rs`).
   Fix: re-read `store.control()` right before writing the image and the row.
3. **A config change takes up to ~10 s to apply** (`CONFIG_CHECK_S` in
   `daemon.rs`). Lower to 2 s.
4. **Low-confidence OCR words stay visible in the stored image.** Tesseract
   words under confidence 30 are dropped from the text and from the boxes, so
   they are neither checked nor blacked out (`crates/platform/src/tesseract.rs`,
   `MIN_CONF`). Fix: keep their boxes and always black them out.
5. **Unredacted pixels touch disk on Linux.** The captured window is written
   to a `0600` temporary PPM for tesseract. Fix: pipe it on stdin
   (`tesseract stdin stdout …`).
6. **Credentials on command lines are stored by the shell integration.**
   `Privacy::clean_command` keeps `curl -u user:pass`, `mysql -pSECRET`,
   `docker login -p …`, `sshpass -p …`, `openssl … -k/-pass`, `redis-cli -a`,
   `7z -pX`, `zip -P`, `echo pw | sudo -S`. Fix: a commands-only table of
   tool-specific patterns that drops the whole command. Mind the negatives:
   `mkdir -p`, `ssh -p 2222`, `git push -u`, `cargo test -p x`, `unzip -p`.
7. **`secret_assignment` skips values that merely start with a code-ish word**
   (`password = importantSecret123`). Make the prefix check whole-identifier.
   Known accepted gaps: values under 8 characters, letters-only values.
8. **Secrets soft-wrapped across lines are not detected.** Also run the hard
   rules over the text with line breaks removed in `Privacy::check_text`.
9. **Deleted rows linger in the database file.** Add
   `PRAGMA secure_delete = ON` and checkpoint the WAL after a purge
   (`crates/core/src/store.rs`); check the FTS shadow tables too.
10. **An agent can end a privacy pause.** Remove the `screen_resume` MCP tool;
    only the user resumes (`crates/core/src/mcp.rs`, docs, skill).
11. **An unnamed foreground process classifies as fully recordable.** Windows
    reports `unknown` for elevated windows. Classify an empty or `unknown` app
    as title-only (`Privacy::classify`).
12. **Sensitive-window marks never expire and cannot be cleared.** A terminal
    titled `pwsh` that once showed a key is hidden for good. Give marks a 12 h
    lifetime, add `screensense purge --marks`, show the count in `status`.
13. **Insights survive a tightening of the rules.** They are model output
    derived from screen text; clear them when a purge hides or deletes anything.

## Robustness

14. `purge` (CLI) does not store the rules hash, so the daemon purges again at
    its next start; a failed startup purge is not retried; a brand-new database
    logs "privacy rules changed: 0 …".
15. **Windows autostart registers `daemon run`**, which opens a console window
    that stays open and leaves the daemon without a log file. Register
    `daemon start` instead (`crates/platform/src/autostart.rs`, `win_misc.rs`).
16. The MCP server loads the config once; read-time rule checks use the rules
    from when it started.
17. `Store::span_touch` cannot move an end backwards, so the sensor rewrites the
    row when the user goes idle. Add `span_set_end`.
18. `PurgeStats` does not count rows removed through `forget_window`.

## CI on Windows (first run, 2026-10-11)

Linux and MSRV jobs pass. On `windows-latest` the 95 unit tests of the cli
crate pass and 27 of 30 integration tests in `crates/cli/tests/cli.rs` pass.
Three fail and stop the run before the core and platform test binaries and the
smoke steps (`doctor --capture`, PowerShell parse check) are reached:

- `doctor_reports_without_leaking_hidden_titles` (cli.rs:2017)
- `doctor_explains_common_failures` (cli.rs:2100)
- `shell_init_prints_the_snippet` (cli.rs:1604)

Not yet diagnosed. Likely test assumptions that only hold on Linux (the tests
unset `DISPLAY` to get the null backend, which does nothing on Windows; a path
comparison with backslashes), but confirm on a Windows machine before changing
them. Run `cargo test --workspace --no-fail-fast` there to see everything.

## Accepted and documented, not planned

- `stream` lines are tagged `[screen data]` per line rather than fenced.
- The X11 backend and the Windows screen-copy fallback capture whatever is on
  top of the foreground window's rectangle; content checks still apply.
- A hook command with a quoted executable path is a syntax error in PowerShell.
