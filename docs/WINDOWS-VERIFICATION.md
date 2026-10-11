# Windows verification checklist

The Windows backend was written and type-checked from Linux. Each item below is
an assumption about Windows or the `windows` crate that only a real run can
confirm. Tick them off on a Windows 10/11 machine; `screensense doctor --capture`
exercises most of the first two sections.

## Capture (`crates/platform/src/win.rs`)

- [ ] `PrintWindow(PW_RENDERFULLCONTENT)` returns a correct, non-black frame for
      Chrome, Edge, VS Code (Electron), Windows Terminal and a UWP app, also when
      partly covered. A solid-colour frame is treated as blank and falls back to a
      screen copy, which only runs for the foreground window.
- [ ] `GetWindowRect` and `DwmGetWindowAttribute(DWMWA_EXTENDED_FRAME_BOUNDS)` are
      in the same coordinate space once per-monitor DPI awareness is set, so the
      crop that removes the invisible resize border is aligned (check on a
      monitor scaled to 150 %).
- [ ] `GetDIBits` with a negative height yields top-down rows on your GPU/driver.
- [ ] A 4K window captures in acceptable time and memory.
- [ ] UWP apps resolve to their real process name, not `ApplicationFrameHost.exe`.
- [ ] `GetWindowTextW` on a busy foreign window does not stall the sensor.

## OCR (`win_ocr.rs`)

- [ ] `IAsyncOperation::get()` on an MTA thread returns (does not deadlock).
- [ ] `TryCreateFromUserProfileLanguages` failing falls through to the first
      available recognizer language.
- [ ] Word boxes are in the pixel space of the submitted bitmap, including after
      the downscale for images larger than `MaxImageDimension`.
- [ ] CJK text arrives one character per "word" and is joined without spaces by
      the pipeline.

## Private-browsing probe (`win_uia.rs`)

- [ ] An Incognito / InPrivate window is recognised for Chrome, Edge, Brave,
      Vivaldi and Opera within the limits (400 nodes, depth 10, 600 ms).
- [ ] A normal window with many tabs still finishes the walk and reports
      "not private" (at least 30 named elements seen); otherwise every browser
      window stays title-only, which is safe but loses the feature.
- [ ] The first probe after a browser starts may be "unknown" (lazy
      accessibility tree) and a later one succeeds.
- [ ] The 400 ms UIA connection/transaction timeouts do not break normal calls.

## Notifications, autostart, process (`win.rs`, `win_misc.rs`)

- [ ] A toast appears using the borrowed PowerShell AppUserModelID (it is
      attributed to "Windows PowerShell").
- [ ] `autostart on` writes `HKCU\…\Run\ScreenSense` (the command is the full
      path of the exe followed by `daemon run`) and the daemon starts at
      sign-in. The binary is a console-subsystem program and nothing hides its
      console: `daemon start` creates the daemon with `CREATE_NO_WINDOW`, but a
      process started by the Run key is not, so a console window may appear
      and stay open. Check this. A daemon started this way has no log file
      (`daemon run` writes to the console).
- [ ] `daemon start` survives closing the terminal it was started from
      (a Job object that kills children on close would defeat it).
- [ ] `pid_alive` and `daemon stop` behave.

## Shell integration (`crates/core/src/shell.rs`, PowerShell script)

- [ ] `Invoke-Expression (& screensense shell-init powershell | Out-String)` parses
      in Windows PowerShell 5.1 and PowerShell 7.
- [ ] Each command is recorded once with a sensible exit code; `$LASTEXITCODE`
      and the existing prompt are unchanged; no window flashes.
- [ ] Arguments containing quotes and trailing backslashes arrive intact.

## Integrations

- [ ] `setup claude|codex|gemini|opencode` writes working entries when the
      executable path contains spaces.
