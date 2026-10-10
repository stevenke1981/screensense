//! Wiring ScreenSense into AI coding agents.
//!
//! `install` / `remove` edit one config file per agent. They touch only their own
//! entry, never "repair" a file they cannot parse, back the file up before the first
//! change and replace it atomically. The prompt hook's I/O (`parse_hook_input`,
//! `hook_output`) lives here too so the CLI's `hook` command is a thin wrapper.
//!
//! Known limitation: `serde_json` is built without `preserve_order`, so a file we
//! modify is re-serialised with its object keys sorted alphabetically (array order,
//! values and every other entry are preserved). The timestamped backup made before
//! each modification is what makes that acceptable.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde_json::{json, Map, Value};

use crate::{Error, Result};

#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum Agent {
    Claude,
    Codex,
    Gemini,
    OpenCode,
}

impl Agent {
    pub const ALL: [Agent; 4] = [Agent::Claude, Agent::Codex, Agent::Gemini, Agent::OpenCode];

    pub fn name(self) -> &'static str {
        match self {
            Agent::Claude => "claude",
            Agent::Codex => "codex",
            Agent::Gemini => "gemini",
            Agent::OpenCode => "opencode",
        }
    }

    pub fn parse(s: &str) -> Option<Agent> {
        match s.trim().to_ascii_lowercase().as_str() {
            "claude" | "claude-code" | "claude_code" | "claudecode" => Some(Agent::Claude),
            "codex" | "codex-cli" | "codex_cli" => Some(Agent::Codex),
            "gemini" | "gemini-cli" | "gemini_cli" => Some(Agent::Gemini),
            "opencode" | "open-code" | "open_code" => Some(Agent::OpenCode),
            _ => None,
        }
    }

    pub fn display(self) -> &'static str {
        match self {
            Agent::Claude => "Claude Code",
            Agent::Codex => "Codex CLI",
            Agent::Gemini => "Gemini CLI",
            Agent::OpenCode => "OpenCode",
        }
    }

    /// Config directory, relative to the home directory.
    fn dir_parts(self) -> &'static [&'static str] {
        match self {
            Agent::Claude => &[".claude"],
            Agent::Codex => &[".codex"],
            Agent::Gemini => &[".gemini"],
            Agent::OpenCode => &[".config", "opencode"],
        }
    }

    fn file_name(self) -> &'static str {
        match self {
            Agent::Claude | Agent::Gemini => "settings.json",
            Agent::Codex => "hooks.json",
            Agent::OpenCode => "opencode.json",
        }
    }

    /// `~/.claude` style label for messages.
    fn dir_label(self) -> String {
        format!("~/{}", self.dir_parts().join("/"))
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Action {
    Installed,
    Updated,
    Unchanged,
    Removed,
    NotInstalled,
    /// Nothing was written; the string says why and what to do by hand.
    Skipped(String),
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct SetupResult {
    pub agent: Agent,
    pub path: PathBuf,
    pub action: Action,
    pub backup: Option<PathBuf>,
    /// Extra lines for the user (e.g. "run /hooks in Codex once to trust it").
    pub notes: Vec<String>,
}

fn agent_dir(agent: Agent, home: &Path) -> PathBuf {
    let mut p = home.to_path_buf();
    for part in agent.dir_parts() {
        p.push(part);
    }
    p
}

/// Where the home-relative config lives. `home` is injected so tests use a temp dir.
pub fn config_file(agent: Agent, home: &Path) -> PathBuf {
    agent_dir(agent, home).join(agent.file_name())
}

/// Agents whose config directory exists under `home`.
pub fn detect(home: &Path) -> Vec<Agent> {
    Agent::ALL
        .into_iter()
        .filter(|a| agent_dir(*a, home).is_dir())
        .collect()
}

// ---------------------------------------------------------------------------
// Command strings
// ---------------------------------------------------------------------------

/// The exe path as text: `\\?\` / `\\?\UNC\` prefixes removed, backslashes turned
/// into forward slashes (every Windows shell and every POSIX shell accepts them).
pub(crate) fn forward_slashes(exe: &Path) -> String {
    let s = exe.to_string_lossy();
    let s: String = if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = s.strip_prefix(r"\\?\") {
        rest.to_string()
    } else if let Some(rest) = s.strip_prefix("//?/UNC/") {
        format!("//{rest}")
    } else if let Some(rest) = s.strip_prefix("//?/") {
        rest.to_string()
    } else {
        s.into_owned()
    };
    s.replace('\\', "/")
}

/// True when the path needs no quoting in cmd.exe, PowerShell and POSIX shells.
fn is_plain_path(p: &str) -> bool {
    !p.is_empty()
        && p.chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '_' | '.' | '/' | ':' | '-' | '+'))
}

/// Characters we refuse to put in a hook command string: they would expand or end the
/// quoting in at least one of the shells a hook may run under.
fn has_unquotable_char(p: &str) -> bool {
    p.chars()
        .any(|c| matches!(c, '"' | '$' | '`' | '%' | '\n' | '\r' | '\0'))
}

/// The command string written into hook configs: `<exe> hook`, forward slashes.
///
/// A path made only of letters, digits and `_ . / : - +` is written bare, because a
/// *quoted* path followed by an argument is a syntax error in PowerShell
/// (`"C:/x/screensense.exe" hook`), and Codex / Gemini / Claude Code may run hooks
/// through PowerShell on Windows. Any other path is wrapped in double quotes
/// (`"C:/Program Files/x/screensense.exe" hook`), which cmd.exe and POSIX shells accept.
pub fn hook_command(exe: &Path) -> String {
    let p = forward_slashes(exe);
    if is_plain_path(&p) {
        format!("{p} hook")
    } else {
        format!("\"{p}\" hook")
    }
}

/// Is this hook command one we wrote? The text must end in ` hook`, and what comes
/// before it, lower-cased, with quotes removed and `\` read as `/`, must have the file
/// name `screensense` or `screensense.exe`. So `echo screensense hook` (file name
/// `echo screensense`), `desktop-sense hook` and `... screensense.exe hookx` are not ours.
fn is_ours_command(command: &str) -> bool {
    let Some(prefix) = command.trim_end().strip_suffix(" hook") else {
        return false;
    };
    let cleaned: String = prefix
        .trim()
        .chars()
        .filter(|c| !matches!(c, '"' | '\''))
        .collect::<String>()
        .to_lowercase()
        .replace('\\', "/");
    let file = cleaned.rsplit('/').next().unwrap_or("");
    file == "screensense" || file == "screensense.exe"
}

fn is_ours_entry(v: &Value) -> bool {
    v.get("command")
        .and_then(Value::as_str)
        .is_some_and(is_ours_command)
}

// ---------------------------------------------------------------------------
// Entry shapes
// ---------------------------------------------------------------------------

/// Event name and hook entry for the agents that have prompt hooks.
///
/// Shapes follow the desktop-sense reference (`dsense/integrations.py`, `AGENTS`):
/// * Claude Code `settings.json`, `UserPromptSubmit`, `timeout` in seconds.
/// * Codex CLI `hooks.json`, `UserPromptSubmit`, `commandWindows`, `timeout` in
///   seconds (10), `statusMessage`.
/// * Gemini CLI `settings.json`, `BeforeAgent`, `name`, `timeout` in milliseconds (5000).
fn hook_entry(agent: Agent, cmd: &str) -> Option<(&'static str, Value)> {
    match agent {
        Agent::Claude => Some((
            "UserPromptSubmit",
            json!({"type": "command", "command": cmd, "timeout": 5}),
        )),
        Agent::Codex => Some((
            "UserPromptSubmit",
            json!({
                "type": "command",
                "command": cmd,
                "commandWindows": cmd,
                "timeout": 10,
                "statusMessage": "screensense",
            }),
        )),
        Agent::Gemini => Some((
            "BeforeAgent",
            json!({"type": "command", "command": cmd, "name": "screensense", "timeout": 5000}),
        )),
        Agent::OpenCode => None,
    }
}

fn opencode_entry(exe: &Path) -> Value {
    json!({"type": "local", "command": [forward_slashes(exe), "mcp"], "enabled": true})
}

const OPENCODE_SCHEMA: &str = "https://opencode.ai/config.json";

/// The JSON a user can paste by hand (shown by `screensense setup --print <agent>`).
pub fn manual_snippet(agent: Agent, exe: &Path) -> String {
    let doc = match hook_entry(agent, &hook_command(exe)) {
        Some((event, entry)) => json!({"hooks": {event: [{"hooks": [entry]}]}}),
        None => json!({"$schema": OPENCODE_SCHEMA, "mcp": {"screensense": opencode_entry(exe)}}),
    };
    serde_json::to_string_pretty(&doc).unwrap_or_default()
}

