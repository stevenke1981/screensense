# Third-party notices

ScreenSense itself is MIT licensed (see [LICENSE](LICENSE)). This file lists
what it builds on.

## desktop-sense (inspiration, no code copied)

ScreenSense is inspired by [desktop-sense](https://github.com/angletech2026-arch/desktop-sense).
No code was copied from it. The built-in privacy keyword lists and the
error-line patterns were informed by studying that project, so its licence is
reproduced here as a courtesy and in acknowledgement.

```
MIT License

Copyright (c) 2026 AngleTech

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

## Rust dependencies

The CLI/core/platform binary links the Rust crates below. The main direct dependencies are:

| Crate | Used for | Licence |
|---|---|---|
| `rusqlite` (with `libsqlite3-sys`, bundled SQLite) | the local database | MIT |
| `clap` | command line parsing | MIT OR Apache-2.0 |
| `serde`, `serde_json`, `toml` | configuration, JSON-RPC, hook I/O | MIT OR Apache-2.0 |
| `regex` | privacy and error-detection rules | MIT OR Apache-2.0 |
| `chrono` | clock times | MIT OR Apache-2.0 |
| `image` | JPEG/PNG encoding | MIT OR Apache-2.0 |
| `ureq` (no TLS) | HTTP to a local model server (analyzer) | MIT OR Apache-2.0 |
| `unicode-general-category` | stripping invisible characters | Apache-2.0 |
| `x11rb` (Linux) | talking to the X server | MIT OR Apache-2.0 |
| `windows` (Windows) | Win32 and WinRT bindings | MIT OR Apache-2.0 |
| `anyhow`, `thiserror` | error handling | MIT OR Apache-2.0 |

**SQLite.** `libsqlite3-sys` compiles the SQLite amalgamation into the binary. SQLite is in the public domain.

**GUI crate.** `crates/gui` is licensed under GPL-3.0-only (see [`crates/gui/LICENSE`](crates/gui/LICENSE)). Its Slint UI and build crates are pinned to 1.15.1. Slint's upstream package offers GPL-3.0-only or separate royalty-free/commercial license options; this repository selects the GPL-3.0-only route and does not rely on a paid Slint license. The GUI's GPL license scope does not change the MIT licenses of the existing CLI, core, and platform crates. See [Slint's versioned package metadata](https://raw.githubusercontent.com/slint-ui/slint/v1.15.1/Cargo.toml) and [licensing options](https://slint.dev/pricing).

Each release archive also includes `LICENSES/index.json` and available original `LICENSE`, `NOTICE`, `COPYING` and `COPYRIGHT` texts collected for that archive's target-filtered Cargo dependency set, including build and development dependencies. This collection is not an exact runtime-linked dependency inventory. The online summary below is a concise overview; the per-archive index and texts provide the notices included with that build.

### Summary by licence

The 148-crate count below is the historical all-target dependency summary for the v0.1.0 CLI/core/platform release, generated from that release Cargo.lock. It does not include the GUI crate and its Slint dependency graph. Some dependencies in the release summary are target-specific or build-time rather than linked into one binary. Where a crate offers a choice of licences, the existing CLI/core/platform distribution uses the permissive option.

| Licence expression (SPDX, as declared by the crate) | Crates |
|---|---|
| MIT OR Apache-2.0 | 94 |
| Unicode-3.0 (the `icu_*` family and its data) | 18 |
| MIT | 8 |
| Apache-2.0 OR MIT | 7 |
| MIT/Apache-2.0 | 3 |
| Unlicense OR MIT | 3 |
| Apache-2.0 | 2 |
| Zlib | 2 |
| BSD-3-Clause OR Apache-2.0 | 2 |
| Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT | 2 |
| MIT OR Zlib OR Apache-2.0 | 2 |
| MIT OR Apache-2.0 OR Zlib | 2 |
| (MIT OR Apache-2.0) AND Unicode-3.0 | 1 |
| Zlib OR Apache-2.0 OR MIT | 1 |
| 0BSD OR MIT OR Apache-2.0 | 1 |

The historical CLI/core/platform release dependency summary above contains no declared GPL, LGPL, AGPL, or MPL licence. The GUI crate is a separate GPL-3.0-only component as described above.
The full licence text of each crate is in its source distribution on
crates.io; this file does not reproduce them one by one. To list the crates
yourself:

```
cargo metadata --format-version 1
cargo tree -e normal --prefix none -p screensense | sort -u
```

## External programs (not distributed with ScreenSense)

* **Tesseract OCR** (Linux): ScreenSense runs the `tesseract` command if it is
  installed. It is not bundled; its own licence (Apache-2.0) applies to your
  installation.
* **Windows.Media.Ocr** (Windows): part of the operating system.
* **notify-send** (Linux, optional): used for desktop notifications if present.
