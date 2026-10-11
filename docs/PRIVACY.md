# Privacy

ScreenSense records what is on your screen. This document says exactly what
it records, what it refuses to record, where the data lives and how to get rid
of it. It is written from the code; the rule tables are in
[`crates/core/src/privacy.rs`](../crates/core/src/privacy.rs) and the purge
logic in [`crates/core/src/purge.rs`](../crates/core/src/purge.rs).

Read the limits as carefully as the features. Detection of sensitive windows
and of secrets is **best effort**. It depends on window titles, on application
names and on how well the OCR engine read the screen.

## What is recorded

| What | Where | Kept for (default) |
|---|---|---|
| Window timeline: app name, window title, start and end time | `spans` table | 30 days (`retention.timeline_days`) |
| Recognised (OCR) text of a window, after redaction | `frames` table and its full-text index | 7 days (`retention.frames_days`) |
| A JPEG of the window, with redacted words blacked out | `images/YYYY-MM-DD/*.jpg` | 6 hours (`retention.images_hours`) and at most 300 MB in total (`retention.max_images_mb`), oldest deleted first |
| Errors: error lines seen on screen, failed shell commands | `errors` table | 30 days |
| Shell commands with exit code, directory and duration (only with the opt-in shell integration) | `commands` table | 30 days |
| Analyzer insights (only if you enable the analyzer) | `insights` table | 90 days (`retention.insights_days`) |
| Per-conversation bookkeeping for delta digests (a session id and two timestamps) | `sessions` table | 2 days after last use |

Everything is in one directory:

| | Location |
|---|---|
| Windows | `%LOCALAPPDATA%\screensense` |
| Linux | `$XDG_DATA_HOME/screensense`, by default `~/.local/share/screensense` |

It holds `screensense.db` (SQLite, plus its `-wal` and `-shm` files), the
`images` directory, `daemon.log` and `daemon.lock`. `SCREENSENSE_DATA` or
`--data-dir` moves it.

ScreenSense does not record keystrokes, the clipboard, audio, or windows that
are not in the foreground.

**The data is not encrypted.** On Linux the directory is set to mode `0700`; on
Windows it relies on the permissions of your user profile. Any program running
as you can read it, and so can anyone with administrator access or with access
to an unencrypted disk or backup.

## Three tiers

Every foreground window is classified before anything is stored.

| Tier | Stored |
|---|---|
| **Full** | Title, recognised text, image. The text must still pass the content checks below. |
| **Title only** | The title. The window is never captured and never read by OCR. |
| **Hidden** | The app name only. The title is discarded before storage. |

### Decision order

`Privacy::classify` applies these checks in order; the first match wins.

1. **Hidden app**: the app is on the hidden list → Hidden.
2. **Hidden title**: the title matches a hidden-title pattern → Hidden.
3. **Self window**: the app is an AI tool, or it is a terminal whose title
   names one → Title only. The agent never gets a screenshot of itself.
4. **Title-only app** → Title only.
5. **Title-only title** → Title only.
6. **Browser check**, only for apps classed as browsers: the operating system
   says the window is private → Hidden; it cannot tell → Title only (unless
   `privacy.unknown_browser = "full"`).
7. Otherwise → Full.

App names are compared in lower case, without directory and without `.exe`.
Titles are compared after removing invisible characters, leading spinner
glyphs and notification counts such as `(3) `.

### Built-in rules, by category

These are summaries with a few examples each. **See the source for the full
list**: `crates/core/src/privacy.rs`, constants `HIDDEN_APPS`, `HIDDEN_TITLE`,
`SELF_APPS`, `SELF_TITLE`, `TITLE_ONLY_APPS`, `TITLE_ONLY_TITLE`.

**Hidden apps** (name only)

* Password managers: 1Password, Bitwarden, KeePass, KeePassXC, LastPass, …
* System keyrings: seahorse, KWalletManager, gnome-keyring.
* Authenticator: Authy.
* Cryptocurrency wallets: Exodus, Ledger Live, Electrum, Trezor Suite, …

**Hidden titles** (name only), matched case-insensitively

* Private browsing wording in several languages: "InPrivate", "Incognito",
  "Private Browsing", "無痕", "シークレット", …
* Password-manager names and phrases: "password manager", "密碼管理", …
* Online-banking phrases: "online banking", "internet banking", "網路銀行",
  "ネットバンキング", … This matches the *phrase*, not bank names: a bank's
  page whose title is only the bank's name is **not** recognised. Add your
  bank with `privacy.hidden_title_regex`.
* Medical records: "medical record", "patient portal", "MyChart", "病歷", "カルテ", …
* Adult content: a list of site names and terms.
* Secret files by name: `.env`, `credentials.json`, `id_rsa`, `.pem`,
  `kubeconfig`, `.aws/credentials`, `.npmrc`, `.tfstate`, …
* Wallet secrets: "seed phrase", "recovery phrase", "MetaMask", "助記詞", …

**Self windows** (title only)

* AI tool apps: Claude, Codex, OpenCode, ChatGPT, Gemini, Copilot.
* Terminals whose title names `claude`, `codex`, `opencode`, `gemini`
  (at the start), `aider`, `qwen-code` or `cursor-agent`.