// ---------------------------------------------------------------------------
// File handling
// ---------------------------------------------------------------------------

enum Loaded {
    Missing,
    Parsed(Value),
    /// Exists but must not be edited; the string says why.
    Bad(String),
}

fn load(path: &Path) -> Result<Loaded> {
    let bytes = match fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Loaded::Missing),
        Err(e) => return Err(e.into()),
    };
    let Ok(text) = std::str::from_utf8(&bytes) else {
        return Ok(Loaded::Bad("the file is not valid UTF-8".into()));
    };
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    if text.trim().is_empty() {
        return Ok(Loaded::Parsed(Value::Object(Map::new())));
    }
    Ok(match serde_json::from_str::<Value>(text) {
        Ok(v @ Value::Object(_)) => Loaded::Parsed(v),
        Ok(_) => Loaded::Bad("the top-level value is not a JSON object".into()),
        Err(e) => Loaded::Bad(format!(
            "it is not plain JSON ({e}); comments and trailing commas (JSONC) are not supported"
        )),
    })
}

/// `<name>.bak-screensense-YYYYmmdd-HHMMSS` (UTC) next to `path`; never overwrites an
/// earlier backup made in the same second.
fn backup(path: &Path) -> Result<PathBuf> {
    let stamp = chrono::Utc::now().format("%Y%m%d-%H%M%S");
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut dst = path.with_file_name(format!("{name}.bak-screensense-{stamp}"));
    let mut n = 1;
    while dst.exists() {
        dst = path.with_file_name(format!("{name}.bak-screensense-{stamp}-{n}"));
        n += 1;
    }
    fs::copy(path, &dst)?;
    Ok(dst)
}

/// Write `<file>.screensense.tmp` next to the target, flush, then rename over it.
/// A symlinked config file (dotfile managers) is written through the link.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let target = match fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_symlink() => fs::canonicalize(path).unwrap_or(path.to_path_buf()),
        _ => path.to_path_buf(),
    };
    if let Some(dir) = target.parent() {
        fs::create_dir_all(dir)?;
    }
    let mut tmp_name = target.file_name().unwrap_or_default().to_os_string();
    tmp_name.push(".screensense.tmp");
    let tmp = target.with_file_name(tmp_name);
    let attempt = (|| -> std::io::Result<()> {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.flush()?;
        f.sync_all()?;
        drop(f);
        if let Ok(meta) = fs::metadata(&target) {
            // Keep the original's mode (settings files often hold tokens: 0600).
            let _ = fs::set_permissions(&tmp, meta.permissions());
        }
        fs::rename(&tmp, &target)
    })();
    if attempt.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    attempt.map_err(Error::from)
}

/// Back up (when the file existed) and atomically write `doc`, pretty-printed.
fn commit(path: &Path, doc: &Value, existed: bool) -> Result<Option<PathBuf>> {
    let bak = if existed { Some(backup(path)?) } else { None };
    let mut text = serde_json::to_string_pretty(doc)?;
    text.push('\n');
    write_atomic(path, text.as_bytes())?;
    Ok(bak)
}

// ---------------------------------------------------------------------------
// JSON editing
// ---------------------------------------------------------------------------

#[derive(PartialEq, Eq, Debug)]
enum Change {
    Added,
    Replaced,
    Same,
}

/// Put `entry` under `hooks.<event>[*].hooks[*]`, replacing our existing entry in place
/// (extra copies of ours are dropped) or appending a new `{"hooks":[entry]}` group.
/// `Err` carries the reason when the document's shape is not what we expect.
fn apply_hook(doc: &mut Value, event: &str, entry: &Value) -> std::result::Result<Change, String> {
    let root = doc
        .as_object_mut()
        .ok_or("the top-level value is not an object")?;
    let hooks = root.entry("hooks").or_insert_with(|| json!({}));
    let hooks = hooks
        .as_object_mut()
        .ok_or("`hooks` exists but is not an object")?;
    let groups = hooks.entry(event).or_insert_with(|| json!([]));
    let groups = groups
        .as_array_mut()
        .ok_or_else(|| format!("`hooks.{event}` exists but is not an array"))?;

    let mut found = false;
    let mut changed = false;
    groups.retain_mut(|group| {
        let Some(inner) = group.get_mut("hooks").and_then(Value::as_array_mut) else {
            return true;
        };
        let mut emptied_by_us = false;
        let mut i = 0;
        while i < inner.len() {
            if !is_ours_entry(&inner[i]) {
                i += 1;
            } else if !found {
                found = true;
                if inner[i] != *entry {
                    inner[i] = entry.clone();
                    changed = true;
                }
                i += 1;
            } else {
                inner.remove(i);
                changed = true;
                emptied_by_us = inner.is_empty();
            }
        }
        !emptied_by_us
    });
    if !found {
        groups.push(json!({"hooks": [entry.clone()]}));
        return Ok(Change::Added);
    }
    Ok(if changed {
        Change::Replaced
    } else {
        Change::Same
    })
}

/// Remove our entries; drop groups / the event array / `hooks` that this left empty.
fn strip_hook(doc: &mut Value, event: &str) -> bool {
    let Some(hooks) = doc.get_mut("hooks").and_then(Value::as_object_mut) else {
        return false;
    };
    let Some(groups) = hooks.get_mut(event).and_then(Value::as_array_mut) else {
        return false;
    };
    let mut removed = false;
    groups.retain_mut(|group| {
        let Some(inner) = group.get_mut("hooks").and_then(Value::as_array_mut) else {
            return true;
        };
        let before = inner.len();
        inner.retain(|h| !is_ours_entry(h));
        let n = before - inner.len();
        removed |= n > 0;
        !(n > 0 && inner.is_empty())
    });
    if !removed {
        return false;
    }
    if groups.is_empty() {
        hooks.remove(event);
    }
    if hooks.is_empty() {
        if let Some(root) = doc.as_object_mut() {
            root.remove("hooks");
        }
    }
    true
}

// ---------------------------------------------------------------------------
// install / remove
// ---------------------------------------------------------------------------

fn base_result(agent: Agent, home: &Path, action: Action) -> SetupResult {
    SetupResult {
        agent,
        path: config_file(agent, home),
        action,
        backup: None,
        notes: Vec::new(),
    }
}

fn skipped(mut res: SetupResult, reason: &str, snippet: &str) -> SetupResult {
    res.action = Action::Skipped(format!(
        "{} left untouched: {reason}. Add this by hand:\n{snippet}",
        res.path.display()
    ));
    res
}

pub fn install(agent: Agent, home: &Path, exe: &Path) -> Result<SetupResult> {
    let dir = agent_dir(agent, home);
    if !dir.is_dir() {
        return Ok(base_result(
            agent,
            home,
            Action::Skipped(format!(
                "{} does not appear to be installed (no {})",
                agent.display(),
                agent.dir_label()
            )),
        ));
    }
    match agent {
        Agent::OpenCode => install_opencode(home, exe),
        _ => install_hook(agent, home, exe),
    }
}

fn install_hook(agent: Agent, home: &Path, exe: &Path) -> Result<SetupResult> {
    let mut res = base_result(agent, home, Action::Unchanged);
    let snippet = manual_snippet(agent, exe);
    let exe_text = forward_slashes(exe);
    if has_unquotable_char(&exe_text) {
        return Ok(skipped(
            res,
            &format!(
                "the program path {exe_text} contains a character (\" $ ` % or a line break) \
                 that cannot be quoted safely in a hook command; move screensense to a plainer path"
            ),
            &snippet,
        ));
    }
    let cmd = hook_command(exe);
    let Some((event, entry)) = hook_entry(agent, &cmd) else {
        return Ok(res);
    };
    let (mut doc, existed) = match load(&res.path)? {
        Loaded::Missing => (Value::Object(Map::new()), false),
        Loaded::Parsed(v) => (v, true),
        Loaded::Bad(why) => return Ok(skipped(res, &why, &snippet)),
    };
    match apply_hook(&mut doc, event, &entry) {
        Err(why) => return Ok(skipped(res, &why, &snippet)),
        Ok(Change::Same) => res.action = Action::Unchanged,
        Ok(change) => {
            res.backup = commit(&res.path, &doc, existed)?;
            res.action = if change == Change::Replaced {
                Action::Updated
            } else {
                Action::Installed
            };
        }
    }
    if cmd.starts_with('"') {
        res.notes.push(
            "the program path needs quoting, which PowerShell does not accept in a hook command; \
             if the hook shows an error under PowerShell, install screensense to a path without spaces"
                .into(),
        );
    }
    match agent {
        Agent::Claude => {
            if install_skill(home, &mut res)? && res.action == Action::Unchanged {
                res.action = Action::Updated;
            }
            res.notes.push(format!(
                "for on-demand tools too, run: claude mcp add screensense -- \"{exe_text}\" mcp"
            ));
        }
        Agent::Codex => res
            .notes
            .push("run /hooks in Codex once and mark the screensense hook as trusted".into()),
        Agent::Gemini => res
            .notes
            .push("Gemini CLI hook support is experimental".into()),
        Agent::OpenCode => {}
    }
    Ok(res)
}

