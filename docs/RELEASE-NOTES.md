# ScreenSense v0.1.0

ScreenSense is an early, opt-in local screen context tool for coding workflows. It records a privacy-filtered foreground-window history locally and can return a short digest through a user-configured hook or a seven-tool stdio MCP server. Recording, agent setup, autostart and shell command capture are separate user-controlled steps.

## What changed

- Fail-closed handling for invalid TOML and privacy regexes; current rules are reloaded before history is returned.
- Pause and purge revisions invalidate in-flight OCR and analyzer work so stale results cannot write back after a pause, wipe or privacy change.
- MCP exposes `screen_pause` but no `screen_resume`. Pause requests can extend the current deadline but cannot shorten it; only the user CLI can resume early.
- `now`, `shot`, hooks, MCP and analyzer output apply current privacy rules and purge before returning data. A stored JPEG that matches a newly applied soft mask is removed and its path is not returned.
- Linux OCR passes image bytes to `tesseract` stdin without creating a raw PPM temporary file.
- Windows capture uses `PrintWindow` on the target HWND. It retries default mode after an unusable full-content result and does not copy desktop pixels.
- Image deletion failures are reported. Deleted history does not return, and a later purge can retry unreferenced-image cleanup.
- Prior analyzer insight text stays inside the untrusted-data fence.

## Verification

On runtime-review candidate `b0b01e715cb9e07b64c6f1f85ade40242f574e45`, all required CI jobs passed: Linux formatting, clippy, workspace tests, and Xvfb backend/daemon/doctor capture end-to-end; Windows clippy, tests, and release smoke checks; and the Rust 1.89 minimum-version check. The test suites passed with 100 CLI unit tests, 38 CLI integration tests, 342 core tests and 30 platform tests. Two native Windows tests also passed separately: synthetic owned-window capture/OCR, and memory-only OCR. The local fixture captured 892x212 and recognized seven expected words; the GitHub Windows fixture captured 900x220 and recognized seven words. This is candidate evidence, not final-release-commit validation; final CI and archive gates apply to the final commit recorded in `BUILD-INFO.txt`.

The optimized Windows ZIP passed fresh-extraction CLI and short-lived MCP smoke checks on the build host, including no-screen doctor, synthetic record/commands, pause deadline behavior, purge retention and invalid-regex diagnostics. This was not a clean Windows OS test. PowerShell 5.1 and 7 shell-init output parsed successfully; profile installation and command-hook execution were not tested.

## Requirements and limits

- **Windows 10/11 x64:** requires the Microsoft Visual C++ v14 x64 runtime (`VCRUNTIME140.dll`). If missing, install the [official x64 Redistributable](https://aka.ms/vc14/vc_redist.x64.exe); see Microsoft's [supported versions](https://learn.microsoft.com/en-us/cpp/windows/latest-supported-vc-redist?view=msvc-170). Windows OCR also requires an installed OCR language. The archive is unsigned.
- **Linux x86_64 / X11:** requires a compatible glibc runtime, an X11 session with `DISPLAY`, and `tesseract` with the desired language data for OCR. Wayland screen observation is unsupported.
- macOS screen observation is unsupported. Linux/X11 private-browsing state is unknown, so browsers are title-only by default.
- Each release archive includes `LICENSES/index.json` and available original `LICENSE`, `NOTICE`, `COPYING` and `COPYRIGHT` texts collected for that archive target dependency set, including build and development dependencies. This is not an exact runtime-linked dependency inventory.
- Ordinary Windows applications, private-browser UI Automation, notifications, autostart registry execution, PowerShell hooks, and live agent sessions have not been verified.
- SQLite and JPEG data are not encrypted. Regex and OCR privacy filters are best effort, not security guarantees. SQLite/FTS/WAL cleanup is not forensic erasure from SSDs or backups. Sensitive-window marks do not expire and have no UI clear control. Quoted executable paths remain a PowerShell hook syntax limitation.

Before publication, the release workflow requires same-commit CI and Windows/Linux archive smoke checks. The final commit SHA is recorded in `BUILD-INFO.txt`; review the final release body and evidence against that commit.