**Title-only apps**

* Chat: Telegram, Discord, WhatsApp, Signal, LINE, WeChat, KakaoTalk, …
* Work chat and calls: Slack, Teams, Zoom, Webex, Lark/Feishu, DingTalk.
* Mail: Outlook, Thunderbird, Mailspring, Evolution, Geary.
* Lock and sign-in screens: LockApp, LogonUI, i3lock, swaylock, …
* Games and launchers: Steam, Epic Games Launcher, Battle.net, …
* Remote desktop (someone else's screen): mstsc, AnyDesk, TeamViewer,
  Parsec, RustDesk, Remmina, …

**Title-only titles**

* Webmail: "Gmail", "Outlook", "Inbox", "收件匣", …
* Web chat: "WhatsApp", "Discord", "Telegram", "Slack", "Messenger", "微信", …
* Video calls: "Google Meet", "Zoom", "Microsoft Teams", "Webex".

Applications and sites that are not on these lists are recorded in full. Add
your own with `[privacy]` in the config file; your additions cannot remove a
built-in rule. See [CONFIG.md](CONFIG.md).

## Content checks on recognised text

A window classified Full is captured and read by OCR. The recognised text is
checked **before anything is written**.

### Hard hits: the frame is dropped

If any of the following is found, nothing from that look is stored (no text,
no image), and the window is remembered as sensitive (see below).

* A **sensitive-text pattern**: a medical-record-number or national-ID label
  (for example "病歷號", "身分證字號", "MRN", "medical record number"), a US
  Social Security number written next to its label, or seed-phrase wording.
* **Two or more distinct sensitive keywords** on screen, from a list of
  medical and identity terms in English, Traditional and Simplified Chinese,
  Japanese, Korean, Spanish, French, German and Portuguese (for example
  "diagnosis", "prescription", "date of birth", "passport number", "處方",
  "健保卡").
* A match of a **hard redaction rule**. The families present in the table
  (`BUILTIN_RULES`) are:
  * PEM private-key blocks;
  * vendor API keys and tokens recognised by their prefix or shape: Anthropic
    (`sk-ant-`), OpenAI-style (`sk-`, `sk-proj-`, …), Stripe (`sk_live_`,
    `rk_test_`, `whsec_`), GitHub (`ghp_`, `github_pat_`, …), GitLab
    (`glpat-`), AWS access key IDs (`AKIA…`, `ASIA…`), Google API keys
    (`AIza…`), Slack tokens (`xoxb-`, `xapp-`, …), Slack and Discord webhook
    URLs, Supabase (`sb_secret_`, `sbp_`), npm (`npm_`), Hugging Face (`hf_`),
    SendGrid (`SG.`), Discord and Telegram bot tokens, and a group of further
    prefixes (`pypi-`, `dckr_pat_`, `gsk_`, `r8_`, `re_`, `shpat_`, `dop_v1_`,
    `pplx-`, `xai-`);
  * JSON Web Tokens;
  * `Bearer <token>` and `Authorization: Basic <…>` headers;
  * URLs with embedded credentials (`scheme://user:password@host`);
  * secret assignments such as `API_KEY=…` or `"password": "…"`, with
    heuristics that skip code (`token = get_token()`), placeholders and masks;
  * payment-card numbers that pass the Luhn check and an issuer-prefix check;
  * Taiwan (ROC) national ID numbers that pass their checksum.

Frames with fewer than `capture.min_text_chars` (20) recognised characters are
also dropped: there is too little text to verify that the frame is safe. A
consequence is that without a working OCR engine no frame is stored at all;
only the window timeline is.

### Soft hits: masked, frame kept

These are replaced by a placeholder in the text and blacked out in the image:

* database URLs (`postgres://…`, `mysql://…`, `mongodb://…`, `redis://…`, …) → `[db-url]`
* secret command-line flags (`--password x`, `--token=…`, `--api-key …`) → `[secret]`
* e-mail addresses → `[email]`
* Taiwan mobile numbers (`09xx-xxx-xxx`) and international numbers written
  with a leading `+` → `[phone]`

### What is *not* covered

There is no rule for: national ID numbers of other countries (apart from a US
SSN next to its label), IBANs and bank account numbers, postal addresses,
personal names, phone numbers in other national formats, or a secret that has
no recognisable prefix and is not written as an assignment. Add rules for
what matters to you with `[[privacy.redact]]`, `privacy.sensitive_text_regex`
and `privacy.sensitive_keywords`.

## Pixel redaction and its limit

The OCR engine returns a pixel box for every word. Every word that overlaps a
redacted span of text is covered with a black rectangle (the box plus a
2-pixel margin) before the JPEG is written. So a soft hit is hidden in the
stored image as well as in the stored text.

The limit is fundamental: **only text the OCR engine recognised can be
checked or blacked out.** If OCR misreads a secret (a `0` read as `O`, a key
split across two lines, small or low-contrast text, text inside a picture, a
script the installed language pack does not cover), the secret is neither
detected nor masked, and it stays visible in the stored image. Handwriting,
photos, QR codes and faces are never analysed.

If that risk is not acceptable, set `capture.keep_images = false`: only
recognised text is stored, and text that OCR did not read is simply absent.

On Linux/X11 the capture is the screen area the window occupies, so anything
drawn over the window at that moment (a notification, an always-on-top
window) is in the picture. It goes through the same text checks, but it was
classified under the foreground window's name and title, not its own.

## Private browsing

A private or incognito window often has an ordinary title, so the title rules
cannot be relied on. ScreenSense asks the platform.

| Platform | How | Result |
|---|---|---|
| Windows | A UI Automation walk over the browser's own controls, looking for private-mode markers. It is bounded (400 elements, depth 10, 600 ms). | Private → Hidden. Positively normal → Full. Could not tell (limits reached, accessibility tree not built yet, any error) → **unknown**. |
| Linux / X11 | There is no signal to ask. | Always **unknown**. |

Unknown is handled by `privacy.unknown_browser`. The default, `"title_only"`,
means an unknown browser window is **not captured**. On Linux/X11 this means
that by default browser windows are recorded by title only. Setting
`unknown_browser = "full"` captures them; private windows are then caught only
if their title says so (the hidden-title rule above). Do not count on that:
the title of a Chrome, Brave, Opera or Vivaldi incognito window does not.

The Windows probe has not been run on real hardware yet; see
[WINDOWS-VERIFICATION.md](WINDOWS-VERIFICATION.md).

## Sensitive-window memory

When a hard hit drops a frame, that window (app plus title) is remembered and
treated as Hidden from then on, including after a restart. The mark is stored
in the database as a hash of the app name and title, not the title itself.

## Shell commands

With the shell integration, command lines are cleaned before storage. A
command is **not stored at all** if it matches a hidden-title or
`hidden_path_regex` pattern (for example anything mentioning `.env` or
`id_rsa`) or contains a hard-rule secret. Soft hits are masked. A command line
that starts with a space is never reported by the shell snippet in the first
place, and neither are `screensense …` commands.

## Retroactive purge

Tightening a rule deletes matching history; it does not merely hide it.

* The effective rule set (built-in plus yours) has a hash. The daemon compares
  it with the stored one when it starts and when it reloads the configuration;
  if it differs, every stored span, frame, error and command is re-checked
  against the current rules.
* `screensense purge` runs the same check on demand.
* A window that is now Hidden loses its stored title, frames, errors and
  images. One that is now Title only loses its frames and images. A frame or
  error whose text a new rule would mask or reject is deleted, because the
  stored text and pixels predate the rule. A command that would now be cleaned
  differently is deleted.
* Image files are deleted from disk together with their rows.

In addition, everything is checked against the current rules again each time
it is read for a digest or a tool result.

Loosening a rule never brings anything back, and never widens what an old
record shows.

## Pausing

`screensense pause [MIN]` stops recording; `screensense resume` continues. An
agent can do the same through the `screen_pause` and `screen_resume` MCP
tools. A pause is a request stored in the database and honoured by the daemon.

## Deleting everything

```
screensense purge --all
```

deletes every span, frame, error, command, insight and session, removes all
images and compacts the database file. It keeps two things on purpose: the
stored rules hash and the hashed sensitive-window marks, so that windows found
sensitive earlier stay hidden.

To remove every trace, stop the daemon and delete the data directory:

```
screensense daemon stop
# Windows (PowerShell)
Remove-Item -Recurse -Force "$env:LOCALAPPDATA\screensense"
# Linux
rm -rf "${XDG_DATA_HOME:-$HOME/.local/share}/screensense"
```

`screensense setup --remove` takes the hook or MCP entry out of your AI tools,
and `screensense autostart off` removes the sign-in entry.

## What leaves the machine

**ScreenSense itself sends nothing anywhere.** It has no telemetry, no update
check and no cloud backend. The one network connection it can make is to the
model server you configure for the optional analyzer, which must be on this
machine (a loopback address) unless you set `analyzer.allow_remote = true`.

**But the digest goes wherever your AI tool sends its prompts.** That is the
purpose of the tool: the hook adds the digest to your prompt, and the MCP
tools return screen text to the agent. From there it is transmitted to that
tool's model provider under that provider's terms, exactly like text you typed
yourself. Window titles, error lines, command lines and recognised screen text
that passed the filters above are in it. If your agent uses a cloud model,
filtered screen content reaches that cloud.

**Images** are not sent by ScreenSense. The digest may contain the *path* of
the latest stored image; an image leaves the machine only if the agent opens
that file and its tool uploads it. To prevent that:

* `digest.include_image_path = false`: the path is never mentioned;
* `capture.keep_images = false`: no image is stored at all.

With the analyzer enabled and `analyzer.send_images = true`, the latest image
is sent to the configured model server.

## Prompt injection

Text on your screen can be written by anyone: a web page, a chat message, a
log line. ScreenSense reduces the risk that an agent treats it as an
instruction (sanitising, a random fence, an explicit "data, not instructions"
preamble) but cannot eliminate it. See [SECURITY.md](../SECURITY.md).
