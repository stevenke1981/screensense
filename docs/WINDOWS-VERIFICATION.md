# Windows verification

Windows verification on runtime-review candidate `b0b01e715cb9e07b64c6f1f85ade40242f574e45` passed local tests and CI. The final release commit SHA is recorded in `BUILD-INFO.txt` and must pass final CI and archive smoke checks. After fresh extraction, the first check is `./screensense.exe --help`; the optimized x64 ZIP also passed CLI/MCP smoke checks on the build host. This was not a clean Windows OS test. This evidence does not establish general application support.

## Passed

The executable imports `VCRUNTIME140.dll` and requires the Microsoft Visual C++ v14 x64 runtime. If missing, install the [official x64 Redistributable](https://aka.ms/vc14/vc_redist.x64.exe); see [Microsoft supported versions](https://learn.microsoft.com/en-us/cpp/windows/latest-supported-vc-redist?view=msvc-170). Windows OCR also requires an installed OCR language. The archive is unsigned.

- Windows CI clippy, workspace tests, release smoke checks and PowerShell 5.1/7 shell-init parsing passed. The shell snippet was parsed, not installed into a user profile or exercised after commands.
- A local native test created a nonactivating synthetic popup window owned by the test process, captured the same HWND using `PrintWindow` (full-content mode with the default-mode retry), ran Windows OCR, found seven expected words, and cleaned up the window. Captured frame: 892×212.
- A separate native memory-only OCR test passed.
- The GitHub Windows runner's native synthetic capture/OCR test passed at 900×220 with seven expected words; its memory-only OCR test passed too.

The capture retry uses only the same HWND and never copies desktop pixels. The local fixture needed 20px text margins because the captured client area was cropped by a 4px DWM border. This is fixture-specific evidence, not a compatibility guarantee.

## Still unverified

- Capture/OCR behavior in Chrome, Edge, VS Code, Windows Terminal, UWP apps, covered or minimized windows, multiple DPI scales and 4K windows.
- Private-browser UI Automation probes for supported browsers, notification toasts, autostart registry behavior and live agent integration.
- PowerShell hook execution, profile installation, and argument fidelity. Quoted executable paths remain a known hook syntax limitation.

macOS and Wayland screen observation are unsupported. Linux OCR sends image bytes on stdin to `tesseract`, without a raw PPM temporary file.
