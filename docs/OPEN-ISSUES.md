# Open issues and release review record

This file preserves the 2026-10-11 independent review findings and records their release disposition. Resolved findings remain here as history. Release verification facts are in [WINDOWS-VERIFICATION.md](WINDOWS-VERIFICATION.md) and the release entry in [CHANGELOG.md](../CHANGELOG.md).

## Privacy and correctness review

1. **Invalid config could discard user privacy rules. RESOLVED.** Invalid TOML and invalid privacy regexes now fail closed for recording and history output. Diagnostic defaults are descriptive only.
2. **Pause could allow an in-flight capture to finish. RESOLVED.** A store pause/wipe during OCR invalidates the work; regression coverage confirms it leaves no frame, error, image or analyzer job.
3. **Config reload delay. RESOLVED.** The daemon checks about every 2 seconds.
4. **Low-confidence OCR word boxes. RESOLVED.** Low-confidence OCR words may have their text cleared while their bounds remain available for image masking. When geometry is uncertain, masking uses a conservative black region. The privacy regression suite covers this behavior; OCR text is not treated as a reliable source for masking geometry.
5. **Raw Linux OCR pixels on disk. RESOLVED.** Image bytes go to tesseract stdin; no raw PPM temporary file is created.
6. **Credential-shaped shell commands. RESOLVED.** Command-specific hard rules drop matching commands.
7. **Secret assignment prefix matching. RESOLVED.** Identifier prefix matching is constrained to complete identifiers.
8. **Line-wrapped secret detection. RESOLVED.** Hard checks include a line-break-free pass.
9. **Deleted SQLite rows lingering. MITIGATED, NOT FORENSIC ERASURE.** Secure-delete, FTS and WAL cleanup are best effort. No SSD or backup erasure guarantee is made.
10. **Agent ending a pause. RESOLVED.** MCP has seven tools and no resume operation. `screen_pause` keeps the longest active deadline; only the user CLI can resume early.
11. **Unnamed foreground process classification. RESOLVED.** An empty or literal `unknown` app identity is classified as Title only, so OCR and image capture are skipped. This does not mean every unrecognized app is Hidden.
12. **Sensitive-window marks never expire or clear. OPEN, DOCUMENTED.** Marks remain until the database/data directory is removed. `purge --all` preserves them.
13. **Insights surviving a stricter privacy rule. RESOLVED.** A stricter-rule purge removes derived insights; privacy revisions cancel active OCR/analyzer work and prevent stale results from being written back. Fencing prior insight text is an additional model trust boundary, not a substitute for deletion.

## Robustness review

14. **Purge retry and zero-change logging. PARTIALLY OPEN.** A failed purge does not advance the rules hash and can be retried. Zero-change purge log noise remains a known issue.
15. **Windows autostart launch behavior. IMPLEMENTED, RUNTIME UNVERIFIED.** The registered command uses `daemon start`; registry launch and console behavior were not exercised.
16. **MCP config staleness. RESOLVED.** Each request reloads config and applies the current privacy purge before returning data.
17. **Span end update behavior. RESOLVED.** The span end setter supports correction when returning to idle.
18. **Purge statistics accounting. RESOLVED.** The count now includes rows removed through sensitive-window cleanup.

## Retained limits

- Real Windows applications, browser private-mode UI Automation, notifications, and a live user-agent session have not been verified. Windows proof covers only nonactivating synthetic HWND fixtures and memory-only OCR.
- Windows capture uses `PrintWindow` on the target HWND, retries the default mode after a failed/unusable full-content result, and never falls back to copying desktop pixels.
- The generated PowerShell snippet parsed under 5.1 and 7, but was not installed or exercised. Quoted executable paths remain a syntax limitation.
- macOS and Wayland screen observation are unsupported. Linux browser private-mode state is unknown and title-only by default.
- Sensitive-window marks have no expiry or UI clear. SQLite/JPEG data is not encrypted; cleanup is not forensic erasure. Regex/OCR filters are not security guarantees.

## Release CI

On runtime-review candidate `b0b01e715cb9e07b64c6f1f85ade40242f574e45`, all required CI jobs passed: Linux format, clippy, workspace tests and Xvfb backend/daemon/doctor capture end-to-end; Windows clippy, tests and release smoke checks; and the Rust 1.89 MSRV job. Native Windows fixture tests also ran separately and both passed: owned-window capture plus OCR, and memory-only OCR. PowerShell 5.1/7 shell-init parsing passed. This is candidate evidence, not final-release-commit validation. The final commit SHA is recorded in `BUILD-INFO.txt` and must pass the release workflow checks. No claim is made that hooks were installed or that real applications were exercised.