fn install_opencode(home: &Path, exe: &Path) -> Result<SetupResult> {
    let mut res = base_result(Agent::OpenCode, home, Action::Unchanged);
    let snippet = manual_snippet(Agent::OpenCode, exe);
    let jsonc = agent_dir(Agent::OpenCode, home).join("opencode.jsonc");
    if jsonc.exists() {
        res.path = jsonc;
        return Ok(skipped(
            res,
            "an opencode.jsonc exists and ScreenSense does not edit JSONC (it would lose your comments)",
            &snippet,
        ));
    }
    let entry = opencode_entry(exe);
    let (mut doc, existed) = match load(&res.path)? {
        Loaded::Missing => {
            let mut m = Map::new();
            m.insert("$schema".into(), Value::String(OPENCODE_SCHEMA.into()));
            (Value::Object(m), false)
        }
        Loaded::Parsed(v) => (v, true),
        Loaded::Bad(why) => return Ok(skipped(res, &why, &snippet)),
    };
    let change = {
        let Some(root) = doc.as_object_mut() else {
            return Ok(skipped(
                res,
                "the top-level value is not an object",
                &snippet,
            ));
        };
        let mcp = root.entry("mcp").or_insert_with(|| json!({}));
        let Some(mcp) = mcp.as_object_mut() else {
            return Ok(skipped(res, "`mcp` exists but is not an object", &snippet));
        };
        match mcp.get("screensense") {
            Some(v) if *v == entry => Change::Same,
            Some(_) => {
                mcp.insert("screensense".into(), entry);
                Change::Replaced
            }
            None => {
                mcp.insert("screensense".into(), entry);
                Change::Added
            }
        }
    };
    if change != Change::Same {
        res.backup = commit(&res.path, &doc, existed)?;
        res.action = if change == Change::Replaced {
            Action::Updated
        } else {
            Action::Installed
        };
    }
    res.notes.push(
        "OpenCode has no prompt hook; the agent calls the screen_* tools when it needs them. \
         Add a line to your AGENTS.md if you want it to check the screen proactively, for example: \
         \"When I refer to something on my screen or an error I just hit, call the screensense \
         screen_now or screen_errors tool before asking me to paste it.\""
            .into(),
    );
    Ok(res)
}

pub fn remove(agent: Agent, home: &Path) -> Result<SetupResult> {
    let mut res = base_result(agent, home, Action::NotInstalled);
    if !agent_dir(agent, home).is_dir() {
        return Ok(res);
    }
    if agent == Agent::OpenCode {
        return remove_opencode(res, home);
    }
    let Some((event, _)) = hook_entry(agent, "") else {
        return Ok(res);
    };
    match load(&res.path)? {
        Loaded::Missing => {}
        Loaded::Bad(why) => {
            res.action = Action::Skipped(format!(
                "{} left untouched: {why}. Remove the screensense entry under hooks.{event} by hand.",
                res.path.display()
            ));
            return Ok(res);
        }
        Loaded::Parsed(mut doc) => {
            if strip_hook(&mut doc, event) {
                res.backup = commit(&res.path, &doc, true)?;
                res.action = Action::Removed;
            }
        }
    }
    if agent == Agent::Claude {
        remove_skill(home, &mut res)?;
    }
    Ok(res)
}

fn remove_opencode(mut res: SetupResult, home: &Path) -> Result<SetupResult> {
    let jsonc = agent_dir(Agent::OpenCode, home).join("opencode.jsonc");
    if jsonc.exists() {
        res.notes.push(format!(
            "{} exists; ScreenSense never edits JSONC, so remove its mcp.screensense entry there by hand if you added one",
            jsonc.display()
        ));
    }
    match load(&res.path)? {
        Loaded::Missing => {}
        Loaded::Bad(why) => {
            res.action = Action::Skipped(format!(
                "{} left untouched: {why}. Remove mcp.screensense by hand.",
                res.path.display()
            ));
        }
        Loaded::Parsed(mut doc) => {
            let mut removed = false;
            if let Some(mcp) = doc.get_mut("mcp").and_then(Value::as_object_mut) {
                removed = mcp.remove("screensense").is_some();
                if removed && mcp.is_empty() {
                    if let Some(root) = doc.as_object_mut() {
                        root.remove("mcp");
                    }
                }
            }
            if removed {
                res.backup = commit(&res.path, &doc, true)?;
                res.action = Action::Removed;
            }
        }
    }
    Ok(res)
}

// ---------------------------------------------------------------------------
// Claude Code skill
// ---------------------------------------------------------------------------

fn skill_path(home: &Path) -> PathBuf {
    home.join(".claude")
        .join("skills")
        .join("screen")
        .join("SKILL.md")
}

/// Returns true when the file was created or replaced.
fn install_skill(home: &Path, res: &mut SetupResult) -> Result<bool> {
    let path = skill_path(home);
    let want = skill_markdown().as_bytes();
    match fs::read(&path) {
        Ok(cur) if cur == want => Ok(false),
        Ok(_) => {
            let bak = backup(&path)?;
            write_atomic(&path, want)?;
            res.notes.push(format!(
                "replaced the /screen skill at {} (previous file saved as {})",
                path.display(),
                bak.display()
            ));
            Ok(true)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            write_atomic(&path, want)?;
            res.notes
                .push(format!("installed the /screen skill at {}", path.display()));
            Ok(true)
        }
        Err(e) => Err(e.into()),
    }
}

fn remove_skill(home: &Path, res: &mut SetupResult) -> Result<()> {
    let path = skill_path(home);
    match fs::read(&path) {
        Ok(cur) if cur == skill_markdown().as_bytes() => {
            fs::remove_file(&path)?;
            if let Some(dir) = path.parent() {
                let _ = fs::remove_dir(dir); // only succeeds when empty
            }
            res.notes
                .push(format!("removed the /screen skill ({})", path.display()));
            res.action = Action::Removed;
        }
        Ok(_) => res.notes.push(format!(
            "left {} alone: it differs from the bundled skill (modified by you?)",
            path.display()
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    Ok(())
}

/// The Claude Code skill text (also stored in the repo at `skills/screen/SKILL.md`).
pub fn skill_markdown() -> &'static str {
    include_str!("../../../skills/screen/SKILL.md")
}

// ---------------------------------------------------------------------------
// The prompt hook's I/O
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Default)]
pub struct HookInput {
    pub session_id: Option<String>,
    pub prompt: String,
    pub hook_event_name: String,
    pub cwd: Option<String>,
}

/// Tolerant: BOM, non-JSON, missing or wrongly typed fields give defaults. A broken
/// hook input must never become an error that blocks the user's prompt.
pub fn parse_hook_input(stdin: &str) -> HookInput {
    let text = stdin.trim_start_matches('\u{feff}').trim();
    let Ok(Value::Object(m)) = serde_json::from_str::<Value>(text) else {
        return HookInput::default();
    };
    let get = |k: &str| m.get(k).and_then(Value::as_str).map(str::to_owned);
    HookInput {
        session_id: get("session_id"),
        prompt: get("prompt").unwrap_or_default(),
        hook_event_name: get("hook_event_name").unwrap_or_default(),
        cwd: get("cwd"),
    }
}

/// Claude Code replaces `additionalContext` longer than 10,000 characters with a file
/// path and a preview; stay clearly below. Measured in UTF-16 code units (what a
/// JavaScript `.length` sees), the stricter reading of "characters".
const CONTEXT_LIMIT: usize = 9_500;
/// Room kept for the lines after the closing fence (the "(More: ...)" hint).
const TAIL_LIMIT: usize = 400;

fn units(s: &str) -> usize {
    s.encode_utf16().count()
}

/// The longest prefix of `s` that fits in `n` UTF-16 units, cut on a char boundary.
fn take_units(s: &str, n: usize) -> &str {
    let mut used = 0;
    for (i, c) in s.char_indices() {
        used += c.len_utf16();
        if used > n {
            return &s[..i];
        }
    }
    s
}

