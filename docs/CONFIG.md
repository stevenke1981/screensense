# Configuration reference

ScreenSense reads one TOML file. Every key is optional; a missing file means
"all defaults" and is not an error.

| | Windows | Linux |
|---|---|---|
| Config file | `%APPDATA%\screensense\config.toml` | `$XDG_CONFIG_HOME/screensense/config.toml` (default `~/.config/screensense/config.toml`) |
| Data directory | `%LOCALAPPDATA%\screensense` | `$XDG_DATA_HOME/screensense` (default `~/.local/share/screensense`) |

The environment variables `SCREENSENSE_CONFIG` and `SCREENSENSE_DATA` override
these locations, and so do the global options `--config <FILE>` and
`--data-dir <DIR>`.

```
screensense config path     # where the file is
screensense config init     # write a commented starter file
screensense config show     # the effective configuration
screensense config check    # parse the file and report problems
```

This document is written from `crates/core/src/config.rs`; that file is the
authority if the two ever disagree.

## When a change takes effect

The running daemon checks the config file about every 10 seconds. When it has
changed (or appeared, or been removed), the new settings and privacy rules
apply without a restart, and stored history the new rules forbid is deleted
(see [PRIVACY.md](PRIVACY.md)). One exception: the OCR engine is chosen when
the daemon starts, so a change to `capture.ocr` or `capture.ocr_language`
needs `screensense daemon restart`. `screensense hook` reads the file on every
call; `screensense mcp` reads it once, when it starts. A file that no longer
parses is reported, and the daemon keeps working with defaults (rule 2 below).

## Rules that keep a bad config file safe

1. **Privacy lists are additive.** Everything under `[privacy]` and `[apps]` is
   added on top of the built-in rules. Nothing in the file can remove or switch
   off a built-in rule.
2. **A file that cannot be parsed is ignored as a whole.** Defaults apply, and
   the error is shown by `screensense status`, `screensense doctor` and
   `screensense config check`. It does not half-apply. Note what "defaults
   apply" means for privacy: while the file is broken, your own additions
   (hidden apps, regexes, keywords) are not in force either, only the built-in
   rules are, and a running daemon keeps recording under them. Run
   `screensense config check` after editing.
3. **Unknown keys are rejected.** A typo such as `keep_imagez = true` is a parse
   error (rule 2 then applies), so a misspelt privacy setting cannot pass
   unnoticed.
4. **Numbers are clamped** to the ranges given below. A value that is not a
   finite number is replaced by the default first.
5. A user regular expression that does not compile is skipped and reported; the
   other rules still apply.

A UTF-8 byte-order mark at the start of the file is tolerated (Notepad writes one).

## Top level

| Key | Type | Default | Range | Meaning |
|---|---|---|---|---|
| `language` | string | `"auto"` | | Language of digests, CLI output and notifications. `"en"`, or `"zh"` (also `zh-tw`, `zh_tw`, `zh-hant`) for Traditional Chinese. Any other value means `auto`: the first non-empty of `SCREENSENSE_LANG`, `LC_ALL`, `LC_MESSAGES`, `LANG` decides (a value starting with `zh` selects Chinese, anything else English). |
| `poll_interval_s` | float | `1.0` | 0.2 – 10 | Seconds between foreground-window polls. |
| `idle_threshold_s` | float | `120.0` | 10 – 86400 | Seconds without input before the user counts as idle. |

## `[capture]`

| Key | Type | Default | Range | Meaning |
|---|---|---|---|---|
| `enabled` | bool | `true` | | `false` stops all looks: no screen text, no image, no errors found on screen. The window timeline (app and title, as the privacy rules allow), shell commands from the shell integration and the digest keep working. `status` notes "capture disabled in config". |
| `min_interval_s` | float | `3.0` | 1 – 600 | Shortest gap between two looks at the same window. |
| `max_interval_s` | float | `30.0` | `min_interval_s` – 3600 | Longest gap. The gap doubles every time a look finds nothing changed. |
| `dwell_s` | float | `1.5` | 0 – 60 | A window must stay in front this long before its first look. |
| `change_ratio` | float | `0.004` | 0 – 1 | Fraction of thumbnail pixels that must differ for a look to count as a change. |
| `refresh_s` | float | `180.0` | 0 – 86400 | Re-read an unchanged window at least this often. `0` disables. |
| `ocr` | bool | `true` | | Run text recognition. `false` means there is no OCR engine, and **without one nothing is captured** (a frame that cannot be checked for secrets is not stored), so `false` has the same effect as `enabled = false` for screen content. Needs `daemon restart`. |
| `ocr_language` | string | `""` | | Windows: a BCP-47 tag such as `zh-Hant-TW`; empty means the user's profile languages. Linux: a tesseract language spec such as `eng+chi_tra`; empty means `eng`. Needs `daemon restart`. |
| `ocr_max_chars` | integer | `8000` | 200 – 100000 | Upper bound on recognised text kept per frame. |
| `min_text_chars` | integer | `20` | | Frames with fewer characters of recognised text than this are dropped: too little to verify they are safe. Counted on the text after soft redaction, with leading and trailing whitespace removed; every character counts the same, spaces and line breaks inside the text included. |
| `keep_images` | bool | `true` | | Keep a pixel-redacted JPEG next to the text. `false` stores text only. |
| `image_max_width` | integer | `1600` | 320 – 7680 | Stored images are scaled down to this width. |
| `jpeg_quality` | integer | `70` | 20 – 95 | JPEG quality of stored images. |

