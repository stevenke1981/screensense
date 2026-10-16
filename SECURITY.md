# Security policy

ScreenSense reads your screen and hands text to AI agents that may be able to
run commands. Reports about its security and privacy behaviour are welcome.

## Reporting a vulnerability

Please report privately, not in a public issue:

1. Open <https://github.com/stevenke1981/screensense/security/advisories/new>
   (the repository's **Security** tab → **Report a vulnerability**).
2. Describe what you did, what you expected and what happened. A minimal
   reproduction helps most: a window title, a line of on-screen text, a config
   file, or a JSON-RPC message.
3. Do not include real secrets or real personal data. A synthetic value of the
   same shape is enough.

This is a small project maintained in spare time. There is no guaranteed
response time and no bounty. Only the latest release is supported.

## In scope

* **Privacy-rule bypasses**: a window that the documented rules say is Hidden
  or Title only, but whose title, text or image is stored or shown.
* **Fence escapes**: screen-derived text (window title, OCR text, command
  line, error line) that appears outside the `<<<DATA …>>>` / `<<<END DATA …>>>`
  markers in a digest, a CLI view or an MCP result, or that manages to forge
  or close a marker.
* **Redaction misses with a reproducible sample**: text of a kind listed in
  [docs/PRIVACY.md](docs/PRIVACY.md) that is stored unmasked, or is masked in
  the text but still legible in the stored image even though OCR recognised
  it. Please attach the text (synthetic) and, if relevant, a screenshot.
* **Path handling**: anything that makes ScreenSense read, write or delete a
  file outside its data directory, its config file or the agent config files
  it is asked to edit.
* **The MCP server** (`screensense mcp`): crashes on malformed input, error
  messages that echo client or screen text, tools doing more than documented.
* **The config writers** (`screensense setup`): damaging or silently rewriting
  another tool's settings, losing a user's existing entries, writing a hook
  command that runs something other than `screensense hook`.
* **The analyzer's local-only rule**: a way to make it contact a non-loopback
  host without `analyzer.allow_remote = true`.
* **Install scripts and release workflow**: checksum verification that can be
  bypassed.

## Out of scope

* An attacker who already runs code as your user, or has administrator or
  physical access. They can read the screen and the data directory directly.
* What an AI tool or its model provider does with a digest once it has it.
* An OCR engine misreading text (see the limits below); it is a known limit,
  though samples that show a *systematic* miss are useful as ordinary issues.
* Sites and applications that are simply not on the built-in lists. Those are
  feature requests; please open a normal issue or pull request.

## Threat model in brief

**Assets.** The history of what was on your screen, and the integrity of the
agent that reads it.

**Untrusted input.** Everything on screen. A web page, a chat message, a file
name or a log line can contain any text, including text written to look like
an instruction to an AI agent. Window titles, OCR text, command lines and
error lines are all treated as attacker-influenced. The local model's answer
(analyzer) is treated as untrusted too.

**What ScreenSense does about it.**

* Every screen-derived string is sanitised before it is shown to a model:
  invisible and format characters are removed, URLs are reduced to
  `[url:host]`, `javascript:` / `data:` style schemes are neutralised, `<` and
  `>` are replaced by the look-alikes `‹` and `›` and backticks by
  apostrophes (so `<<<` cannot appear in the content), whitespace is collapsed
  and every line is length-capped. The characters `|`, `{`, `}` and `\` are
  kept.
* It is placed between markers that carry a random per-call id (8 hex digits,
  32 bits, from a randomly seeded hasher; it is not a cryptographic generator), preceded by a sentence
  saying that the content is data and not instructions. Because the content
  cannot contain `<<<` or `>>>`, it cannot close or forge a marker.
* Nothing screen-derived is placed outside the fence in what is handed to a
  model, including MCP error messages. The one human-facing exception is the
  report of `screensense doctor --capture`, which is meant to be read by you
  (see below).
* ScreenSense never executes a command, never opens a URL and never follows
  anything it read on screen.
* It makes no network connection, except, when you enable the analyzer, to the
  model server you configured. That URL must be `http://` on a loopback
  address unless `analyzer.allow_remote = true`; proxies and redirects are not
  followed and no credentials are sent.
* Privacy rules fail closed: an unknown tier reads as Hidden, an unknown
  browser state is not captured, no OCR engine means nothing is captured, an
  invalid TOML or privacy regexes block recording and history output. Diagnostics may show defaults as explanatory values, but they do not enable recording under those defaults; user rules can only add to built-in ones.
* The programs it starts are fixed: `tesseract` (Linux, for OCR) and
  `notify-send` (Linux, for notifications). It does not run anything it read
  on screen.
* Neither log file (`daemon.log`, `hook-errors.log`) holds window titles,
  screen text, command lines or prompt text.

## Known limitations

These are properties of the design, not bugs. Please read them before relying
on ScreenSense.

* **Prompt injection is reduced, not eliminated.** The fence and the preamble
  are a request to the model. A model can still be persuaded by text inside
  the fence. Do not give an agent that reads your screen more authority than
  you would give to whatever is on your screen. Review what it proposes to
  run.
* **The database and images are not encrypted.** They are protected only by
  the permissions of your user profile (mode `0700` on the data directory on
  Linux; the profile ACL on Windows). Any process running as you can read
  them. SQLite secure-delete, FTS and WAL cleanup are best effort, not forensic erasure from SSDs or backups. Use full-disk encryption if the machine can be lost or shared.
* **Detection is best effort.** Sensitive windows are recognised by app name
  and title; secrets by pattern. A site that is not on a list, a secret
  without a recognisable shape, or text the OCR engine misread is not caught.
  A secret that OCR misreads is neither detected nor blacked out in the image.
* Quoted executable paths remain a syntax limitation for generated PowerShell hooks.
  Regex and OCR filtering are not security guarantees; sensitive-window marks
  do not expire and have no UI clear operation.
* **Invalid privacy configuration fails closed.** Invalid TOML or privacy regexes block recording and history reads. Diagnostic commands may display defaults for explanation; this is not a recording fallback. The daemon checks configuration about every 2 seconds.
* **OCR input is not written as a raw Linux PPM temporary file.** Linux sends captured image bytes to tesseract stdin; Windows OCR uses memory.
* **`doctor --capture` prints recognised text without a fence.** It shows at
  most three lines, soft hits masked, and nothing if the text trips a content
  check; it prints the window title only for windows recorded in full. Read it
  before you paste it into an issue.
* **The digest leaves the machine with your prompt** if your AI tool uses a
  remote model. ScreenSense filters it; it does not control where the tool
  sends it.
* **A stored image path is a capability.** If the digest mentions an image
  path, an agent with file access can open that image. Turn it off with
  `digest.include_image_path = false` or `capture.keep_images = false`.
* **Windows evidence is limited to synthetic fixtures.** Local and CI-owned window capture/OCR fixtures passed, as did memory-only OCR. Real applications, private-browser UI Automation, notifications and live agent sessions remain unverified. See [docs/WINDOWS-VERIFICATION.md](docs/WINDOWS-VERIFICATION.md).
* **On Linux/X11 the private-browsing state cannot be determined**, so browser
  windows are title-only by default.
* Release archives are checked against a `SHA256SUMS` file from the same
  release. That detects a corrupted download; it does not prove who built the
  release. Windows archives are unsigned; binaries are not code-signed.