/// Cut `context` to the limit on a line boundary, keeping the closing fence.
fn cap_context(context: &str) -> String {
    if units(context) <= CONTEXT_LIMIT {
        return context.to_string();
    }
    const MARK: &str = "(truncated)";
    let lines: Vec<&str> = context.split('\n').collect();
    let fence = lines.iter().rposition(|l| l.starts_with("<<<END DATA"));
    let (head, tail): (&[&str], Vec<&str>) = match fence {
        Some(i) => {
            let fence_line = take_units(lines[i], 200);
            let mut tail = vec![fence_line];
            let mut used = units(fence_line);
            for l in &lines[i + 1..] {
                if used + 1 + units(l) > TAIL_LIMIT {
                    break;
                }
                used += 1 + units(l);
                tail.push(l);
            }
            (&lines[..i], tail)
        }
        None => (&lines[..], Vec::new()),
    };
    let tail_text = tail.join("\n");
    let reserved = units(MARK)
        + 1
        + if tail.is_empty() {
            0
        } else {
            units(&tail_text) + 1
        };
    let budget = CONTEXT_LIMIT.saturating_sub(reserved);
    let mut out = String::new();
    let mut used = 0;
    for l in head {
        let cost = units(l) + 1;
        if used + cost > budget {
            break;
        }
        out.push_str(l);
        out.push('\n');
        used += cost;
    }
    if out.is_empty() {
        if let Some(first) = head.first() {
            out.push_str(take_units(first, budget.saturating_sub(1)));
            out.push('\n');
        }
    }
    out.push_str(MARK);
    if !tail.is_empty() {
        out.push('\n');
        out.push_str(&tail_text);
    }
    out
}

/// `s` as a JSON string literal with every non-ASCII char written as `\uXXXX`.
fn json_ascii_string(s: &str) -> String {
    let quoted = serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into());
    let mut out = String::with_capacity(quoted.len());
    for c in quoted.chars() {
        if c.is_ascii() {
            out.push(c);
        } else {
            let mut buf = [0u16; 2];
            for u in c.encode_utf16(&mut buf) {
                out.push_str(&format!("\\u{u:04x}"));
            }
        }
    }
    out
}