## `[apps]`

Extra application names, added to the built-in lists. Use the executable name
on Windows (`MyTerm.exe`) and the process name on Linux (`myterm`). Names are
compared case-insensitively, without directory and without `.exe`.

| Key | Type | Default | Meaning |
|---|---|---|---|
| `terminal` | list of strings | `[]` | Treated as terminals (all error patterns apply; AI-tool titles are recognised). |
| `editor` | list of strings | `[]` | Treated as editors (only the strict error patterns apply). |
| `browser` | list of strings | `[]` | Treated as browsers (the private-browsing check applies). |

## `[privacy]`

User additions to the built-in rules. Additive only. See
[PRIVACY.md](PRIVACY.md) for what the built-in rules cover.

| Key | Type | Default | Meaning |
|---|---|---|---|
| `hidden_apps` | list of strings | `[]` | Apps recorded by name only. |
| `hidden_title_regex` | list of regexes | `[]` | Windows whose title matches are recorded by app name only. Also hides matching commands and paths. |
| `title_only_apps` | list of strings | `[]` | Apps recorded by title only (never captured). |
| `title_only_title_regex` | list of regexes | `[]` | Windows whose title matches are recorded by title only. |
| `self_apps` | list of strings | `[]` | The AI tool's own windows: title only, so it never screenshots itself. |
| `sensitive_text_regex` | list of regexes | `[]` | On-screen text matching any of these drops the whole frame. |
| `sensitive_keywords` | list of strings | `[]` | Two or more distinct keywords on screen (built-in and yours together) drop the whole frame. |
| `hidden_path_regex` | list of regexes | `[]` | Commands and paths matching these are hidden. |
| `redact` | list of tables | `[]` | Extra redaction rules, see below. |
| `unknown_browser` | string | `"title_only"` | What to do with a browser window whose private-browsing state cannot be determined: `"title_only"` (safe default) or `"full"`. Any value other than `full` means `title_only`. |

Title, path and sensitive-text regexes are compiled case-insensitively.

### `[[privacy.redact]]`

| Key | Type | Default | Meaning |
|---|---|---|---|
| `pattern` | regex | `""` | What to look for. Compiled exactly as written; start it with `(?i)` for case-insensitivity. An empty pattern is ignored with a warning. |
| `replace` | string | `""` | Replacement text, e.g. `[client]`. Empty means `[redacted]`. |
| `hard` | bool | `false` | `true`: seeing this on screen drops the whole frame instead of masking it. |

## `[digest]`

The per-message context block handed to the AI tool.

| Key | Type | Default | Range | Meaning |
|---|---|---|---|---|
| `enabled` | bool | `true` | | Produce a digest at all. |
| `delta` | bool | `true` | | Within one conversation, send only what changed since the previous message. |
| `full_refresh_minutes` | float | `30.0` | 1 – 1440 | Send a full digest again after this long. |
| `minutes` | float | `20.0` | 1 – 1440 | How far back the timeline goes. |
| `max_tokens` | integer | `900` | 150 – 2400 | Estimated-token ceiling for a full digest. |
| `delta_max_tokens` | integer | `300` | 60 – `max_tokens` | Estimated-token ceiling for a delta digest. |
| `max_spans` | integer | `10` | 1 – 50 | Most timeline entries shown. |
| `min_span_s` | float | `8.0` | 0 – 3600 | Shorter window visits are folded into "N brief switches". |
| `include_errors` | bool | `true` | | Include the errors section. |
| `include_image_path` | bool | `true` | | Mention the path of the latest stored image so the AI can open it. |
| `frame_max_age_s` | float | `1200.0` | 10 – 86400 | A stored frame older than this does not count as "latest": its image path is not mentioned, and `screensense text` / `screen_text` without a frame id will not return it. |

Token counts are estimates, not a tokenizer: four ASCII characters count as one
token, every other character as one token.

## `[errors]`

| Key | Type | Default | Range | Meaning |
|---|---|---|---|---|
| `dedupe_s` | float | `90.0` | 1 – 86400 | An error with the same key seen again within this many seconds of its last sighting is the same sighting: no new entry, no count increase. |
| `repeat_window_s` | float | `900.0` | 10 – 86400 | An error seen again later than `dedupe_s` but within this many seconds of its last sighting counts as a re-appearance: the existing entry's count goes up. Seen again after more than this, it is a new entry. |

## `[analyzer]`

Optional background analysis by a model running on this machine. Off by default.