/// One line of JSON for stdout. `event` is echoed when it is `UserPromptSubmit` or
/// `BeforeAgent`, otherwise `UserPromptSubmit`. Over-long context is cut on a line
/// boundary with the closing `<<<END DATA` fence kept, so the model never sees an
/// unclosed fence. Pure ASCII, so any console code page passes it through.
pub fn hook_output(event: &str, context: &str) -> String {
    let event = match event {
        "UserPromptSubmit" | "BeforeAgent" => event,
        _ => "UserPromptSubmit",
    };
    format!(
        "{{\"hookSpecificOutput\":{{\"hookEventName\":{},\"additionalContext\":{}}}}}",
        json_ascii_string(event),
        json_ascii_string(&cap_context(context))
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    const EXE: &str = "/opt/ss/screensense";

    fn exe() -> &'static Path {
        Path::new(EXE)
    }

    /// A home directory in which the given agents' config dirs exist.
    fn home_with(agents: &[Agent]) -> TempDir {
        let t = TempDir::new().unwrap();
        for a in agents {
            fs::create_dir_all(agent_dir(*a, t.path())).unwrap();
        }
        t
    }

    fn read_json(p: &Path) -> Value {
        serde_json::from_str(&fs::read_to_string(p).unwrap()).unwrap()
    }

    fn backups(path: &Path) -> Vec<PathBuf> {
        let prefix = format!(
            "{}.bak-screensense-",
            path.file_name().unwrap().to_string_lossy()
        );
        let mut v: Vec<PathBuf> = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .is_some_and(|n| n.to_string_lossy().starts_with(&prefix))
            })
            .collect();
        v.sort();
        v
    }

    /// How many of our hook entries sit under hooks.<event>.
    fn ours_in(doc: &Value, event: &str) -> usize {
        doc["hooks"][event]
            .as_array()
            .map(|groups| {
                groups
                    .iter()
                    .filter_map(|g| g["hooks"].as_array())
                    .flatten()
                    .filter(|h| is_ours_entry(h))
                    .count()
            })
            .unwrap_or(0)
    }

    fn event_of(a: Agent) -> &'static str {
        hook_entry(a, "x hook").unwrap().0
    }

    const HOOK_AGENTS: [Agent; 3] = [Agent::Claude, Agent::Codex, Agent::Gemini];

    #[test]
    fn agent_names_parse_and_display() {
        for a in Agent::ALL {
            assert_eq!(Agent::parse(a.name()), Some(a));
            assert_eq!(Agent::parse(&a.name().to_uppercase()), Some(a));
        }
        assert_eq!(Agent::parse("Claude-Code"), Some(Agent::Claude));
        assert_eq!(Agent::parse("claude_code"), Some(Agent::Claude));
        assert_eq!(Agent::parse("nope"), None);
        assert_eq!(Agent::OpenCode.display(), "OpenCode");
        assert_eq!(Agent::Codex.display(), "Codex CLI");
    }

    #[test]
    fn config_file_and_detect() {
        let home = Path::new("/h");
        assert_eq!(
            config_file(Agent::Claude, home),
            Path::new("/h/.claude/settings.json")
        );
        assert_eq!(
            config_file(Agent::Codex, home),
            Path::new("/h/.codex/hooks.json")
        );
        assert_eq!(
            config_file(Agent::Gemini, home),
            Path::new("/h/.gemini/settings.json")
        );
        assert_eq!(
            config_file(Agent::OpenCode, home),
            Path::new("/h/.config/opencode/opencode.json")
        );
        let t = home_with(&[Agent::Gemini, Agent::OpenCode]);
        assert_eq!(detect(t.path()), vec![Agent::Gemini, Agent::OpenCode]);
        assert!(detect(TempDir::new().unwrap().path()).is_empty());
    }

    #[test]
    fn install_without_agent_dir_is_skipped_and_creates_nothing() {
        let t = TempDir::new().unwrap();
        for a in Agent::ALL {
            let r = install(a, t.path(), exe()).unwrap();
            match &r.action {
                Action::Skipped(m) => {
                    assert!(m.contains("does not appear to be installed"), "{m}");
                    assert!(m.contains(a.display()), "{m}");
                }
                other => panic!("{a:?}: {other:?}"),
            }
            assert!(!r.path.exists());
        }
        assert_eq!(fs::read_dir(t.path()).unwrap().count(), 0);
    }

    #[test]
    fn fresh_install_writes_exactly_one_entry_with_the_right_shape() {
        for a in HOOK_AGENTS {
            let t = home_with(&[a]);
            let r = install(a, t.path(), exe()).unwrap();
            assert_eq!(r.action, Action::Installed, "{a:?}");
            assert_eq!(r.backup, None);
            let doc = read_json(&r.path);
            let ev = event_of(a);
            assert_eq!(ours_in(&doc, ev), 1, "{a:?}");
            let groups = doc["hooks"][ev].as_array().unwrap();
            assert_eq!(groups.len(), 1);
            let entry = &groups[0]["hooks"][0];
            assert_eq!(entry["type"], "command");
            assert_eq!(entry["command"], "/opt/ss/screensense hook");
            assert!(groups[0].get("matcher").is_none());
            let text = fs::read_to_string(&r.path).unwrap();
            assert!(text.ends_with("}\n"));
            assert!(text.contains("\n  \"hooks\""), "2-space indent");
            assert!(!r.path.with_extension("json.screensense.tmp").exists());
        }
    }

    #[test]
    fn claude_entry_uses_seconds_timeout() {
        let t = home_with(&[Agent::Claude]);
        let r = install(Agent::Claude, t.path(), exe()).unwrap();
        let e = &read_json(&r.path)["hooks"]["UserPromptSubmit"][0]["hooks"][0];
        assert_eq!(e["timeout"], 5);
        assert!(e.get("commandWindows").is_none());
    }

    #[test]
    fn codex_entry_matches_the_reference_shape() {
        let t = home_with(&[Agent::Codex]);
        let r = install(Agent::Codex, t.path(), exe()).unwrap();
        assert_eq!(r.path, t.path().join(".codex/hooks.json"));
        let e = &read_json(&r.path)["hooks"]["UserPromptSubmit"][0]["hooks"][0];
        assert_eq!(e["command"], "/opt/ss/screensense hook");
        assert_eq!(e["commandWindows"], "/opt/ss/screensense hook");
        assert_eq!(e["timeout"], 10);
        assert_eq!(e["statusMessage"], "screensense");
        assert!(r
            .notes
            .iter()
            .any(|n| n.contains("/hooks") && n.contains("trusted")));
    }

    #[test]
    fn gemini_entry_matches_the_reference_shape() {
        let t = home_with(&[Agent::Gemini]);
        let r = install(Agent::Gemini, t.path(), exe()).unwrap();
        let doc = read_json(&r.path);
        assert!(doc["hooks"].get("UserPromptSubmit").is_none());
        let e = &doc["hooks"]["BeforeAgent"][0]["hooks"][0];
        assert_eq!(e["name"], "screensense");
        assert_eq!(e["timeout"], 5000);
        assert!(r.notes.iter().any(|n| n.contains("experimental")));
    }

    #[test]
    fn opencode_install_creates_mcp_entry_and_schema() {
        let t = home_with(&[Agent::OpenCode]);
        let r = install(Agent::OpenCode, t.path(), exe()).unwrap();
        assert_eq!(r.action, Action::Installed);
        assert_eq!(r.path, t.path().join(".config/opencode/opencode.json"));
        let doc = read_json(&r.path);
        assert_eq!(doc["$schema"], "https://opencode.ai/config.json");
        let e = &doc["mcp"]["screensense"];
        assert_eq!(e["type"], "local");
        assert_eq!(e["command"], json!([EXE, "mcp"]));
        assert_eq!(e["enabled"], true);
        assert!(r.notes.iter().any(|n| n.contains("AGENTS.md")));
        // an existing file does not get a $schema added
        let t2 = home_with(&[Agent::OpenCode]);
        let p = config_file(Agent::OpenCode, t2.path());
        fs::write(&p, "{\"theme\":\"dark\"}").unwrap();
        install(Agent::OpenCode, t2.path(), exe()).unwrap();
        let d2 = read_json(&p);
        assert!(d2.get("$schema").is_none());
        assert_eq!(d2["theme"], "dark");
    }

    #[test]
    fn second_install_is_unchanged_and_makes_no_backup() {
        for a in Agent::ALL {
            let t = home_with(&[a]);
            let first = install(a, t.path(), exe()).unwrap();
            let before = fs::read(&first.path).unwrap();
            let again = install(a, t.path(), exe()).unwrap();
            assert_eq!(again.action, Action::Unchanged, "{a:?}");
            assert_eq!(again.backup, None);
            assert!(backups(&first.path).is_empty(), "{a:?}");
            assert_eq!(fs::read(&first.path).unwrap(), before);
        }
    }

    #[test]
    fn install_with_a_different_exe_updates_in_place_with_a_backup() {
        for a in Agent::ALL {
            let t = home_with(&[a]);
            let first = install(a, t.path(), exe()).unwrap();
            let old = fs::read(&first.path).unwrap();
            let r = install(a, t.path(), Path::new("/usr/local/bin/screensense")).unwrap();
            assert_eq!(r.action, Action::Updated, "{a:?}");
            let bak = r.backup.clone().unwrap();
            assert_eq!(fs::read(&bak).unwrap(), old, "backup holds the old file");
            assert_eq!(backups(&r.path).len(), 1);
            let doc = read_json(&r.path);
            if a == Agent::OpenCode {
                assert_eq!(
                    doc["mcp"]["screensense"]["command"][0],
                    "/usr/local/bin/screensense"
                );
            } else {
                assert_eq!(ours_in(&doc, event_of(a)), 1);
                assert_eq!(
                    doc["hooks"][event_of(a)][0]["hooks"][0]["command"],
                    "/usr/local/bin/screensense hook"
                );
            }
        }
    }

    #[test]
    fn backup_name_has_the_documented_shape() {
        let t = home_with(&[Agent::Claude]);
        let p = config_file(Agent::Claude, t.path());
        fs::write(&p, "{}").unwrap();
        let r = install(Agent::Claude, t.path(), exe()).unwrap();
        let name = r
            .backup
            .unwrap()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let re = regex::Regex::new(r"^settings\.json\.bak-screensense-\d{8}-\d{6}$").unwrap();
        assert!(re.is_match(&name), "{name}");
    }

    #[test]
    fn other_content_is_preserved_on_install_and_remove() {
        let t = home_with(&[Agent::Claude]);
        let p = config_file(Agent::Claude, t.path());
        let original = json!({
            "model": "opus",
            "env": {"A": "1"},
            "permissions": {"allow": ["Bash(ls)"]},
            "hooks": {
                "PreToolUse": [{"matcher": "Bash", "hooks": [{"type": "command", "command": "check.sh"}]}],
                "UserPromptSubmit": [
                    {"hooks": [{"type": "command", "command": "echo screensense hook"},
                               {"type": "command", "command": "desktop-sense hook"}]},
                    {"matcher": "", "hooks": [{"type": "command", "command": "other.sh", "timeout": 3}]}
                ]
            }
        });
        fs::write(&p, serde_json::to_string_pretty(&original).unwrap()).unwrap();
        let r = install(Agent::Claude, t.path(), exe()).unwrap();
        assert_eq!(r.action, Action::Installed);
        assert!(r.backup.is_some());
        let doc = read_json(&p);
        assert_eq!(doc["model"], "opus");
        assert_eq!(doc["env"], original["env"]);
        assert_eq!(doc["permissions"], original["permissions"]);
        assert_eq!(doc["hooks"]["PreToolUse"], original["hooks"]["PreToolUse"]);
        let groups = doc["hooks"]["UserPromptSubmit"].as_array().unwrap();
        assert_eq!(groups.len(), 3);
        assert_eq!(groups[0], original["hooks"]["UserPromptSubmit"][0]);
        assert_eq!(groups[1], original["hooks"]["UserPromptSubmit"][1]);
        assert_eq!(ours_in(&doc, "UserPromptSubmit"), 1);
        // remove puts everything back exactly as it was (semantically)
        let r = remove(Agent::Claude, t.path()).unwrap();
        assert_eq!(r.action, Action::Removed);
        assert_eq!(read_json(&p), original);
    }

    #[test]
    fn our_entry_inside_a_shared_group_is_replaced_in_place() {
        let t = home_with(&[Agent::Claude]);
        let p = config_file(Agent::Claude, t.path());
        let doc = json!({"hooks": {"UserPromptSubmit": [{"matcher": "x", "hooks": [
            {"type": "command", "command": "first.sh"},
            {"type": "command", "command": "\"/old/path/screensense.exe\" hook", "timeout": 99},
            {"type": "command", "command": "last.sh"}]}]}});
        fs::write(&p, doc.to_string()).unwrap();
        let r = install(Agent::Claude, t.path(), exe()).unwrap();
        assert_eq!(r.action, Action::Updated);
        let got = read_json(&p);
        let inner = got["hooks"]["UserPromptSubmit"][0]["hooks"]
            .as_array()
            .unwrap();
        assert_eq!(inner.len(), 3);
        assert_eq!(inner[0]["command"], "first.sh");
        assert_eq!(inner[1]["command"], "/opt/ss/screensense hook");
        assert_eq!(inner[1]["timeout"], 5);
        assert_eq!(inner[2]["command"], "last.sh");
        assert_eq!(got["hooks"]["UserPromptSubmit"][0]["matcher"], "x");
    }

    #[test]
    fn duplicate_entries_of_ours_collapse_to_one() {
        let t = home_with(&[Agent::Claude]);
        let p = config_file(Agent::Claude, t.path());
        let dup = json!({"type": "command", "command": "/a/screensense hook"});
        let doc = json!({"hooks": {"UserPromptSubmit": [{"hooks": [dup]}, {"hooks": [dup]}, {"hooks": [{"type":"command","command":"keep.sh"}]}]}});
        fs::write(&p, doc.to_string()).unwrap();
        let r = install(Agent::Claude, t.path(), exe()).unwrap();
        assert_eq!(r.action, Action::Updated);
        let got = read_json(&p);
        assert_eq!(ours_in(&got, "UserPromptSubmit"), 1);
        let groups = got["hooks"]["UserPromptSubmit"].as_array().unwrap();
        assert_eq!(groups.len(), 2, "the emptied duplicate group is dropped");
        assert_eq!(groups[1]["hooks"][0]["command"], "keep.sh");
    }

    #[test]
    fn opencode_keeps_other_servers_and_settings() {
        let t = home_with(&[Agent::OpenCode]);
        let p = config_file(Agent::OpenCode, t.path());
        let original =
            json!({"theme": "x", "mcp": {"other": {"type": "remote", "url": "http://x"}}});
        fs::write(&p, original.to_string()).unwrap();
        install(Agent::OpenCode, t.path(), exe()).unwrap();
        let doc = read_json(&p);
        assert_eq!(doc["theme"], "x");
        assert_eq!(doc["mcp"]["other"], original["mcp"]["other"]);
        assert!(doc["mcp"]["screensense"].is_object());
        let r = remove(Agent::OpenCode, t.path()).unwrap();
        assert_eq!(r.action, Action::Removed);
        assert_eq!(read_json(&p), original);
    }

    #[test]
    fn invalid_json_is_never_touched() {
        let bad: [&[u8]; 6] = [
            b"{ not json",
            b"{\n  // a comment\n  \"hooks\": {}\n}",
            b"{\"a\": 1,}",
            b"[1, 2]",
            b"\"str\"",
            b"\xff\xfe\x00",
        ];
        for a in Agent::ALL {
            for content in bad {
                let t = home_with(&[a]);
                let p = config_file(a, t.path());
                fs::write(&p, content).unwrap();
                let r = install(a, t.path(), exe()).unwrap();
                match &r.action {
                    Action::Skipped(m) => {
                        assert!(m.contains("by hand"), "{m}");
                        assert!(m.contains("screensense"), "{m}");
                    }
                    other => panic!("{a:?} {content:?}: {other:?}"),
                }
                assert_eq!(
                    fs::read(&p).unwrap(),
                    content,
                    "{a:?} must stay byte-identical"
                );
                assert!(backups(&p).is_empty());
                let r = remove(a, t.path()).unwrap();
                assert!(
                    matches!(r.action, Action::Skipped(_)),
                    "{a:?}: {:?}",
                    r.action
                );
                assert_eq!(fs::read(&p).unwrap(), content);
            }
        }
    }

    #[test]
    fn skipped_message_carries_the_snippet() {
        let t = home_with(&[Agent::Codex]);
        fs::write(config_file(Agent::Codex, t.path()), "oops").unwrap();
        let r = install(Agent::Codex, t.path(), exe()).unwrap();
        let Action::Skipped(m) = r.action else {
            panic!()
        };
        assert!(m.contains(&manual_snippet(Agent::Codex, exe())));
        assert!(m.contains("UserPromptSubmit"));
    }

    #[test]
    fn opencode_jsonc_is_never_edited() {
        let t = home_with(&[Agent::OpenCode]);
        let dir = agent_dir(Agent::OpenCode, t.path());
        let jsonc = dir.join("opencode.jsonc");
        fs::write(&jsonc, "{ // mine\n}").unwrap();
        let r = install(Agent::OpenCode, t.path(), exe()).unwrap();
        let Action::Skipped(m) = &r.action else {
            panic!("{:?}", r.action)
        };
        assert!(m.contains("opencode.jsonc"));
        assert!(m.contains("\"mcp\""));
        assert_eq!(fs::read_to_string(&jsonc).unwrap(), "{ // mine\n}");
        assert!(!dir.join("opencode.json").exists());
        // also when a plain opencode.json sits next to it
        fs::write(dir.join("opencode.json"), "{}").unwrap();
        let r = install(Agent::OpenCode, t.path(), exe()).unwrap();
        assert!(matches!(r.action, Action::Skipped(_)));
        assert_eq!(fs::read_to_string(dir.join("opencode.json")).unwrap(), "{}");
        // remove: nothing of ours, but tell the user about the jsonc
        let r = remove(Agent::OpenCode, t.path()).unwrap();
        assert_eq!(r.action, Action::NotInstalled);
        assert!(r.notes.iter().any(|n| n.contains("opencode.jsonc")));
    }

    #[test]
    fn wrong_types_are_skipped_without_writing() {
        let cases: [(Agent, &str); 7] = [
            (Agent::Claude, r#"{"hooks": []}"#),
            (Agent::Claude, r#"{"hooks": {"UserPromptSubmit": {}}}"#),
            (Agent::Claude, r#"{"hooks": null}"#),
            (Agent::Codex, r#"{"hooks": "x"}"#),
            (Agent::Gemini, r#"{"hooks": {"BeforeAgent": 3}}"#),
            (Agent::OpenCode, r#"{"mcp": []}"#),
            (Agent::OpenCode, r#"{"mcp": "x"}"#),
        ];
        for (a, content) in cases {
            let t = home_with(&[a]);
            let p = config_file(a, t.path());
            fs::write(&p, content).unwrap();
            let r = install(a, t.path(), exe()).unwrap();
            assert!(
                matches!(r.action, Action::Skipped(_)),
                "{a:?} {content}: {:?}",
                r.action
            );
            assert_eq!(fs::read_to_string(&p).unwrap(), content);
            assert!(backups(&p).is_empty());
        }
    }

    #[test]
    fn foreign_groups_of_odd_shape_are_left_alone() {
        let t = home_with(&[Agent::Claude]);
        let p = config_file(Agent::Claude, t.path());
        let doc =
            json!({"hooks": {"UserPromptSubmit": ["weird", {"hooks": "nope"}, {"matcher": "m"}]}});
        fs::write(&p, doc.to_string()).unwrap();
        let r = install(Agent::Claude, t.path(), exe()).unwrap();
        assert_eq!(r.action, Action::Installed);
        let got = read_json(&p);
        let groups = got["hooks"]["UserPromptSubmit"].as_array().unwrap();
        assert_eq!(groups.len(), 4);
        assert_eq!(
            &groups[..3],
            &doc["hooks"]["UserPromptSubmit"].as_array().unwrap()[..]
        );
    }

    #[test]
    fn bom_and_empty_files_are_handled() {
        let t = home_with(&[Agent::Claude]);
        let p = config_file(Agent::Claude, t.path());
        fs::write(&p, "\u{feff}{\"model\": \"m\"}").unwrap();
        let r = install(Agent::Claude, t.path(), exe()).unwrap();
        assert_eq!(r.action, Action::Installed);
        assert_eq!(read_json(&p)["model"], "m");
        let t = home_with(&[Agent::Gemini]);
        let p = config_file(Agent::Gemini, t.path());
        fs::write(&p, "  \n").unwrap();
        let r = install(Agent::Gemini, t.path(), exe()).unwrap();
        assert_eq!(r.action, Action::Installed);
        assert_eq!(ours_in(&read_json(&p), "BeforeAgent"), 1);
    }

    #[test]
    fn remove_deletes_only_ours_and_drops_empty_containers() {
        for a in HOOK_AGENTS {
            let t = home_with(&[a]);
            let installed = install(a, t.path(), exe()).unwrap();
            let r = remove(a, t.path()).unwrap();
            assert_eq!(r.action, Action::Removed, "{a:?}");
            assert!(r.backup.is_some());
            assert!(installed.path.exists(), "the file is never deleted");
            assert_eq!(read_json(&installed.path), json!({}), "{a:?}");
            let again = remove(a, t.path()).unwrap();
            assert_eq!(again.action, Action::NotInstalled);
            assert_eq!(again.backup, None);
        }
    }

    #[test]
    fn remove_keeps_a_group_that_still_holds_other_hooks() {
        let t = home_with(&[Agent::Gemini]);
        let p = config_file(Agent::Gemini, t.path());
        let doc = json!({"hooks": {"BeforeAgent": [{"hooks": [
            {"type": "command", "command": "\"C:/x/ScreenSense.EXE\" hook"},
            {"type": "command", "command": "mine.sh"}]}]}, "other": true});
        fs::write(&p, doc.to_string()).unwrap();
        let r = remove(Agent::Gemini, t.path()).unwrap();
        assert_eq!(r.action, Action::Removed);
        assert_eq!(
            read_json(&p),
            json!({"hooks": {"BeforeAgent": [{"hooks": [{"type": "command", "command": "mine.sh"}]}]}, "other": true})
        );
    }

    #[test]
    fn remove_is_not_installed_when_absent() {
        let t = home_with(&Agent::ALL);
        for a in Agent::ALL {
            let r = remove(a, t.path()).unwrap();
            assert_eq!(r.action, Action::NotInstalled, "{a:?}: no file");
            assert!(!r.path.exists());
        }
        let p = config_file(Agent::Claude, t.path());
        fs::write(
            &p,
            r#"{"hooks": {"UserPromptSubmit": [{"hooks": [{"type":"command","command":"x"}]}]}}"#,
        )
        .unwrap();
        let before = fs::read(&p).unwrap();
        let r = remove(Agent::Claude, t.path()).unwrap();
        assert_eq!(r.action, Action::NotInstalled);
        assert_eq!(fs::read(&p).unwrap(), before);
        assert!(backups(&p).is_empty());
        let none = TempDir::new().unwrap();
        assert_eq!(
            remove(Agent::Codex, none.path()).unwrap().action,
            Action::NotInstalled
        );
    }

    #[test]
    fn claude_skill_is_installed_updated_and_removed() {
        let t = home_with(&[Agent::Claude]);
        let skill = skill_path(t.path());
        let r = install(Agent::Claude, t.path(), exe()).unwrap();
        assert_eq!(fs::read_to_string(&skill).unwrap(), skill_markdown());
        assert!(r.notes.iter().any(|n| n.contains("/screen skill")));
        assert!(r
            .notes
            .iter()
            .any(|n| n.contains("claude mcp add screensense")));
        // unchanged second time
        let r = install(Agent::Claude, t.path(), exe()).unwrap();
        assert_eq!(r.action, Action::Unchanged);
        assert!(backups(&skill).is_empty());
        // a different file there is backed up first
        fs::write(&skill, "my own skill").unwrap();
        let r = install(Agent::Claude, t.path(), exe()).unwrap();
        assert_eq!(r.action, Action::Updated, "the skill changed");
        assert_eq!(r.backup, None, "settings.json was not modified");
        let baks = backups(&skill);
        assert_eq!(baks.len(), 1);
        assert_eq!(fs::read_to_string(&baks[0]).unwrap(), "my own skill");
        assert_eq!(fs::read_to_string(&skill).unwrap(), skill_markdown());
        // a missing skill alone is repaired
        fs::remove_file(&skill).unwrap();
        let r = install(Agent::Claude, t.path(), exe()).unwrap();
        assert_eq!(r.action, Action::Updated);
        assert!(skill.exists());
        // remove: ours goes, the dir goes when empty (the backup keeps it here)
        let r = remove(Agent::Claude, t.path()).unwrap();
        assert_eq!(r.action, Action::Removed);
        assert!(!skill.exists());
    }

    #[test]
    fn remove_leaves_a_modified_skill_alone() {
        let t = home_with(&[Agent::Claude]);
        install(Agent::Claude, t.path(), exe()).unwrap();
        let skill = skill_path(t.path());
        fs::write(&skill, "edited by the user").unwrap();
        let r = remove(Agent::Claude, t.path()).unwrap();
        assert_eq!(
            r.action,
            Action::Removed,
            "the hook entry itself was removed"
        );
        assert_eq!(fs::read_to_string(&skill).unwrap(), "edited by the user");
        assert!(r
            .notes
            .iter()
            .any(|n| n.contains("left") && n.contains("alone")));
        // removing an empty skill dir
        let t = home_with(&[Agent::Claude]);
        install(Agent::Claude, t.path(), exe()).unwrap();
        remove(Agent::Claude, t.path()).unwrap();
        assert!(!skill_path(t.path()).parent().unwrap().exists());
    }

    #[test]
    fn is_ours_rule_is_precise() {
        let yes = [
            "\"C:/x/screensense.exe\" hook",
            "C:/x/screensense.exe hook",
            "/usr/bin/screensense hook",
            "\"/usr/bin/screensense\" hook",
            "screensense hook",
            "screensense.exe hook",
            "\"C:\\Program Files\\ScreenSense\\SCREENSENSE.EXE\" hook",
            "'/opt/ss dir/screensense' hook",
        ];
        for c in yes {
            assert!(is_ours_command(c), "{c}");
        }
        let no = [
            "desktop-sense hook",
            "echo screensense hook",
            "\"C:/x/screensense.exe\" hookx",
            "\"C:/x/screensense.exe\" hook now",
            "/usr/bin/screensense mcp",
            "/usr/bin/screensense-old hook",
            "/usr/bin/xscreensense hook",
            "python /home/me/screensense/ds.py hook",
            "hook",
            " hook",
            "",
        ];
        for c in no {
            assert!(!is_ours_command(c), "{c}");
        }
        assert!(!is_ours_entry(&json!({"command": 5})));
        assert!(!is_ours_entry(&json!({"type": "command"})));
        assert!(!is_ours_entry(&json!("screensense hook")));
    }

    #[test]
    fn hook_command_quotes_only_when_needed() {
        assert_eq!(
            hook_command(Path::new("/usr/bin/screensense")),
            "/usr/bin/screensense hook"
        );
        assert_eq!(
            hook_command(Path::new(r"C:\Users\me\bin\screensense.exe")),
            "C:/Users/me/bin/screensense.exe hook"
        );
        assert_eq!(
            hook_command(Path::new(r"\\?\C:\Users\me\bin\screensense.exe")),
            "C:/Users/me/bin/screensense.exe hook"
        );
        assert_eq!(
            hook_command(Path::new(r"C:\Program Files\Screen Sense\screensense.exe")),
            "\"C:/Program Files/Screen Sense/screensense.exe\" hook"
        );
        assert_eq!(
            hook_command(Path::new(r"\\?\C:\Program Files (x86)\ss\screensense.exe")),
            "\"C:/Program Files (x86)/ss/screensense.exe\" hook"
        );
        assert_eq!(
            hook_command(Path::new(r"\\?\UNC\srv\share\screensense.exe")),
            "//srv/share/screensense.exe hook"
        );
        assert_eq!(
            hook_command(Path::new("/home/me/my tools/screensense")),
            "\"/home/me/my tools/screensense\" hook"
        );
        for p in [
            "/a b/screensense",
            r"C:\a b\screensense.exe",
            r"\\?\C:\a b\screensense.exe",
            "/x/it's/screensense",
        ] {
            assert!(is_ours_command(&hook_command(Path::new(p))), "{p}");
        }
    }

    #[test]
    fn exe_with_unquotable_characters_is_refused_for_hooks() {
        for bad in [
            "/tmp/a\"b/screensense",
            "/tmp/$HOME/screensense",
            "/tmp/a`b/screensense",
            "C:/100%/screensense.exe",
        ] {
            let t = home_with(&[Agent::Claude]);
            let r = install(Agent::Claude, t.path(), Path::new(bad)).unwrap();
            assert!(matches!(r.action, Action::Skipped(_)), "{bad}");
            assert!(!r.path.exists());
        }
        // OpenCode takes the path as a JSON array element, no shell involved
        let t = home_with(&[Agent::OpenCode]);
        let r = install(
            Agent::OpenCode,
            t.path(),
            Path::new("/tmp/$HOME/screensense"),
        )
        .unwrap();
        assert_eq!(r.action, Action::Installed);
    }

    #[test]
    fn quoted_exe_path_round_trips_through_install() {
        let t = home_with(&[Agent::Claude]);
        let exe = Path::new(r"C:\Program Files\Screen Sense\screensense.exe");
        let r = install(Agent::Claude, t.path(), exe).unwrap();
        let e = &read_json(&r.path)["hooks"]["UserPromptSubmit"][0]["hooks"][0];
        assert_eq!(
            e["command"],
            "\"C:/Program Files/Screen Sense/screensense.exe\" hook"
        );
        assert!(r.notes.iter().any(|n| n.contains("PowerShell")));
        let again = install(Agent::Claude, t.path(), exe).unwrap();
        assert_eq!(again.action, Action::Unchanged);
        let o = home_with(&[Agent::OpenCode]);
        let r = install(Agent::OpenCode, o.path(), exe).unwrap();
        assert_eq!(
            read_json(&r.path)["mcp"]["screensense"]["command"][0],
            "C:/Program Files/Screen Sense/screensense.exe"
        );
    }

    #[test]
    fn manual_snippets_parse_and_match_what_install_writes() {
        for a in HOOK_AGENTS {
            let snip: Value = serde_json::from_str(&manual_snippet(a, exe())).unwrap();
            let t = home_with(&[a]);
            let r = install(a, t.path(), exe()).unwrap();
            let doc = read_json(&r.path);
            assert_eq!(
                snip["hooks"][event_of(a)],
                doc["hooks"][event_of(a)],
                "{a:?}"
            );
        }
        let snip: Value = serde_json::from_str(&manual_snippet(Agent::OpenCode, exe())).unwrap();
        assert_eq!(snip["mcp"]["screensense"]["command"], json!([EXE, "mcp"]));
        assert_eq!(snip["$schema"], "https://opencode.ai/config.json");
    }

    #[test]
    fn symlinked_config_is_written_through() {
        #[cfg(unix)]
        {
            let t = home_with(&[Agent::Claude]);
            let real = t.path().join("dotfiles-settings.json");
            fs::write(&real, "{\"model\":\"m\"}").unwrap();
            let link = config_file(Agent::Claude, t.path());
            std::os::unix::fs::symlink(&real, &link).unwrap();
            install(Agent::Claude, t.path(), exe()).unwrap();
            assert!(fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink());
            assert_eq!(ours_in(&read_json(&real), "UserPromptSubmit"), 1);
        }
    }

    #[cfg(unix)]
    #[test]
    fn file_mode_is_preserved() {
        use std::os::unix::fs::PermissionsExt;
        let t = home_with(&[Agent::Claude]);
        let p = config_file(Agent::Claude, t.path());
        fs::write(&p, "{}").unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o600)).unwrap();
        install(Agent::Claude, t.path(), exe()).unwrap();
        assert_eq!(
            fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    // ---- hook I/O ----

    #[test]
    fn parse_hook_input_reads_the_fields() {
        let h = parse_hook_input(
            r#"{"session_id":"s1","prompt":"fix this","hook_event_name":"UserPromptSubmit","cwd":"/w","extra":1}"#,
        );
        assert_eq!(h.session_id.as_deref(), Some("s1"));
        assert_eq!(h.prompt, "fix this");
        assert_eq!(h.hook_event_name, "UserPromptSubmit");
        assert_eq!(h.cwd.as_deref(), Some("/w"));
    }

    #[test]
    fn parse_hook_input_is_tolerant() {
        let bom = parse_hook_input("\u{feff}{\"prompt\":\"hi\",\"session_id\":\"a\"}\n");
        assert_eq!(bom.prompt, "hi");
        assert_eq!(bom.session_id.as_deref(), Some("a"));
        assert_eq!(parse_hook_input("garbage {{"), HookInput::default());
        assert_eq!(parse_hook_input(""), HookInput::default());
        assert_eq!(parse_hook_input("[1,2]"), HookInput::default());
        assert_eq!(parse_hook_input("null"), HookInput::default());
        let missing = parse_hook_input("{}");
        assert_eq!(missing, HookInput::default());
        let odd = parse_hook_input(
            r#"{"prompt": 42, "session_id": 7, "cwd": null, "hook_event_name": ["x"]}"#,
        );
        assert_eq!(odd.prompt, "");
        assert_eq!(odd.session_id, None);
        assert_eq!(odd.cwd, None);
        assert_eq!(odd.hook_event_name, "");
        let unicode = parse_hook_input(r#"{"prompt":"\u65e5\u672c\u8a9e \ud83d\ude00"}"#);
        assert_eq!(unicode.prompt, "日本語 😀");
    }

    fn sample_context(body_lines: usize) -> String {
        let mut s = String::from(
            "[screensense | live desktop state 14:02:11]\nuntrusted warning\n<<<DATA 3f9a1c2e>>>\n",
        );
        for i in 0..body_lines {
            s.push_str(&format!("line {i} {}\n", "x".repeat(60)));
        }
        s.push_str("<<<END DATA 3f9a1c2e>>>\n(More: screensense now)\n");
        s
    }

    fn additional_context(out: &str) -> String {
        let v: Value = serde_json::from_str(out).unwrap();
        v["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap()
            .to_string()
    }

    #[test]
    fn hook_output_is_single_line_json() {
        let out = hook_output("UserPromptSubmit", "line one\nline \"two\"\t\\ end");
        assert!(!out.contains('\n'));
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["hookSpecificOutput"]["hookEventName"], "UserPromptSubmit");
        assert_eq!(
            v["hookSpecificOutput"]["additionalContext"],
            "line one\nline \"two\"\t\\ end"
        );
        assert_eq!(v.as_object().unwrap().len(), 1);
        assert_eq!(v["hookSpecificOutput"].as_object().unwrap().len(), 2);
    }

    #[test]
    fn hook_output_is_pure_ascii() {
        let ctx = "錯誤 error 日本語 \u{1F600} é \u{7f} \u{2028}";
        let out = hook_output("UserPromptSubmit", ctx);
        assert!(out.is_ascii(), "{out}");
        assert!(out.contains("\\u932f"));
        assert!(
            out.contains("\\ud83d\\ude00"),
            "astral chars as surrogate pairs: {out}"
        );
        assert_eq!(additional_context(&out), ctx);
    }

    #[test]
    fn hook_output_event_name() {
        let v = |e: &str| -> String {
            let o: Value = serde_json::from_str(&hook_output(e, "c")).unwrap();
            o["hookSpecificOutput"]["hookEventName"]
                .as_str()
                .unwrap()
                .to_string()
        };
        assert_eq!(v("BeforeAgent"), "BeforeAgent");
        assert_eq!(v("UserPromptSubmit"), "UserPromptSubmit");
        assert_eq!(v(""), "UserPromptSubmit");
        assert_eq!(v("PreToolUse"), "UserPromptSubmit");
        assert_eq!(v("beforeagent"), "UserPromptSubmit");
    }

    #[test]
    fn short_context_is_not_touched() {
        let ctx = sample_context(5);
        assert_eq!(
            additional_context(&hook_output("UserPromptSubmit", &ctx)),
            ctx
        );
    }

    #[test]
    fn long_context_is_cut_on_a_line_with_the_fence_intact() {
        let ctx = sample_context(600);
        assert!(ctx.len() > 30_000 || units(&ctx) > 9_500);
        let got = additional_context(&hook_output("UserPromptSubmit", &ctx));
        assert!(units(&got) <= 9_500, "{}", units(&got));
        assert!(
            units(&got) > 8_000,
            "keeps most of the budget: {}",
            units(&got)
        );
        let lines: Vec<&str> = got.split('\n').collect();
        assert_eq!(lines[0], "[screensense | live desktop state 14:02:11]");
        assert!(lines[2].starts_with("<<<DATA 3f9a1c2e>>>"));
        let n = lines.len();
        assert_eq!(lines[n - 1], "", "the context's trailing newline survives");
        assert_eq!(lines[n - 4], "(truncated)");
        assert_eq!(lines[n - 3], "<<<END DATA 3f9a1c2e>>>");
        assert_eq!(lines[n - 2], "(More: screensense now)");
        // every kept body line is whole
        for l in &lines[3..n - 4] {
            assert!(
                l.starts_with("line ") && l.ends_with(&"x".repeat(60)),
                "{l}"
            );
        }
    }

    #[test]
    fn thirty_thousand_chars_of_one_kind_stay_under_the_cap() {
        let mut ctx = String::from("head\n<<<DATA ab>>>\n");
        ctx.push_str(&"a\n".repeat(15_000));
        ctx.push_str("<<<END DATA ab>>>");
        assert!(ctx.len() >= 30_000);
        let got = additional_context(&hook_output("BeforeAgent", &ctx));
        assert!(units(&got) <= 9_500);
        assert!(got.ends_with("(truncated)\n<<<END DATA ab>>>"));
        assert!(got.starts_with("head\n<<<DATA ab>>>\n"));
    }

    #[test]
    fn cap_counts_utf16_units_and_keeps_cjk_lines_whole() {
        let mut ctx = String::from("<<<DATA q>>>\n");
        for _ in 0..3000 {
            ctx.push_str("錯誤😀\n");
        }
        ctx.push_str("<<<END DATA q>>>\n");
        let got = cap_context(&ctx);
        assert!(units(&got) <= 9_500, "{}", units(&got));
        assert!(got.contains("(truncated)\n<<<END DATA q>>>"));
        assert!(got
            .lines()
            .filter(|l| l.contains("錯"))
            .all(|l| l == "錯誤😀"));
    }

    #[test]
    fn cut_without_a_fence_still_stays_under_the_cap() {
        let ctx = "plain line\n".repeat(5000);
        let got = cap_context(&ctx);
        assert!(units(&got) <= 9_500);
        assert!(got.ends_with("\n(truncated)"));
        let one_line = "y".repeat(40_000);
        let got = cap_context(&one_line);
        assert!(units(&got) <= 9_500);
        assert!(got.ends_with("(truncated)"));
    }

    #[test]
    fn last_fence_line_wins_when_the_data_mentions_one() {
        let mut ctx = String::from("<<<DATA z>>>\n<<<END DATA fake>>>\n");
        ctx.push_str(&"filler line\n".repeat(2000));
        ctx.push_str("<<<END DATA z>>>\n");
        let got = cap_context(&ctx);
        assert!(
            got.contains("(truncated)\n<<<END DATA z>>>"),
            "{}",
            &got[got.len() - 60..]
        );
    }

    #[test]
    fn skill_file_has_frontmatter_named_screen() {
        let s = skill_markdown();
        assert!(s.starts_with("---\n"));
        let end = s[4..].find("\n---").expect("closing frontmatter fence");
        let fm = &s[4..4 + end];
        assert!(fm.lines().any(|l| l.trim() == "name: screen"), "{fm}");
        assert!(fm
            .lines()
            .any(|l| l.starts_with("description: ") && l.len() > 60));
        assert!(s.lines().count() <= 60, "{} lines", s.lines().count());
        for cmd in [
            "screensense now",
            "screensense recent",
            "screensense errors",
            "screensense search",
            "screensense text",
            "screensense commands",
            "screensense status",
            "screensense pause",
            "screensense resume",
            "screensense stream",
        ] {
            assert!(s.contains(cmd), "{cmd}");
        }
        assert!(s.contains("[screen data]"));
        assert!(s.contains("<<<DATA"));
    }
}