| Key | Type | Default | Range | Meaning |
|---|---|---|---|---|
| `enabled` | bool | `false` | | Lets the daemon run the analyzer in the background, when a new error appears. `screensense analyze` does not need it: it runs one analysis on demand as soon as `backend`, `url` and `model` form a usable setup. |
| `backend` | string | `"ollama"` | | `"ollama"` (native API) or `"openai"` (OpenAI-compatible: LM Studio, llama.cpp, vLLM). |
| `url` | string | `""` | | Empty means the backend's default: `http://127.0.0.1:11434` for `ollama`, `http://127.0.0.1:1234/v1` for `openai`. Only `http://` is accepted; `https://`, `user:password@`, a query or a fragment are refused. |
| `model` | string | `""` | | Model name. Required when the analyzer is used. |
| `allow_remote` | bool | `false` | | Allow a URL whose host is not a loopback address. Off by default so screen text cannot leave the machine by accident. |
| `send_images` | bool | `false` | | Attach the latest stored image (needs a vision model). |
| `min_gap_s` | float | `120.0` | 10 – 86400 | Minimum age of the latest stored insight before the daemon starts another one. Does not apply to `screensense analyze`. |
| `max_calls_per_hour` | integer | `12` | | Upper bound on stored analyses in the last hour; applies to manual runs too. Computed from the database, so it survives a restart. |
| `timeout_s` | integer | `120` | 5 – 900 | Request timeout in seconds. |
| `notify` | bool | `true` | | Show a desktop notification when an analysis has suggestions. |
| `user_profile` | string | `""` | | One line about you, given to the model so advice fits (stack, preferred style). |

## `[watch]`

Thresholds for `screensense stream`.

| Key | Type | Default | Range | Meaning |
|---|---|---|---|---|
| `min_dwell_s` | float | `45.0` | 0 – 3600 | Only report a window switch once the new window has been in front this long. |
| `min_gap_s` | float | `20.0` | 0 – 3600 | At most one window-switch line per this many seconds. |

## `[retention]`

| Key | Type | Default | Range | Meaning |
|---|---|---|---|---|
| `images_hours` | float | `6.0` | 0 – 8760 | Stored images are deleted after this long. |
| `max_images_mb` | integer | `300` | | Size cap for all stored images; the oldest go first. |
| `frames_days` | float | `7.0` | 0.1 – 3650 | Frame text (and its search index) is deleted after this long. |
| `timeline_days` | float | `30.0` | 0.1 – 3650 | Window timeline, commands and errors. |
| `insights_days` | float | `90.0` | 0.1 – 3650 | Analyzer insights. |

## Full annotated example

Every value below is the default unless the comment says otherwise. You do not
need any of it; keep only what you change.

```toml
language = "auto"              # auto | en | zh
poll_interval_s = 1.0
idle_threshold_s = 120.0

[capture]
enabled = true
min_interval_s = 3.0
max_interval_s = 30.0
dwell_s = 1.5
change_ratio = 0.004
refresh_s = 180.0
ocr = true
ocr_language = ""              # Windows: "zh-Hant-TW"   Linux (tesseract): "eng+chi_tra"
ocr_max_chars = 8000
min_text_chars = 20
keep_images = true             # false = store recognised text only, never an image
image_max_width = 1600
jpeg_quality = 70

[apps]
terminal = []                  # example: ["MyTerm.exe"]
editor = []
browser = []

[privacy]
hidden_apps = []               # example: ["MyBank.exe"]: app name only
hidden_title_regex = []        # example: ["ClientName", "payroll"]
title_only_apps = []           # example: ["MyGame.exe"]: title only, never captured
title_only_title_regex = []
self_apps = []
sensitive_text_regex = []      # example: ["CONFIDENTIAL"]: drops the frame
sensitive_keywords = []
hidden_path_regex = []         # example: ["secret-project"]
unknown_browser = "title_only" # or "full"

# Not a default: an example rule. Masks ticket numbers in text and in the image.
[[privacy.redact]]
pattern = "ACME-\\d{6}"
replace = "[ticket]"
hard = false                   # true = drop the whole frame when this appears

[digest]
enabled = true
delta = true
full_refresh_minutes = 30.0
minutes = 20.0
max_tokens = 900
delta_max_tokens = 300
max_spans = 10
min_span_s = 8.0
include_errors = true
include_image_path = true      # false = never hand an image path to the AI tool
frame_max_age_s = 1200.0

[errors]
dedupe_s = 90.0
repeat_window_s = 900.0

[analyzer]
enabled = false
backend = "ollama"             # ollama | openai (LM Studio, llama.cpp, vLLM)
url = ""                       # empty = http://127.0.0.1:11434 (ollama) or http://127.0.0.1:1234/v1 (openai)
model = ""                     # required when enabled, e.g. a name shown by `ollama list`
allow_remote = false
send_images = false
min_gap_s = 120.0
max_calls_per_hour = 12
timeout_s = 120
notify = true
user_profile = ""              # example: "Rust + TypeScript, Windows. Short, concrete fixes."

[watch]
min_dwell_s = 45.0
min_gap_s = 20.0

[retention]
images_hours = 6.0
max_images_mb = 300
frames_days = 7.0
timeline_days = 30.0
insights_days = 90.0
```
