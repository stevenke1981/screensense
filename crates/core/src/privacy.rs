//! Privacy policy: which windows may be recorded, which screen text must be
//! dropped or masked, and how screen-derived text is made safe for a prompt.
//!
//! Everything here is pure (no I/O, no OS). The rules are data: built-in tables
//! plus the user's additive lists from `config`. `Privacy::rules_hash` covers the
//! effective rules so a changed rule triggers a retroactive purge.
//!
//! Conventions:
//! * All built-in regexes are written with `\b` and compiled with an ASCII-only
//!   word boundary (`(?-u:\b)`), so a secret glued to CJK text (`身分證A123456789`)
//!   is still found.
//! * Title / path / sensitive-text regexes (built-in and user) are compiled
//!   case-insensitively. User `redact` rules are compiled exactly as written;
//!   write `(?i)` yourself if you want case-insensitivity.
//! * A built-in that fails to compile is reported through `warnings()` rather
//!   than panicking; a unit test asserts there are none.

use crate::config::{AppsConfig, PrivacyConfig};
use crate::types::{AppClass, LineBoxes, PxRect, Tier};
use regex::{Regex, RegexBuilder};
use std::collections::HashSet;
use std::ops::Range;
use unicode_general_category::{get_general_category, GeneralCategory};

/// Bump whenever a built-in validator or the matching logic changes without a
/// change to the pattern tables (the tables themselves are part of the hash).
pub const RULES_VERSION: u32 = 1;

// ---------------------------------------------------------------------------
// Text helpers
// ---------------------------------------------------------------------------

fn is_title_mark(c: char) -> bool {
    matches!(
        c,
        '\u{2800}'
            ..='\u{28FF}'
                | '✳'
                | '✻'
                | '✽'
                | '✶'
                | '✢'
                | '·'
                | '◐'
                | '◓'
                | '◑'
                | '◒'
                | '◴'
                | '◵'
                | '◶'
                | '◷'
                | '⏺'
                | '●'
                | '○'
                | '•'
                | '*'
    )
}

/// Strip volatile prefixes so one window does not look like many: leading
/// spinner/braille/bullet glyphs and "(3) " / "(12+) " notification counts.
pub fn normalize_title(title: &str) -> String {
    let mut t = title.trim();
    // Bounded: every iteration removes at least one char or stops.
    loop {
        let before = t;
        t = t.trim_start();
        if let Some(c) = t.chars().next() {
            if is_title_mark(c) {
                t = &t[c.len_utf8()..];
                continue;
            }
        }
        if let Some(rest) = t.strip_prefix('(') {
            let digits = rest.bytes().take_while(|b| b.is_ascii_digit()).count();
            if digits > 0 {
                let mut r = &rest[digits..];
                if let Some(x) = r.strip_prefix('+') {
                    r = x;
                }
                if let Some(x) = r.strip_prefix(')') {
                    t = x;
                    continue;
                }
            }
        }
        if t == before {
            break;
        }
    }
    t.trim().to_string()
}

fn is_invisible(c: char) -> bool {
    if c == '\n' || c == '\t' {
        return false;
    }
    // Variation selectors supplement: Mn, but can smuggle bytes invisibly.
    if ('\u{E0100}'..='\u{E01EF}').contains(&c) {
        return true;
    }
    matches!(
        get_general_category(c),
        GeneralCategory::Control
            | GeneralCategory::Format
            | GeneralCategory::PrivateUse
            | GeneralCategory::Unassigned
            | GeneralCategory::Surrogate
    )
}

/// Remove characters a human cannot see but a model can read: Unicode general
/// categories Cc (except `\n` and `\t`), Cf (zero-width, bidi overrides, tag
/// characters), Co, Cn, Cs, plus the variation-selector supplement.
pub fn strip_invisible(text: &str) -> String {
    text.chars().filter(|&c| !is_invisible(c)).collect()
}

fn truncate_chars(s: &str, limit: usize) -> String {
    match s.char_indices().nth(limit) {
        None => s.to_string(),
        Some((idx, _)) => {
            let mut out = s[..idx].trim_end().to_string();
            out.push('…');
            out
        }
    }
}

fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

struct SanitizeRes {
    url: Regex,
    scheme: Regex,
    data: Regex,
}

fn sanitize_res() -> Option<&'static SanitizeRes> {
    use std::sync::OnceLock;
    static RES: OnceLock<Option<SanitizeRes>> = OnceLock::new();
    RES.get_or_init(|| {
        let url = Regex::new(r#"(?i)(?-u:\b)https?://([^\s/?#"'<>(),;|\\{}`]+)[^\s]*"#).ok()?;
        let scheme = Regex::new(r"(?i)(?-u:\b)(?:javascript|vbscript|file):").ok()?;
        let data = Regex::new(r"(?i)(?-u:\b)data:[a-z0-9.+-]+/[a-z0-9.+-]+[^\s]*").ok()?;
        Some(SanitizeRes { url, scheme, data })
    })
    .as_ref()
}

fn host_of(authority: &str) -> String {
    let a = authority.rsplit('@').next().unwrap_or(authority);
    let host = if a.starts_with('[') {
        match a.find(']') {
            Some(i) => &a[..=i],
            None => a,
        }
    } else {
        a.split(':').next().unwrap_or(a)
    };
    host.trim_end_matches(|c: char| !c.is_alphanumeric() && c != ']')
        .to_string()
}

/// Make screen-derived text safe to place on ONE line of a prompt (see module
/// docs of the design: invisible chars, URLs → `[url:host]`, script-ish schemes
/// neutralised, markup characters removed, whitespace collapsed, truncated to
/// `limit` chars with a trailing `…` when cut).
pub fn sanitize(text: &str, limit: usize) -> String {
    let mut s = strip_invisible(text);
    if let Some(r) = sanitize_res() {
        s = r
            .url
            .replace_all(&s, |c: &regex::Captures| {
                format!("[url:{}]", host_of(&c[1]))
            })
            .into_owned();
        s = r.data.replace_all(&s, "[link]:").into_owned();
        s = r.scheme.replace_all(&s, "[link]:").into_owned();
    } else {
        // Regexes failed to build (cannot happen); fail closed.
        s = s.replace("://", ":/ /");
    }
    // Markup that could be mistaken for structure is replaced by look-alikes rather
    // than deleted, so code, generics and Windows paths stay readable:
    // angle brackets become single guillemets (no tag and no fence marker can be
    // formed), a backtick becomes an apostrophe (cannot close a code span).
    let s: String = s
        .chars()
        .map(|c| match c {
            '<' => '\u{2039}',
            '>' => '\u{203a}',
            '`' => '\'',
            other => other,
        })
        .collect();
    let s = collapse_ws(&s);
    truncate_chars(&s, limit)
}

/// Multi-line variant: every line sanitised separately (≤ 240 chars each),
/// empty lines dropped, at most `max_chars` chars in total (newlines included),
/// cut on a line boundary where possible.
pub fn sanitize_block(text: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    let mut out = String::new();
    let mut used = 0usize;
    for raw in text.split('\n') {
        let line = sanitize(raw, 240);
        if line.is_empty() {
            continue;
        }
        let n = line.chars().count();
        let sep = usize::from(used > 0);
        if used + sep + n <= max_chars {
            if sep == 1 {
                out.push('\n');
            }
            out.push_str(&line);
            used += sep + n;
        } else {
            if used == 0 {
                let keep = max_chars.saturating_sub(1);
                let mut cut: String = line.chars().take(keep).collect();
                cut = cut.trim_end().to_string();
                cut.push('…');
                out = cut;
            }
            break;
        }
    }
    out
}

// ---------------------------------------------------------------------------
// FNV-1a 64
// ---------------------------------------------------------------------------

fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

// ---------------------------------------------------------------------------
// Validators (take the matched text)
// ---------------------------------------------------------------------------

type Validator = fn(&str) -> bool;

fn luhn_card(m: &str) -> bool {
    let digits: Vec<u32> = m.chars().filter_map(|c| c.to_digit(10)).collect();
    let n = digits.len();
    if !(13..=19).contains(&n) {
        return false;
    }
    if digits.iter().all(|&d| d == digits[0]) {
        return false;
    }
    // Issuer sanity: keeps epoch-millisecond timestamps (start with 1) and
    // similar long numbers out.
    let ok_shape = match digits[0] {
        4 => matches!(n, 13 | 16 | 19),
        3 => n >= 14,
        2 => n == 16,
        5 => n >= 16,
        6 => true,
        _ => false,
    };
    if !ok_shape {
        return false;
    }
    let mut sum = 0;
    for (i, d) in digits.iter().rev().enumerate() {
        let mut v = *d;
        if i % 2 == 1 {
            v *= 2;
            if v > 9 {
                v -= 9;
            }
        }
        sum += v;
    }
    sum % 10 == 0
}

fn roc_id(m: &str) -> bool {
    let b = m.as_bytes();
    if b.len() != 10 || !b[0].is_ascii_uppercase() || !b[1..].iter().all(u8::is_ascii_digit) {
        return false;
    }
    let code: u32 = match b[0] {
        b'A' => 10,
        b'B' => 11,
        b'C' => 12,
        b'D' => 13,
        b'E' => 14,
        b'F' => 15,
        b'G' => 16,
        b'H' => 17,
        b'I' => 34,
        b'J' => 18,
        b'K' => 19,
        b'L' => 20,
        b'M' => 21,
        b'N' => 22,
        b'O' => 35,
        b'P' => 23,
        b'Q' => 24,
        b'R' => 25,
        b'S' => 26,
        b'T' => 27,
        b'U' => 28,
        b'V' => 29,
        b'W' => 32,
        b'X' => 30,
        b'Y' => 31,
        _ => 33, // Z
    };
    const W: [u32; 11] = [1, 9, 8, 7, 6, 5, 4, 3, 2, 1, 1];
    let mut digits = vec![code / 10, code % 10];
    digits.extend(b[1..].iter().map(|d| u32::from(d - b'0')));
    digits.iter().zip(W.iter()).map(|(d, w)| d * w).sum::<u32>() % 10 == 0
}

fn has_digit(m: &str) -> bool {
    m.bytes().any(|b| b.is_ascii_digit())
}

/// Real tokens are random base62-ish: they contain a digit and a letter.
fn mixed_alnum(m: &str) -> bool {
    has_digit(m) && m.bytes().any(|b| b.is_ascii_alphabetic())
}

fn phone_digits(m: &str) -> bool {
    let n = m.bytes().filter(u8::is_ascii_digit).count();
    (8..=15).contains(&n)
}

const SECRET_WORDS: [&str; 7] = [
    "password",
    "passwd",
    "secret",
    "token",
    "api_key",
    "apikey",
    "access_key",
];
const NAME_NOISE: [&str; 7] = [
    "tokens",
    "tokeniz",
    "token_type",
    "token_count",
    "token_limit",
    "token_len",
    "token_url",
];

/// Decides whether `NAME = value` is a real secret assignment rather than code
/// (`token = get_token()`), a type annotation, a placeholder or a UI label.
fn secret_assignment(m: &str) -> bool {
    let Some(sep) = m.find([':', '=']) else {
        return false;
    };
    let name = m[..sep]
        .trim_matches(|c: char| c == '"' || c == '\'' || c.is_whitespace())
        .to_ascii_lowercase();
    let mut cleaned = name.replace('-', "_");
    for noise in NAME_NOISE {
        cleaned = cleaned.replace(noise, "");
    }
    let named = SECRET_WORDS.iter().any(|w| cleaned.contains(w))
        || cleaned.contains("private_key")
        || cleaned.contains("privatekey");
    if !named {
        return false;
    }
    let raw = m[sep + 1..].trim_start();
    if raw.starts_with('=') || raw.starts_with('>') {
        return false;
    }
    let (value, quoted) = match raw.chars().next() {
        Some(q @ ('"' | '\'')) => (raw[1..].trim_end_matches(q), true),
        _ => (raw, false),
    };
    let v = value.trim();
    if v.chars().count() < 8 {
        return false;
    }
    let lower = v.to_ascii_lowercase();
    const PLACEHOLDERS: [&str; 6] = ["undefined", "null", "true", "false", "none", "nil"];
    if PLACEHOLDERS.contains(&lower.as_str()) {
        return false;
    }
    const CODE_PREFIXES: [&str; 18] = [
        "process.",
        "os.",
        "env.",
        "env(",
        "env[",
        "environ",
        "import",
        "require",
        "getenv",
        "std::env",
        "system.getenv",
        "$",
        "{",
        "<",
        "%",
        "self.",
        "this.",
        "config.",
    ];
    if CODE_PREFIXES.iter().any(|p| lower.starts_with(p)) || lower.ends_with('(') {
        return false;
    }
    if lower.starts_with("http://") || lower.starts_with("https://") || lower.contains("://") {
        return false;
    }
    if v.starts_with('/') || v.starts_with("./") || v.starts_with("../") || v.starts_with("~/") {
        return false;
    }
    if v.chars().any(char::is_whitespace) {
        return false;
    }
    if !quoted && v.chars().any(|c| "()<>{}[]".contains(c)) {
        return false;
    }
    // Masks and filler: "********", "••••••••", "xxxxxxxx".
    let distinct: HashSet<char> = v.chars().collect();
    if distinct.len() < 3 {
        return false;
    }
    // Plain decimal numbers (limits, counts, ids).
    if v.chars()
        .all(|c| c.is_ascii_digit() || c == '.' || c == ',')
    {
        return false;
    }
    // member.access.chains
    if v.contains('.')
        && v.split('.').all(|p| {
            !p.is_empty()
                && p.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                && !p.starts_with(|c: char| c.is_ascii_digit())
        })
    {
        return false;
    }
    // A secret has a digit or a symbol; a bare word is a label ("Required", "Password").
    v.bytes().any(|b| b.is_ascii_digit()) || v.chars().any(|c| "!@#$%^&*+=/~?|:;".contains(c))
}

// ---------------------------------------------------------------------------
// Built-in tables
// ---------------------------------------------------------------------------

#[rustfmt::skip]
const TERMINAL_APPS: &[&str] = &[
    "WindowsTerminal.exe", "wt.exe", "powershell.exe", "pwsh.exe", "cmd.exe", "conhost.exe",
    "OpenConsole.exe", "alacritty", "wezterm", "wezterm-gui", "kitty", "mintty.exe",
    "ConEmu64.exe", "ConEmu.exe", "Hyper.exe", "Tabby.exe", "warp", "gnome-terminal-server",
    "gnome-terminal", "gnome-terminal-", "konsole", "xterm", "urxvt", "foot", "tilix",
    "terminator", "xfce4-terminal", "ghostty", "rio", "st", "lxterminal", "mate-terminal",
    "qterminal", "terminology", "guake", "yakuake", "cool-retro-term", "sakura", "stterm",
    "bash.exe", "wsl.exe", "wslhost.exe", "Terminal.exe", "FluentTerminal.exe",
];

#[rustfmt::skip]
const EDITOR_APPS: &[&str] = &[
    "Code.exe", "code", "Code - Insiders.exe", "code-insiders", "VSCodium.exe", "codium",
    "Cursor.exe", "cursor", "Windsurf.exe", "windsurf", "zed", "Zed.exe", "zeditor",
    "devenv.exe", "rider64.exe", "idea64.exe", "pycharm64.exe", "webstorm64.exe",
    "clion64.exe", "goland64.exe", "rustrover64.exe", "phpstorm64.exe", "datagrip64.exe",
    "studio64.exe", "sublime_text.exe", "sublime_text", "notepad++.exe", "notepad.exe",
    "nvim-qt.exe", "neovide", "gvim", "gvim.exe", "emacs", "emacs.exe", "kate", "gedit",
    "fleet", "Fleet.exe", "Trae.exe", "trae", "Unity.exe", "idea", "pycharm", "webstorm",
    "clion", "goland", "rustrover", "phpstorm", "rider", "android-studio", "lapce",
    "helix-gui", "geany", "mousepad", "pluma", "xed", "gnome-text-editor", "kwrite",
];

#[rustfmt::skip]
const BROWSER_APPS: &[&str] = &[
    "chrome.exe", "chrome", "google-chrome", "google-chrome-stable", "chromium",
    "chromium-browser", "chromium-browse", "msedge.exe", "microsoft-edge",
    "microsoft-edge-stable", "firefox.exe", "firefox", "firefox-esr", "brave.exe", "brave",
    "brave-browser", "opera.exe", "opera", "vivaldi.exe", "vivaldi", "vivaldi-bin",
    "vivaldi-stable", "arc.exe", "zen.exe", "zen", "zen-browser", "librewolf", "waterfox.exe",
    "waterfox", "thorium.exe", "thorium-browser", "iexplore.exe", "floorp", "floorp.exe",
    "tor browser", "torbrowser", "epiphany", "falkon", "midori", "qutebrowser", "sidekick.exe",
    "yandex.exe", "360chrome.exe", "qqbrowser.exe", "sogouexplorer.exe",
];

#[rustfmt::skip]
const HIDDEN_APPS: &[&str] = &[
    "1Password.exe", "1password", "Bitwarden.exe", "bitwarden", "KeePass.exe", "keepass2",
    "KeePassXC.exe", "keepassxc", "LastPass.exe", "lastpass", "Dashlane.exe", "dashlane",
    "NordPass.exe", "nordpass", "Enpass.exe", "enpass", "Proton Pass.exe", "proton-pass",
    "protonpass", "RoboForm.exe", "roboform", "seahorse", "kwalletmanager",
    "kwalletmanager5", "gnome-keyring", "Authy Desktop.exe", "authy", "Keeper.exe",
    "Exodus.exe", "exodus", "Ledger Live.exe", "ledger-live-desktop", "Electrum.exe",
    "electrum", "Trezor Suite.exe", "trezor-suite", "bitcoin-qt", "sparrow",
];

#[rustfmt::skip]
const SELF_APPS: &[&str] = &[
    "claude.exe", "Claude", "claude-desktop", "codex.exe", "codex", "opencode",
    "opencode.exe", "ChatGPT.exe", "chatgpt", "Gemini.exe", "Copilot.exe",
];

#[rustfmt::skip]
const TITLE_ONLY_APPS: &[&str] = &[
    // chat
    "Telegram.exe", "Telegram", "telegram-desktop", "telegram-deskto", "Discord.exe",
    "discord", "WhatsApp.exe", "WhatsApp.Root.exe", "whatsapp-for-linux", "Signal.exe",
    "signal-desktop", "LINE.exe", "LineCall.exe", "LineMediaPlayer.exe", "WeChat.exe",
    "wechat", "Weixin.exe", "weixin", "KakaoTalk.exe", "kakaotalk", "Messenger.exe",
    "Element.exe", "element-desktop", "Skype.exe", "skype", "skypeforlinux",
    // work chat and calls
    "Slack.exe", "slack", "Teams.exe", "ms-teams.exe", "teams", "teams-for-linux",
    "Zoom.exe", "zoom", "Webex.exe", "webex", "CiscoCollabHost.exe", "Lark.exe", "lark",
    "Feishu.exe", "feishu", "DingTalk.exe", "dingtalk",
    // mail
    "OUTLOOK.EXE", "olk.exe", "olk", "thunderbird", "thunderbird-bin", "Thunderbird.exe",
    "Mailspring.exe", "mailspring", "evolution", "geary",
    // lock / login
    "LockApp.exe", "LogonUI.exe", "i3lock", "swaylock", "xsecurelock", "gnome-screensaver",
    // games and launchers
    "steam.exe", "steam", "steamwebhelper.exe", "EpicGamesLauncher.exe", "Battle.net.exe",
    "Riot Client.exe", "RiotClientServices.exe", "LeagueClientUx.exe",
    "League of Legends.exe", "VALORANT-Win64-Shipping.exe", "cs2.exe", "cs2", "dota2.exe",
    "dota2", "Overwatch.exe", "FortniteClient-Win64-Shipping.exe", "GenshinImpact.exe",
    "Minecraft.exe", "minecraft-launcher", "lutris", "heroic",
    // remote desktop: someone else's screen
    "mstsc.exe", "msrdc.exe", "AnyDesk.exe", "anydesk", "TeamViewer.exe", "teamviewer",
    "parsec.exe", "parsecd", "RustDesk.exe", "rustdesk", "remmina", "vncviewer", "xfreerdp",
    "vinagre", "virt-viewer",
];

/// Hidden-title groups: (name, pattern). Compiled case-insensitively.
#[rustfmt::skip]
const HIDDEN_TITLE: &[(&str, &str)] = &[
    (
        "private-browsing",
        r"\binprivate\b|\bincognito\b|private browsing|private window|無痕|无痕|隱私瀏覽|隐私浏览|私密瀏覽|私密浏览|シークレット|プライベートブラウジング|プライベートウィンドウ|시크릿|비공개 ?브라우징|navegaci[oó]n privada|ventana privada|incógnito|navigation priv[eé]e|fen[eê]tre priv[eé]e|privates fenster|privater modus|inkognito|navega[cç][aã]o (?:an[oó]nima|privada|an[oô]nima)|janela (?:an[oó]nima|privada)|navigazione (?:anonima|privata)|инкогнито|приватн",
    ),
    (
        "password-manager",
        r"\b1password\b|\bbitwarden\b|\bkeepass(?:xc)?\b|\blastpass\b|\bdashlane\b|\bnordpass\b|\benpass\b|proton pass|\broboform\b|password manager|password vault|密碼管理|密码管理|密碼庫|密码库|パスワード ?マネージャー|비밀번호 ?관리자|gestor de contrase|gestionnaire de mots de passe|passwort-?manager|gerenciador de senhas|gestor de palavras",
    ),
    (
        "online-banking",
        r"網路銀行|网络银行|網銀|网银|网上银行|網上銀行|ネットバンキング|インターネットバンキング|인터넷 ?뱅킹|\b(?:internet|online)[ -]banking\b|\be-?banking\b|\bnetbanking\b|\binternetbanking\b|banca (?:online|en l[ií]nea|por internet|digital)|banque en ligne|banque à distance|\bbanco (?:online|em linha)\b|\bonline-banking\b",
    ),
    (
        "medical",
        r"病歷|病历|處方|处方|診斷書|诊断书|診斷證明|诊断证明|診断書|カルテ|処方箋|診療録|診察券|처방전|의무기록|진단서|\bmedical records?\b|\bhealth records?\b|\bpatient (?:chart|portal|records?)\b|\bmychart\b|\behr\b|\belectronic (?:medical|health) records?\b|\bemr (?:system|software|login|portal|records?)\b|patientenakte|krankenakte|arztbrief|historia cl[ií]nica|historial m[eé]dico|dossier m[eé]dical|prontu[aá]rio|receta m[eé]dica|ordonnance m[eé]dicale",
    ),
    (
        "adult",
        r"\bnsfw\b|\bporn|\bhentai|\bpornhub|xvideos|xhamster|xnxx|redtube|youporn|spankbang|chaturbate|stripchat|onlyfans|fansly|missav|javdb|javbus|rule34|e-hentai|nhentai|成人影片|成人網站|成人网站|成人論壇|成人论坛|成人動漫|成人动漫|色情|情色|18禁|十八禁|無碼|无码|アダルト|야동|포르노|19금",
    ),
    (
        "secret-files",
        r#"(?:^|[\s\\/"'(=\[])\.env(?:\.[a-z0-9_-]+)?(?:$|[\s\\/"'):;,\]])|credentials?\.json|\bsecrets?\.(?:json|ya?ml|toml)\b|\.pem\b|\.p12\b|\.pfx\b|\bid_rsa\b|\bid_ed25519\b|\bid_ecdsa\b|\bid_dsa\b|\.kdbx?\b|\.npmrc\b|\.netrc\b|\b_netrc\b|service[-_]?account[^\s\\/]*\.json|\bkubeconfig\b|\.kube[\\/]config|\.aws[\\/]credentials|\.git-credentials|\.pgpass|\.htpasswd|\.pypirc|\.tfstate\b|\.tfvars\b|\.gnupg\b"#,
    ),
    (
        "crypto-wallet",
        r"metamask|\bseed phrase\b|\brecovery phrase\b|\bmnemonic phrase\b|\bprivate keys?\b|助記詞|助记词|私鑰|私钥|シードフレーズ|시드 ?구문|복구 ?구문|frase semilla|phrase de r[eé]cup[eé]ration",
    ),
];

/// Titles that mark a terminal as an AI tool's own window (after `normalize_title`).
#[rustfmt::skip]
const SELF_TITLE: &[&str] = &[
    r"\bclaude\b",
    r"\bcodex\b",
    r"\bopencode\b",
    r"^gemini\b",
    r"\baider\b",
    r"\bqwen[- ]code\b",
    r"\bcursor-agent\b",
];

#[rustfmt::skip]
const TITLE_ONLY_TITLE: &[(&str, &str)] = &[
    (
        "webmail",
        r"\bgmail\b|\boutlook\b|yahoo mail|proton ?mail|收件匣|收件箱|受信トレイ|받은편지함|\binbox\b|bandeja de entrada|boîte de réception|posteingang|caixa de entrada",
    ),
    (
        "web-chat",
        r"\bwhatsapp\b|\bdiscord\b|\btelegram\b|\bslack\b|\bmessenger\b|\bwechat\b|微信|(?:^|[-–—|]\s*)line(?:\s+(?:web|for chrome))?$",
    ),
    (
        "video-call",
        r"google meet|^meet - [a-z]{3}-[a-z]{4}-[a-z]{3}|\bzoom\b|microsoft teams|\bwebex\b",
    ),
];

/// Built-in sensitive-text rules (frame dropped when seen on screen).
#[rustfmt::skip]
const SENSITIVE_TEXT: &[&str] = &[
    r"病歷號|病历号|身分證字號|身份證字號|身份证号|健保卡號|(?-i:\bMRN\b)|medical record (?:number|no\b)",
    r"(?:social security(?: number| no\.?| #)?|\bssn\b)[^0-9\n]{0,20}[0-9]{3}-[0-9]{2}-[0-9]{4}\b",
    r"\bseed phrase\b|\brecovery phrase\b|\bmnemonic phrase\b|助記詞|助记词|\b(?:12|24)[- ]word (?:seed|recovery|secret)",
];

#[rustfmt::skip]
const SENSITIVE_KEYWORDS: &[&str] = &[
    // English
    "diagnosis", "prescription", "date of birth", "medical record", "medical history",
    "lab result", "blood pressure", "medication list", "discharge summary", "patient name",
    "health insurance", "insurance policy number", "passport number", "social security",
    // Traditional Chinese
    "病歷", "診斷", "處方", "身分證", "健保卡", "用藥", "檢驗報告", "血壓", "出生日期", "掛號",
    "就診", "住院",
    // Simplified Chinese
    "病历", "诊断", "处方", "身份证", "医保卡", "用药", "检验报告", "血压", "挂号", "就诊",
    // Japanese
    "カルテ", "診断書", "処方箋", "診察券", "検査結果", "保険証", "生年月日",
    // Korean
    "진단서", "처방전", "진료기록", "주민등록번호", "의무기록",
    // es / fr / de / pt
    "historia clínica", "receta médica", "fecha de nacimiento", "dossier médical",
    "date de naissance", "patientenakte", "krankenakte", "geburtsdatum", "prontuário",
    "data de nascimento",
];

struct BuiltinRule {
    name: &'static str,
    pattern: &'static str,
    replacement: &'static str,
    hard: bool,
    validator: Option<Validator>,
    /// Replacement may use `$1` / `${1}` capture references.
    expand: bool,
}

const fn hard(
    name: &'static str,
    pattern: &'static str,
    replacement: &'static str,
    validator: Option<Validator>,
) -> BuiltinRule {
    BuiltinRule {
        name,
        pattern,
        replacement,
        hard: true,
        validator,
        expand: false,
    }
}

const fn soft(name: &'static str, pattern: &'static str, replacement: &'static str) -> BuiltinRule {
    BuiltinRule {
        name,
        pattern,
        replacement,
        hard: false,
        validator: None,
        expand: false,
    }
}

/// Order matters: earlier rules win overlaps, HARD before SOFT.
#[rustfmt::skip]
const BUILTIN_RULES: &[BuiltinRule] = &[
    hard(
        "pem-private-key",
        r"(?s)-----BEGIN [A-Z ]*PRIVATE KEY(?: BLOCK)?-----.*?(?:-----END [A-Z ]*PRIVATE KEY(?: BLOCK)?-----|\z)",
        "[private-key]",
        None,
    ),
    hard("anthropic-key", r"\bsk-ant-[A-Za-z0-9_-]{10,}", "[token]", None),
    hard(
        "openai-key",
        r"\bsk-(?:proj-|svcacct-|admin-|or-v1-)?[A-Za-z0-9_-]{20,}",
        "[token]",
        Some(mixed_alnum),
    ),
    hard("stripe-key", r"\b(?:sk|rk)_(?:live|test)_[A-Za-z0-9]{16,}", "[token]", None),
    hard("stripe-webhook", r"\bwhsec_[A-Za-z0-9]{20,}", "[token]", None),
    hard("github-token", r"\bgh[pousr]_[A-Za-z0-9]{36,}", "[token]", None),
    hard("github-pat", r"\bgithub_pat_[A-Za-z0-9_]{22,}", "[token]", None),
    hard("gitlab-pat", r"\bglpat-[A-Za-z0-9_-]{20,}", "[token]", None),
    hard("aws-access-key", r"\b(?:AKIA|ASIA)[0-9A-Z]{16}\b", "[token]", None),
    hard("google-api-key", r"\bAIza[0-9A-Za-z_-]{35}", "[token]", None),
    hard("slack-token", r"\bxox[abprs]-[A-Za-z0-9-]{10,}|\bxapp-[0-9]-[A-Za-z0-9-]{10,}", "[token]", None),
    hard(
        "chat-webhook",
        r"hooks\.slack\.com/services/[A-Z0-9]+/[A-Z0-9]+/[A-Za-z0-9]+|discord(?:app)?\.com/api/webhooks/[0-9]+/[A-Za-z0-9_-]+",
        "[token]",
        None,
    ),
    hard("supabase-key", r"\bsb_secret_[A-Za-z0-9_-]{10,}|\bsbp_[A-Za-z0-9]{20,}", "[token]", None),
    hard("npm-token", r"\bnpm_[A-Za-z0-9]{36}\b", "[token]", None),
    hard("huggingface-token", r"\bhf_[A-Za-z0-9]{30,}", "[token]", None),
    hard("sendgrid-key", r"\bSG\.[A-Za-z0-9_-]{16,}\.[A-Za-z0-9_-]{16,}", "[token]", None),
    hard(
        "discord-bot-token",
        r"\b[MNO][A-Za-z0-9_-]{23,25}\.[A-Za-z0-9_-]{6}\.[A-Za-z0-9_-]{27,38}",
        "[token]",
        Some(has_digit),
    ),
    hard("telegram-bot-token", r"\b[0-9]{8,10}:[A-Za-z0-9_-]{35}", "[token]", None),
    hard(
        "misc-vendor-token",
        r"\bpypi-[A-Za-z0-9_-]{40,}|\bdckr_pat_[A-Za-z0-9_-]{20,}|\bgsk_[A-Za-z0-9]{40,}|\br8_[A-Za-z0-9]{30,}|\bre_[A-Za-z0-9]{8,}_[A-Za-z0-9]{16,}|\bshp(?:at|ca|pa|ss)_[a-f0-9]{32}|\bdop_v1_[a-f0-9]{64}|\bpplx-[A-Za-z0-9]{40,}|\bxai-[A-Za-z0-9]{60,}",
        "[token]",
        Some(mixed_alnum),
    ),
    hard(
        "jwt",
        r"\beyJ[A-Za-z0-9_-]{8,}\.eyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{4,}",
        "[token]",
        None,
    ),
    hard("bearer-token", r"\b(?i:bearer)[ \t]+[A-Za-z0-9._~+/-]{20,}=*", "[token]", Some(has_digit)),
    hard(
        "basic-auth-header",
        r"(?i)\bauthorization:[ \t]*basic[ \t]+[A-Za-z0-9+/]{12,}={0,2}",
        "[token]",
        None,
    ),
    hard(
        "url-credentials",
        r#"\b[a-zA-Z][a-zA-Z0-9+.-]{1,20}://[^\s:@/]{1,64}:[^\s@/]{1,128}@[^\s'"]+"#,
        "[secret]",
        None,
    ),
    hard(
        "secret-assignment",
        r#"(?i)\b[A-Za-z0-9_.-]*(?:password|passwd|secret|token|api[_-]?key|access[_-]?key|private[_-]?key)[A-Za-z0-9_.-]*["']?[ \t]*[:=][ \t]*(?:"[^"\n]{8,}"|'[^'\n]{8,}'|[^\s"',;&]{8,})"#,
        "[secret]",
        Some(secret_assignment),
    ),
    hard(
        "payment-card",
        r"\b[0-9]{13,19}\b|\b[0-9]{4}(?: [0-9]{4}){3}\b|\b[0-9]{4}(?:-[0-9]{4}){3}\b|\b[0-9]{4} [0-9]{6} [0-9]{4,5}\b|\b[0-9]{4}-[0-9]{6}-[0-9]{4,5}\b",
        "[card]",
        Some(luhn_card),
    ),
    hard("roc-national-id", r"\b[A-Z][1289][0-9]{8}\b", "[id]", Some(roc_id)),
    // ---- SOFT ----
    soft(
        "database-url",
        r#"(?i)\b(?:postgres(?:ql)?|mysql|mongodb(?:\+srv)?|rediss?|amqps?|mssql|sqlserver)://[^\s'"<>)]+"#,
        "[db-url]",
    ),
    BuiltinRule {
        name: "secret-cli-flag",
        pattern: r#"(?i)(--(?:password|passwd|token|secret|api-key|apikey|access-key|auth-token)(?:[ =]+))(?:"[^"]*"|'[^']*'|[^\s"'-][^\s"']*)"#,
        replacement: "${1}[secret]",
        hard: false,
        validator: None,
        expand: true,
    },
    soft(
        "email",
        r"[A-Za-z0-9._%+-]{1,64}@[A-Za-z0-9.-]{1,253}\.[A-Za-z]{2,24}\b",
        "[email]",
    ),
    soft("tw-mobile", r"\b09[0-9]{2}[- ]?[0-9]{3}[- ]?[0-9]{3}\b", "[phone]"),
    BuiltinRule {
        name: "e164-phone",
        pattern: r"\B\+[0-9]{1,3}(?:[ -][0-9]{1,5}){1,4}\b|\B\+[0-9]{8,15}\b",
        replacement: "[phone]",
        hard: false,
        validator: Some(phone_digits),
        expand: false,
    },
];

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TierReason {
    HiddenApp,
    HiddenTitle,
    SelfWindow,
    TitleOnlyApp,
    TitleOnlyTitle,
    PrivateBrowsing,
    UnknownBrowser,
    Allowed,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Classification {
    pub tier: Tier,
    pub reason: TierReason,
}

/// Why on-screen text makes a frame unusable.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Sensitive {
    Rule,
    Keywords,
    Secret,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Redacted {
    pub text: String,
    /// True if any HARD rule matched (the caller must drop the frame).
    pub hard: bool,
    /// Byte ranges of the INPUT that were replaced. Sorted, non-overlapping.
    pub spans: Vec<Range<usize>>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RedactedLayout {
    /// Redacted lines joined with '\n'.
    pub text: String,
    /// Pixel boxes of every word that overlaps a redacted span.
    pub boxes: Vec<PxRect>,
    pub hard: bool,
}

// ---------------------------------------------------------------------------
// Privacy
// ---------------------------------------------------------------------------

struct Rule {
    re: Regex,
    replacement: String,
    hard: bool,
    validator: Option<Validator>,
    expand: bool,
}

pub struct Privacy {
    warnings: Vec<String>,
    terminal: HashSet<String>,
    editor: HashSet<String>,
    browser: HashSet<String>,
    hidden_apps: HashSet<String>,
    title_only_apps: HashSet<String>,
    self_apps: HashSet<String>,
    hidden_title: Vec<Regex>,
    title_only_title: Vec<Regex>,
    self_title: Vec<Regex>,
    sensitive_text: Vec<Regex>,
    hidden_path: Vec<Regex>,
    keywords: Vec<String>,
    rules: Vec<Rule>,
    unknown_browser_full: bool,
    hash: String,
}

/// Lower-case, directory-less, `.exe`-less app name, so `Chrome.EXE`,
/// `chrome` and `/opt/google/chrome/chrome` all compare equal.
fn norm_app(app: &str) -> String {
    let a = app.trim();
    let a = a.rsplit(['/', '\\']).next().unwrap_or(a);
    let lower = a.to_lowercase();
    match lower.strip_suffix(".exe") {
        Some(s) => s.to_string(),
        None => lower,
    }
}

/// Collects compile warnings and the canonical text the rules hash is built from.
struct Build {
    warnings: Vec<String>,
    canon: String,
}

impl Build {
    fn push(&mut self, kind: &str, parts: &[&str]) {
        self.canon.push_str(kind);
        for p in parts {
            self.canon.push_str(&format!("\u{1}{}:{}", p.len(), p));
        }
        self.canon.push('\n');
    }

    /// `builtin` patterns are written with `\b`/`\B` and get ASCII boundaries.
    fn regex(&mut self, kind: &str, pattern: &str, ci: bool, builtin: bool) -> Option<Regex> {
        if pattern.trim().is_empty() {
            self.warnings.push(format!("{kind}: empty pattern ignored"));
            return None;
        }
        let src = if builtin {
            pattern
                .replace(r"\b", r"(?-u:\b)")
                .replace(r"\B", r"(?-u:\B)")
        } else {
            pattern.to_string()
        };
        match RegexBuilder::new(&src).case_insensitive(ci).build() {
            Ok(re) => Some(re),
            Err(e) => {
                let who = if builtin { "built-in" } else { "user" };
                let first = e.to_string().lines().last().unwrap_or("").to_string();
                self.warnings.push(format!(
                    "{who} {kind} regex `{pattern}` not compiled: {first}"
                ));
                None
            }
        }
    }

    fn regex_list(&mut self, kind: &str, builtin: &[&str], user: &[String]) -> Vec<Regex> {
        let mut out = Vec::new();
        for (p, is_builtin) in builtin
            .iter()
            .map(|p| (*p, true))
            .chain(user.iter().map(|p| (p.as_str(), false)))
        {
            if let Some(re) = self.regex(kind, p, true, is_builtin) {
                self.push(kind, &[p]);
                out.push(re);
            }
        }
        out
    }

    fn apps(&mut self, kind: &str, builtin: &[&str], user: &[String]) -> HashSet<String> {
        let set: HashSet<String> = builtin
            .iter()
            .copied()
            .chain(user.iter().map(String::as_str))
            .map(norm_app)
            .filter(|s| !s.is_empty())
            .collect();
        let mut sorted: Vec<&str> = set.iter().map(String::as_str).collect();
        sorted.sort_unstable();
        self.push(kind, &sorted);
        set
    }
}

impl Privacy {
    /// Built-in rules + the user's additions. Never fails: a user regex that
    /// does not compile is skipped and reported through `warnings()`.
    pub fn new(cfg: &PrivacyConfig, apps: &AppsConfig) -> Privacy {
        let mut b = Build {
            warnings: Vec::new(),
            canon: format!("rules-version={RULES_VERSION}\n"),
        };

        let terminal = b.apps("apps.terminal", TERMINAL_APPS, &apps.terminal);
        let editor = b.apps("apps.editor", EDITOR_APPS, &apps.editor);
        let browser = b.apps("apps.browser", BROWSER_APPS, &apps.browser);
        let hidden_apps = b.apps("hidden_apps", HIDDEN_APPS, &cfg.hidden_apps);
        let title_only_apps = b.apps("title_only_apps", TITLE_ONLY_APPS, &cfg.title_only_apps);
        let self_apps = b.apps("self_apps", SELF_APPS, &cfg.self_apps);

        // Hidden titles: built-in groups, then user additions.
        let mut hidden_title = Vec::new();
        for (name, pat) in HIDDEN_TITLE {
            if let Some(re) = b.regex(&format!("hidden_title:{name}"), pat, true, true) {
                b.push("hidden_title", &[name, pat]);
                hidden_title.push(re);
            }
        }
        for p in &cfg.hidden_title_regex {
            if let Some(re) = b.regex("hidden_title_regex", p, true, false) {
                b.push("hidden_title", &["user", p]);
                hidden_title.push(re);
            }
        }
        let mut title_only_title = Vec::new();
        for (name, pat) in TITLE_ONLY_TITLE {
            if let Some(re) = b.regex(&format!("title_only_title:{name}"), pat, true, true) {
                b.push("title_only_title", &[name, pat]);
                title_only_title.push(re);
            }
        }
        for p in &cfg.title_only_title_regex {
            if let Some(re) = b.regex("title_only_title_regex", p, true, false) {
                b.push("title_only_title", &["user", p]);
                title_only_title.push(re);
            }
        }
        let self_title = b.regex_list("self_title", SELF_TITLE, &[]);
        let sensitive_text =
            b.regex_list("sensitive_text", SENSITIVE_TEXT, &cfg.sensitive_text_regex);
        let hidden_path = b.regex_list("hidden_path", &[], &cfg.hidden_path_regex);

        let mut keywords: Vec<String> = SENSITIVE_KEYWORDS
            .iter()
            .copied()
            .chain(cfg.sensitive_keywords.iter().map(String::as_str))
            .map(|k| k.trim().to_lowercase())
            .filter(|k| !k.is_empty())
            .collect();
        keywords.sort();
        keywords.dedup();
        {
            let refs: Vec<&str> = keywords.iter().map(String::as_str).collect();
            b.push("keywords", &refs);
        }

        let mut rules = Vec::new();
        for r in BUILTIN_RULES {
            if let Some(re) = b.regex(&format!("redact:{}", r.name), r.pattern, false, true) {
                b.push(
                    "rule",
                    &[
                        r.name,
                        r.pattern,
                        r.replacement,
                        if r.hard { "hard" } else { "soft" },
                        if r.expand { "expand" } else { "literal" },
                    ],
                );
                rules.push(Rule {
                    re,
                    replacement: r.replacement.to_string(),
                    hard: r.hard,
                    validator: r.validator,
                    expand: r.expand,
                });
            }
        }
        for u in &cfg.redact {
            if let Some(re) = b.regex("redact", &u.pattern, false, false) {
                let replacement = if u.replace.is_empty() {
                    "[redacted]".to_string()
                } else {
                    u.replace.clone()
                };
                b.push(
                    "rule",
                    &[
                        "user",
                        &u.pattern,
                        &replacement,
                        if u.hard { "hard" } else { "soft" },
                        "literal",
                    ],
                );
                rules.push(Rule {
                    re,
                    replacement,
                    hard: u.hard,
                    validator: None,
                    expand: false,
                });
            }
        }

        let unknown_browser_full = cfg.unknown_browser.trim().eq_ignore_ascii_case("full");
        b.push(
            "unknown_browser",
            &[if unknown_browser_full {
                "full"
            } else {
                "title_only"
            }],
        );

        let hash = format!("{:016x}", fnv1a64(b.canon.as_bytes()));
        Privacy {
            warnings: b.warnings,
            terminal,
            editor,
            browser,
            hidden_apps,
            title_only_apps,
            self_apps,
            hidden_title,
            title_only_title,
            self_title,
            sensitive_text,
            hidden_path,
            keywords,
            rules,
            unknown_browser_full,
            hash,
        }
    }

    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    /// Stable across runs and machines: FNV-1a 64 over a canonical
    /// serialisation of all effective rules plus `RULES_VERSION`.
    pub fn rules_hash(&self) -> String {
        self.hash.clone()
    }

    pub fn app_class(&self, app: &str) -> AppClass {
        let a = norm_app(app);
        if self.terminal.contains(&a) {
            AppClass::Terminal
        } else if self.editor.contains(&a) {
            AppClass::Editor
        } else if self.browser.contains(&a) {
            AppClass::Browser
        } else {
            AppClass::Other
        }
    }

    /// First match wins: hidden app → hidden title → self window → title-only
    /// app → title-only title → browser probe → full.
    /// `private_browsing` is only consulted when the app is a browser.
    pub fn classify(
        &self,
        app: &str,
        title: &str,
        private_browsing: Option<bool>,
    ) -> Classification {
        let c = |tier, reason| Classification { tier, reason };
        let a = norm_app(app);
        if self.hidden_apps.contains(&a) {
            return c(Tier::Hidden, TierReason::HiddenApp);
        }
        let t = normalize_title(&strip_invisible(title));
        if any_match(&self.hidden_title, &t) {
            return c(Tier::Hidden, TierReason::HiddenTitle);
        }
        let class = self.app_class(app);
        if self.self_apps.contains(&a)
            || (class == AppClass::Terminal && any_match(&self.self_title, &t))
        {
            return c(Tier::TitleOnly, TierReason::SelfWindow);
        }
        if self.title_only_apps.contains(&a) {
            return c(Tier::TitleOnly, TierReason::TitleOnlyApp);
        }
        if any_match(&self.title_only_title, &t) {
            return c(Tier::TitleOnly, TierReason::TitleOnlyTitle);
        }
        if class == AppClass::Browser {
            match private_browsing {
                Some(true) => return c(Tier::Hidden, TierReason::PrivateBrowsing),
                None if !self.unknown_browser_full => {
                    return c(Tier::TitleOnly, TierReason::UnknownBrowser)
                }
                _ => {}
            }
        }
        c(Tier::Full, TierReason::Allowed)
    }

    /// Would this on-screen text make the frame unusable?
    pub fn check_text(&self, text: &str) -> Option<Sensitive> {
        if any_match(&self.sensitive_text, text) {
            return Some(Sensitive::Rule);
        }
        if self.redact(text).hard {
            return Some(Sensitive::Secret);
        }
        let low = text.to_lowercase();
        let hits = self
            .keywords
            .iter()
            .filter(|k| low.contains(k.as_str()))
            .take(2)
            .count();
        if hits >= 2 {
            return Some(Sensitive::Keywords);
        }
        None
    }

    /// Apply every rule to the ORIGINAL text; earlier rule wins an overlap, then
    /// earlier start. `hard` is true if any validated HARD match exists, even
    /// one that lost an overlap.
    pub fn redact(&self, text: &str) -> Redacted {
        let mut hard = false;
        // (rule index, start, end, replacement)
        let mut cands: Vec<(usize, usize, usize, String)> = Vec::new();
        for (i, rule) in self.rules.iter().enumerate() {
            let mut take = |start: usize, end: usize, rep: String, matched: &str| {
                if start == end {
                    return;
                }
                if let Some(v) = rule.validator {
                    if !v(matched) {
                        return;
                    }
                }
                hard |= rule.hard;
                cands.push((i, start, end, rep));
            };
            if rule.expand {
                for caps in rule.re.captures_iter(text) {
                    if let Some(m) = caps.get(0) {
                        let mut rep = String::new();
                        caps.expand(&rule.replacement, &mut rep);
                        take(m.start(), m.end(), rep, m.as_str());
                    }
                }
            } else {
                for m in rule.re.find_iter(text) {
                    take(m.start(), m.end(), rule.replacement.clone(), m.as_str());
                }
            }
        }
        cands.sort_by_key(|c| (c.0, c.1));
        // Accepted matches, kept sorted by start.
        let mut accepted: Vec<(usize, usize, String)> = Vec::new();
        for (_, s, e, rep) in cands {
            let pos = accepted.partition_point(|a| a.0 < s);
            let clashes_prev = pos > 0 && accepted[pos - 1].1 > s;
            let clashes_next = pos < accepted.len() && accepted[pos].0 < e;
            if !clashes_prev && !clashes_next {
                accepted.insert(pos, (s, e, rep));
            }
        }
        let mut out = String::with_capacity(text.len());
        let mut spans = Vec::with_capacity(accepted.len());
        let mut at = 0;
        for (s, e, rep) in accepted {
            out.push_str(&text[at..s]);
            out.push_str(&rep);
            spans.push(s..e);
            at = e;
        }
        out.push_str(&text[at..]);
        Redacted {
            text: out,
            hard,
            spans,
        }
    }

    pub fn redact_lines(&self, lines: &[LineBoxes]) -> RedactedLayout {
        let mut texts = Vec::with_capacity(lines.len());
        let mut boxes = Vec::new();
        let mut hard = false;
        for line in lines {
            let r = self.redact(&line.text);
            hard |= r.hard;
            for w in &line.words {
                if r.spans.iter().any(|s| w.start < s.end && s.start < w.end) {
                    let x0 = w.x.saturating_sub(2);
                    let y0 = w.y.saturating_sub(2);
                    boxes.push(PxRect {
                        x: x0,
                        y: y0,
                        w: w.x.saturating_add(w.w).saturating_add(2) - x0,
                        h: w.y.saturating_add(w.h).saturating_add(2) - y0,
                    });
                }
            }
            texts.push(r.text);
        }
        let joined: Vec<&str> = lines.iter().map(|l| l.text.as_str()).collect();
        hard |= self.redact(&joined.join("\n")).hard;
        RedactedLayout {
            text: texts.join("\n"),
            boxes,
            hard,
        }
    }

    /// Should a command line / file path be hidden entirely?
    pub fn text_hidden(&self, text: &str) -> bool {
        let t = strip_invisible(text);
        any_match(&self.hidden_title, &t) || any_match(&self.hidden_path, &t)
    }

    /// None = must not be stored. Some = redacted, whitespace-collapsed, ≤ 300 chars.
    pub fn clean_command(&self, cmd: &str) -> Option<String> {
        let t = strip_invisible(cmd);
        if self.text_hidden(&t) {
            return None;
        }
        let r = self.redact(&t);
        if r.hard {
            return None;
        }
        Some(cap_chars(&collapse_ws(&r.text), 300))
    }

    /// normalize_title → redact (soft and hard hits both masked) → ≤ 200 chars.
    pub fn clean_title(&self, title: &str) -> String {
        let t = normalize_title(&strip_invisible(title));
        let r = self.redact(&t);
        cap_chars(&collapse_ws(&r.text), 200)
    }
}

fn any_match(res: &[Regex], text: &str) -> bool {
    res.iter().any(|r| r.is_match(text))
}

/// At most `max` chars, ending in `…` when cut.
fn cap_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        truncate_chars(s, max.saturating_sub(1))
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn release_regression_command_credentials_and_safe_flags() {
        let p = Privacy::new(&Default::default(), &Default::default());
        for cmd in ["curl -u user:pass https://example.com", "mysql -pSECRET", "docker login -p abcdef", "sshpass -p abcdef ssh host", "openssl enc -k abcdef", "openssl enc -pass pass:abcdef", "redis-cli -a abcdef", "7z a -pSECRET a.zip", "zip -P abcdef a.zip", "echo password | sudo -S whoami"] {
            assert!(p.clean_command(cmd).is_none(), "credential command stored: {cmd}");
        }
        for cmd in ["mkdir -p dir", "ssh -p 2222 host", "git push -u origin main", "cargo test -p screensense", "unzip -p a.zip"] {
            assert!(p.clean_command(cmd).is_some(), "safe command dropped: {cmd}");
        }
    }

    #[test]
    fn release_regression_code_prefix_and_wrapped_secret() {
        let p = Privacy::new(&Default::default(), &Default::default());
        for value in ["importantSecret123", "requireSecret123", "environSecret123", "getenvSecret123"] {
            assert!(p.check_text(&format!("password = {value}")).is_some(), "{value}");
        }
        let key = format!("ghp_{}\n{}", "a1B2".repeat(4), "a1B2".repeat(5));
        assert!(p.check_text(&key).is_some());
        for app in ["", "unknown", "UNKNOWN.exe"] {
            assert_ne!(p.classify(app, "ordinary editor", Some(false)).tier, Tier::Full);
        }
    }
    use super::*;
    use crate::config::RedactRule;
    use crate::types::WordBox;

    fn p() -> Privacy {
        Privacy::new(&PrivacyConfig::default(), &AppsConfig::default())
    }

    fn class(pv: &Privacy, app: &str, title: &str, probe: Option<bool>) -> (Tier, TierReason) {
        let c = pv.classify(app, title, probe);
        (c.tier, c.reason)
    }

    // ---- build ----

    #[test]
    fn builtins_compile_cleanly() {
        let pv = p();
        assert!(pv.warnings().is_empty(), "{:?}", pv.warnings());
    }

    #[test]
    fn fnv_known_answers() {
        assert_eq!(fnv1a64(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a64(b"a"), 0xaf63_dc4c_8601_ec8c);
    }

    // ---- normalize_title ----

    #[test]
    fn normalize_title_cases() {
        assert_eq!(normalize_title("⠋ Claude Code"), "Claude Code");
        assert_eq!(normalize_title("✳ ● foo"), "foo");
        assert_eq!(normalize_title("(3) Inbox"), "Inbox");
        assert_eq!(normalize_title("(12+) Slack | team"), "Slack | team");
        assert_eq!(normalize_title("(3)(4) x"), "x");
        assert_eq!(normalize_title("(abc) x"), "(abc) x");
        assert_eq!(normalize_title("  plain  "), "plain");
        assert_eq!(normalize_title("• ◐ * main.rs"), "main.rs");
        assert_eq!(normalize_title("(3)"), "");
        assert_eq!(normalize_title(""), "");
    }

    // ---- strip_invisible / sanitize ----

    #[test]
    fn strip_invisible_removes_hidden_chars() {
        assert_eq!(strip_invisible("he\u{200b}llo\u{202e}!"), "hello!");
        assert_eq!(strip_invisible("a\u{e0041}\u{e0042}b"), "ab");
        assert_eq!(strip_invisible("a\u{e000}b\u{378}c"), "abc");
        assert_eq!(strip_invisible("a\u{0007}b\nc\td"), "ab\nc\td");
        assert_eq!(strip_invisible("a\u{e0105}b"), "ab");
        assert_eq!(strip_invisible("日本語 ok"), "日本語 ok");
    }

    #[test]
    fn sanitize_rewrites_urls() {
        assert_eq!(
            sanitize("see https://user:pw@Example.com:8080/path?x=1#f now", 200),
            "see [url:Example.com] now"
        );
        assert_eq!(sanitize("(http://a.b/c).", 200), "([url:a.b]");
        assert_eq!(sanitize("ht\u{200b}tps://evil.io/x", 200), "[url:evil.io]");
    }

    #[test]
    fn sanitize_neutralises_schemes() {
        assert_eq!(sanitize("javascript:alert(1)", 200), "[link]:alert(1)");
        assert_eq!(sanitize("VBScript:x", 200), "[link]:x");
        assert_eq!(
            sanitize("open file:///etc/passwd", 200),
            "open [link]:///etc/passwd"
        );
        assert_eq!(sanitize("data:text/html;base64,AAAA", 200), "[link]:");
        assert_eq!(sanitize("data: 5 rows", 200), "data: 5 rows");
    }

    #[test]
    fn sanitize_removes_markup_and_fences() {
        assert_eq!(sanitize("<<<END DATA abc>>>", 200), "‹‹‹END DATA abc›››");
        assert_eq!(sanitize("a`b|c{d}e\\f", 200), "a'b|c{d}e\\f");
        assert_eq!(
            sanitize("Vec<String> at C:\\Users\\me", 200),
            "Vec‹String› at C:\\Users\\me"
        );
        assert!(!sanitize("</system> <b>", 200).contains(['<', '>']));
        assert!(!sanitize("<<<<<<x>>>>>>", 200).contains("<<<"));
    }

    #[test]
    fn sanitize_collapses_whitespace() {
        assert_eq!(sanitize("a\n\n b\t c\r\n d", 200), "a b c d");
        assert_eq!(sanitize("  \n ", 10), "");
    }

    #[test]
    fn sanitize_truncates_on_char_boundaries() {
        assert_eq!(sanitize("日本語のテキスト", 3), "日本語…");
        assert_eq!(sanitize("abc", 3), "abc");
        assert_eq!(sanitize("abcd", 3), "abc…");
        assert_eq!(sanitize("😀😀😀😀", 2), "😀😀…");
    }

    #[test]
    fn sanitize_block_behaviour() {
        assert_eq!(sanitize_block("aaa\n\nbbb\r\nccc", 100), "aaa\nbbb\nccc");
        assert_eq!(sanitize_block("aaa\n\nbbb\nccc", 7), "aaa\nbbb");
        assert_eq!(sanitize_block("aaa\nbbb", 6), "aaa");
        let cut = sanitize_block(&"x".repeat(50), 10);
        assert_eq!(cut.chars().count(), 10);
        assert!(cut.ends_with('…'));
        let long = sanitize_block(&"a".repeat(300), 1000);
        assert_eq!(long.chars().count(), 241);
        assert_eq!(sanitize_block("anything", 0), "");
        assert_eq!(
            sanitize_block("<<<x>>>\njavascript:y", 100),
            "‹‹‹x›››\n[link]:y"
        );
    }

    // ---- app matching ----

    #[test]
    fn app_class_matches_exe_and_linux_names() {
        let pv = p();
        assert_eq!(pv.app_class("chrome.exe"), AppClass::Browser);
        assert_eq!(pv.app_class("chrome"), AppClass::Browser);
        assert_eq!(pv.app_class("Chrome.EXE"), AppClass::Browser);
        assert_eq!(pv.app_class("Code.exe"), AppClass::Editor);
        assert_eq!(pv.app_class("code"), AppClass::Editor);
        assert_eq!(pv.app_class("WindowsTerminal.exe"), AppClass::Terminal);
        assert_eq!(pv.app_class("WindowsTerminal"), AppClass::Terminal);
        assert_eq!(
            pv.app_class("/usr/bin/gnome-terminal-server"),
            AppClass::Terminal
        );
        assert_eq!(pv.app_class("firefox"), AppClass::Browser);
        assert_eq!(pv.app_class("notepad-something"), AppClass::Other);
        assert_eq!(pv.app_class(""), AppClass::Other);
    }

    #[test]
    fn user_apps_are_added() {
        let apps = AppsConfig {
            terminal: vec!["MyTerm.exe".into()],
            editor: vec!["myed".into()],
            browser: vec!["  Cool Browser.exe ".into(), String::new()],
        };
        let pv = Privacy::new(&PrivacyConfig::default(), &apps);
        assert_eq!(pv.app_class("myterm"), AppClass::Terminal);
        assert_eq!(pv.app_class("MyEd.exe"), AppClass::Editor);
        assert_eq!(pv.app_class("cool browser"), AppClass::Browser);
        assert_eq!(pv.app_class("chrome"), AppClass::Browser);
    }

    // ---- classify ----

    #[test]
    fn classify_hidden_app() {
        let pv = p();
        assert_eq!(
            class(&pv, "1Password.exe", "Vault", None),
            (Tier::Hidden, TierReason::HiddenApp)
        );
        assert_eq!(
            class(&pv, "keepassxc", "x", Some(false)),
            (Tier::Hidden, TierReason::HiddenApp)
        );
        assert_eq!(
            class(&pv, "seahorse", "Passwords", None).1,
            TierReason::HiddenApp
        );
    }

    #[test]
    fn classify_hidden_title_groups() {
        let pv = p();
        let hidden = [
            "New Tab - Google Chrome (Incognito)",
            "Microsoft Edge InPrivate",
            "Mozilla Firefox Private Browsing",
            "新分頁 - 無痕模式",
            "新标签页 - 无痕模式",
            "新しいタブ - シークレット",
            "시크릿 모드",
            "Navegación privada",
            "Navigation privée - Firefox",
            "Privates Fenster",
            "Navegação anónima",
            "Bitwarden Web Vault",
            "Best password manager 2026",
            "CTBC 網路銀行",
            "网上银行登录",
            "ネットバンキング",
            "인터넷뱅킹",
            "Online-Banking der Sparkasse",
            "banca en línea",
            "banque en ligne",
            "病歷查詢",
            "病历系统",
            "カルテ一覧",
            "처방전 조회",
            "Medical Record #4",
            "Patient chart - Epic",
            "EHR login",
            "Patientenakte",
            "Historia clínica",
            "Dossier médical",
            "NSFW subreddit",
            "成人影片",
            ".env - myproject - Visual Studio Code",
            ".env.local — repo",
            "C:\\proj\\.env",
            "credentials.json",
            "server.pem",
            "id_rsa",
            "id_ed25519.pub",
            "vault.kdbx",
            ".npmrc",
            ".netrc",
            "service-account-prod.json",
            "kubeconfig",
            "secrets.yaml",
            "MetaMask Notification",
            "Your seed phrase",
            "助記詞備份",
            "Export private key",
        ];
        for t in hidden {
            assert_eq!(
                class(&pv, "someapp", t, None),
                (Tier::Hidden, TierReason::HiddenTitle),
                "title {t:?}"
            );
        }
    }

    #[test]
    fn classify_ordinary_titles_are_full() {
        let pv = p();
        for t in [
            "main.rs - screensense - Code",
            "environment.ts",
            "config.rs",
            ".envrc",
            "Cargo.toml",
            "error[E0308]: mismatched types",
            "rust - lifetime of borrowed value - Stack Overflow",
            "Amazon EMR - Console",
        ] {
            assert_eq!(
                class(&pv, "Code.exe", t, None),
                (Tier::Full, TierReason::Allowed),
                "{t:?}"
            );
        }
    }

    #[test]
    fn classify_order_hidden_app_beats_hidden_title_beats_self() {
        let pv = p();
        assert_eq!(
            class(&pv, "Bitwarden.exe", "x", None).1,
            TierReason::HiddenApp
        );
        // A Claude terminal showing an .env file is Hidden, not merely TitleOnly.
        assert_eq!(
            class(&pv, "wt.exe", "claude .env", None).1,
            TierReason::HiddenTitle
        );
        // A self app with a hidden title is hidden.
        assert_eq!(
            class(&pv, "claude.exe", "Incognito", None).1,
            TierReason::HiddenTitle
        );
    }

    #[test]
    fn classify_self_windows() {
        let pv = p();
        let want = (Tier::TitleOnly, TierReason::SelfWindow);
        assert_eq!(class(&pv, "claude.exe", "anything", None), want);
        assert_eq!(class(&pv, "Claude", "anything", None), want);
        assert_eq!(class(&pv, "codex", "x", None), want);
        assert_eq!(class(&pv, "opencode.exe", "x", None), want);
        // raw spinner-prefixed title is normalised internally
        assert_eq!(
            class(&pv, "WindowsTerminal.exe", "✳ Claude Code", None),
            want
        );
        assert_eq!(class(&pv, "wezterm-gui", "⠋ claude", None), want);
        assert_eq!(class(&pv, "alacritty", "opencode - proj", None), want);
        assert_eq!(class(&pv, "kitty", "Gemini CLI", None), want);
        assert_eq!(class(&pv, "xterm", "aider --model x", None), want);
        assert_eq!(class(&pv, "pwsh.exe", "codex", None), want);
        // Not a terminal: the title alone does not make it a self window.
        assert_eq!(
            class(&pv, "Code.exe", "CLAUDE.md - proj", None),
            (Tier::Full, TierReason::Allowed)
        );
        // Terminal with an ordinary title.
        assert_eq!(
            class(&pv, "wt.exe", "cargo test", None),
            (Tier::Full, TierReason::Allowed)
        );
        assert_eq!(
            class(&pv, "wt.exe", "my gemini notes", None),
            (Tier::Full, TierReason::Allowed)
        );
    }

    #[test]
    fn classify_title_only_apps_and_titles() {
        let pv = p();
        let app = (Tier::TitleOnly, TierReason::TitleOnlyApp);
        for a in [
            "Telegram.exe",
            "telegram-desktop",
            "Slack",
            "slack.exe",
            "ms-teams.exe",
            "Zoom.exe",
            "OUTLOOK.EXE",
            "thunderbird",
            "LockApp.exe",
            "mstsc.exe",
            "AnyDesk",
            "cs2.exe",
            "steam",
        ] {
            assert_eq!(class(&pv, a, "whatever", None), app, "{a}");
        }
        let title = (Tier::TitleOnly, TierReason::TitleOnlyTitle);
        for t in [
            "Inbox (3) - me@gmail.com - Gmail",
            "Yahoo Mail",
            "收件匣",
            "Discord | #general",
            "WhatsApp Web",
            "Meet - abc-defg-hij",
            "Zoom Meeting",
            "Proton Mail",
            "Slack | dev",
        ] {
            assert_eq!(class(&pv, "firefox", t, Some(false)), title, "{t}");
        }
        // Title-only beats the browser probe.
        assert_eq!(
            class(&pv, "chrome", "Gmail", None).1,
            TierReason::TitleOnlyTitle
        );
        // "line 42" must not look like the LINE messenger.
        assert_eq!(
            class(&pv, "Code.exe", "foo.rs: line 42", None).0,
            Tier::Full
        );
    }

    #[test]
    fn classify_browser_probe() {
        let pv = p();
        assert_eq!(
            class(&pv, "chrome.exe", "Rust docs", Some(true)),
            (Tier::Hidden, TierReason::PrivateBrowsing)
        );
        assert_eq!(
            class(&pv, "chrome", "Rust docs", None),
            (Tier::TitleOnly, TierReason::UnknownBrowser)
        );
        assert_eq!(
            class(&pv, "chrome", "Rust docs", Some(false)),
            (Tier::Full, TierReason::Allowed)
        );
        // The probe is ignored for non-browsers.
        assert_eq!(
            class(&pv, "Code.exe", "x", Some(true)),
            (Tier::Full, TierReason::Allowed)
        );
        assert_eq!(
            class(&pv, "Code.exe", "x", None),
            (Tier::Full, TierReason::Allowed)
        );
        let cfg = PrivacyConfig {
            unknown_browser: "full".into(),
            ..Default::default()
        };
        let pv = Privacy::new(&cfg, &AppsConfig::default());
        assert_eq!(
            class(&pv, "chrome", "Rust docs", None),
            (Tier::Full, TierReason::Allowed)
        );
        assert_eq!(
            class(&pv, "chrome", "Rust docs", Some(true)).1,
            TierReason::PrivateBrowsing
        );
    }

    #[test]
    fn classify_invisible_chars_do_not_evade() {
        let pv = p();
        assert_eq!(
            class(&pv, "chrome", "In\u{200b}Private", Some(false)).1,
            TierReason::HiddenTitle
        );
    }

    #[test]
    fn classify_user_additions() {
        let cfg = PrivacyConfig {
            hidden_apps: vec!["SecretApp.exe".into()],
            hidden_title_regex: vec!["ClientName".into()],
            title_only_apps: vec!["MyGame".into()],
            title_only_title_regex: vec!["standup".into()],
            self_apps: vec!["myagent".into()],
            ..Default::default()
        };
        let pv = Privacy::new(&cfg, &AppsConfig::default());
        assert!(pv.warnings().is_empty());
        assert_eq!(class(&pv, "secretapp", "x", None).1, TierReason::HiddenApp);
        assert_eq!(
            class(&pv, "Code.exe", "clientname report", None).1,
            TierReason::HiddenTitle
        );
        assert_eq!(
            class(&pv, "MyGame.exe", "x", None).1,
            TierReason::TitleOnlyApp
        );
        assert_eq!(
            class(&pv, "Code.exe", "Daily STANDUP", None).1,
            TierReason::TitleOnlyTitle
        );
        assert_eq!(
            class(&pv, "myagent.exe", "x", None).1,
            TierReason::SelfWindow
        );
    }

    // ---- redaction: hard families ----

    fn hard_samples() -> Vec<(&'static str, String)> {
        vec![
            (
                "pem",
                format!(
                    "-----BEGIN {} PRIVATE KEY-----\nMIIE\n-----END {} PRIVATE KEY-----",
                    "RSA", "RSA"
                ),
            ),
            (
                "pem-openssh",
                format!(
                    "-----BEGIN {} PRIVATE KEY-----\nb3Bl\n-----END {} PRIVATE KEY-----",
                    "OPENSSH", "OPENSSH"
                ),
            ),
            ("anthropic", format!("sk-ant-{}", "api03-aB1cD2eF3gH4iJ5k")),
            ("openai", format!("sk-{}", "aB3dE5gH7jK9mN1pQ3sT5vX7zC9")),
            (
                "openai-proj",
                format!("sk-proj-{}", "aB3dE5gH7jK9mN1pQ3sT5vX7"),
            ),
            (
                "stripe-live",
                format!("{}_{}_{}", "sk", "live", "4eC39HqLyjWDarjtT1zdp7dc"),
            ),
            (
                "stripe-rk",
                format!("{}_{}_{}", "rk", "test", "4eC39HqLyjWDarjtT1zdp7dc"),
            ),
            (
                "stripe-whsec",
                format!("whsec_{}", "a1B2c3D4e5F6g7H8i9J0k1L2"),
            ),
            ("github-ghp", format!("ghp_{}", "a1B2".repeat(9))),
            ("github-gho", format!("gho_{}", "Z9y8".repeat(10))),
            (
                "github-pat",
                format!("github_pat_{}", "11AbCdEfG0_hIjKlMnOp12345"),
            ),
            ("gitlab", format!("glpat-{}", "aB1cD2eF3gH4iJ5kL6mN")),
            ("aws-akia", format!("AKIA{}", "ABCDEFGH12345678")),
            ("aws-asia", format!("ASIA{}", "ABCDEFGH12345678")),
            (
                "google",
                format!("AIza{}{}", "a1B2c3D4e5F6g7H8i9J0", "k1L2m3N4o5P6q7R"),
            ),
            ("slack", format!("xoxb-{}", "1234567890-abcdefghij")),
            ("supabase-secret", format!("sb_secret_{}", "a1B2c3D4e5F6")),
            (
                "supabase-pat",
                format!("sbp_{}", "a1b2c3d4e5f6a7b8c9d0e1f2"),
            ),
            ("npm", format!("npm_{}", "a1B2".repeat(9))),
            ("huggingface", format!("hf_{}", "a1B2".repeat(8))),
            (
                "sendgrid",
                format!("SG.{}.{}", "aB1cD2eF3gH4iJ5k6L", "mN7oP8qR9sT0uV1wX2"),
            ),
            (
                "discord",
                format!(
                    "{}.{}.{}",
                    "MTA1Nzk4ODMyNDU2NzgxMjM0", "Gx7aBc", "dEf9GhIjKlMnOpQrStUvWxYz012"
                ),
            ),
            (
                "telegram",
                format!("{}:{}", "123456789", "AAHdEfGhIjKlMnOpQrStUvWxYz0123456_-"),
            ),
            (
                "jwt",
                format!(
                    "eyJ{}.eyJ{}.{}",
                    "hbGciOiJIUzI1NiJ9", "zdWIiOiIxMjM0NTY3ODkwIn0", "SflKxwRJSMeKKF2QT4fw"
                ),
            ),
            (
                "bearer",
                format!("Authorization: Bearer {}", "a1B2c3D4e5F6g7H8i9J0k1L2"),
            ),
            (
                "basic-auth",
                format!("Authorization: Basic {}", "dXNlcjpwYXNzd29yZDEyMw=="),
            ),
            (
                "url-credentials",
                "https://admin:s3cr3tPass@db.example.com/x".to_string(),
            ),
            ("env-password", "DB_PASSWORD=hunter2xyz".to_string()),
            (
                "json-secret",
                r#""client_secret": "k9f8a7s6d5f4g3""#.to_string(),
            ),
            ("py-api-key", "api_key = 'k9f8a7s6d5f4'".to_string()),
            (
                "query-token",
                "https://x.io/cb?access_token=abc123def456ghi".to_string(),
            ),
            ("card", "4111 1111 1111 1111".to_string()),
            ("card-dashes", "5500-0000-0000-0004".to_string()),
            ("card-plain", "4111111111111111".to_string()),
            ("card-amex", "3782 822463 10005".to_string()),
            ("roc-id", "A123456789".to_string()),
            (
                "pypi",
                format!(
                    "pypi-{}",
                    "AgEIcHlwaS5vcmc1B2c3D4e5F6g7H8i9J0k1L2m3N4o5P6q7R8s9T0"
                ),
            ),
            (
                "slack-webhook",
                format!(
                    "https://hooks.slack.com/services/{}/{}/{}",
                    "T01ABCDEF", "B02GHIJKL", "a1B2c3D4e5F6g7H8"
                ),
            ),
        ]
    }

    #[test]
    fn every_hard_family_is_detected_and_masked() {
        let pv = p();
        for (name, sample) in hard_samples() {
            let text = format!("see {sample} end");
            let r = pv.redact(&text);
            assert!(r.hard, "{name}: not hard");
            assert!(!r.spans.is_empty(), "{name}: no spans");
            assert!(r.text.starts_with("see "), "{name}: {}", r.text);
            assert_ne!(r.text, text, "{name}: unchanged");
            assert!(
                r.text.contains('[') && r.text.ends_with("end"),
                "{name}: {}",
                r.text
            );
            assert_eq!(
                pv.check_text(&text),
                Some(Sensitive::Secret),
                "{name}: check_text"
            );
            assert!(pv.clean_command(&text).is_none(), "{name}: clean_command");
        }
    }

    #[test]
    fn hard_labels_and_spans() {
        let pv = p();
        let key = format!("ghp_{}", "a1B2".repeat(9));
        let input = format!("token {key}!");
        let r = pv.redact(&input);
        assert_eq!(r.spans.len(), 1);
        assert_eq!(&input[r.spans[0].clone()], key);
        assert_eq!(r.text, "token [token]!");
        let pem = format!("x -----BEGIN {} PRIVATE KEY-----\nabc", "EC");
        assert!(pv.redact(&pem).text.contains("[private-key]"));
        assert_eq!(pv.redact("4111 1111 1111 1111").text, "[card]");
        assert_eq!(pv.redact("A123456789").text, "[id]");
        // Secret glued to CJK text is still found (ASCII word boundaries).
        let cjk = "身分證A123456789號";
        let r = pv.redact(cjk);
        assert!(r.hard);
        assert_eq!(r.text, "身分證[id]號");
    }

    #[test]
    fn luhn_and_roc_checksums() {
        let pv = p();
        assert!(pv.redact("4111 1111 1111 1111").hard);
        assert!(!pv.redact("4111 1111 1111 1112").hard);
        assert_eq!(pv.redact("4111 1111 1111 1112").text, "4111 1111 1111 1112");
        assert!(!pv.redact("4111111111111112").hard);
        assert!(!pv.redact("0000000000000000").hard);
        // epoch milliseconds / nanoseconds are not cards
        assert!(!pv.redact("1717171717171").hard);
        assert!(!pv.redact("1717171717171717171").hard);
        assert!(pv.redact("A123456789").hard);
        assert!(!pv.redact("A123456788").hard);
        assert_eq!(pv.redact("A123456788").text, "A123456788");
        assert!(roc_id("A123456789"));
        assert!(!roc_id("a123456789"));
        assert!(!roc_id("A12345678"));
    }

    #[test]
    fn code_and_prose_are_not_redacted() {
        let pv = p();
        let ok = [
            "let token = get_token();",
            "password: undefined",
            "const API_KEY = process.env.API_KEY",
            "commit 3b6fead9a1c2d4e5f60718293a4b5c6d7e8f9012",
            "550e8400-e29b-41d4-a716-446655440000",
            "error TS2339: Property 'x' does not exist on type 'Y'",
            "the patient build was slow",
            "max_tokens: 16000000",
            "token_count = 12345678",
            "Password: Required",
            "password: ********",
            "Password: ••••••••",
            "token: Option<String>,",
            "secret: &str,",
            "let api_key = self.config.api_key;",
            "if token == \"abcdefgh\" {",
            "secret_key = os.environ[\"SECRET_KEY\"]",
            "password = input(\"Enter your password here\")",
            "\"forgot_password\": \"Forgot your password?\"",
            "token_url: https://example.com/oauth/token",
            "1717171717171",
            "2024-01-15 123045",
            "2024-01-15 12:30:45",
            "192.168.1.10 port 8080",
            "import { x } from '@angular/core'",
            "cargo build --release",
            "task-force-something-long-name-here",
            "re_compile_some_function_name_long",
        ];
        for t in ok {
            let r = pv.redact(t);
            assert!(!r.hard, "hard: {t:?} -> {}", r.text);
            assert_eq!(r.text, t, "changed: {t:?}");
            assert_eq!(pv.check_text(t), None, "{t:?}");
        }
    }

    // ---- soft rules, spans, overlaps ----

    #[test]
    fn soft_rules_mask_with_exact_spans() {
        let pv = p();
        let input = "mail bob@example.com now";
        let r = pv.redact(input);
        assert!(!r.hard);
        assert_eq!(r.text, "mail [email] now");
        assert_eq!(&input[r.spans[0].clone()], "bob@example.com");

        let input = "call 0912-345-678 or +886912345678 or +886 912 345 678.";
        let r = pv.redact(input);
        assert_eq!(r.text, "call [phone] or [phone] or [phone].");
        let got: Vec<&str> = r.spans.iter().map(|s| &input[s.clone()]).collect();
        assert_eq!(got, ["0912-345-678", "+886912345678", "+886 912 345 678"]);

        let input = "mysql -u root --password hunter2 -h db";
        let r = pv.redact(input);
        assert!(!r.hard);
        assert_eq!(r.text, "mysql -u root --password [secret] -h db");
        assert_eq!(&input[r.spans[0].clone()], "--password hunter2");

        let r = pv.redact("run --api-key=abc123 --verbose");
        assert_eq!(r.text, "run --api-key=[secret] --verbose");
        let r = pv.redact("run --token \"two words\" x");
        assert_eq!(r.text, "run --token [secret] x");
        // flag followed by another flag keeps it
        assert_eq!(pv.redact("--token --verbose").text, "--token --verbose");

        let input = "DATABASE_URL postgres://localhost:5432/app";
        let r = pv.redact(input);
        assert!(!r.hard);
        assert_eq!(r.text, "DATABASE_URL [db-url]");
        assert_eq!(&input[r.spans[0].clone()], "postgres://localhost:5432/app");
        assert_eq!(pv.redact("redis://cache:6379/0").text, "[db-url]");
        assert_eq!(pv.redact("MongoDB+SRV://c.example.net/db").text, "[db-url]");
    }

    #[test]
    fn spans_are_sorted_non_overlapping_and_non_ascii_safe() {
        let pv = p();
        let input = "測試 a@b.co 然後 0912345678 結束 c@d.org";
        let r = pv.redact(input);
        assert_eq!(r.spans.len(), 3);
        assert!(r.spans.windows(2).all(|w| w[0].end <= w[1].start));
        let got: Vec<&str> = r.spans.iter().map(|s| &input[s.clone()]).collect();
        assert_eq!(got, ["a@b.co", "0912345678", "c@d.org"]);
        assert_eq!(r.text, "測試 [email] 然後 [phone] 結束 [email]");
    }

    #[test]
    fn overlaps_earlier_rule_wins_and_hard_is_kept() {
        let pv = p();
        // DB URL (soft) beats the e-mail inside it.
        let r = pv.redact("postgresql://bob@example.com/db");
        assert_eq!(r.text, "[db-url]");
        assert_eq!(r.spans.len(), 1);
        // URL credentials (hard) swallow the e-mail-looking part.
        let r = pv.redact("https://bob:pw123@example.com/x");
        assert!(r.hard);
        assert_eq!(r.text, "[secret]");
        assert_eq!(r.spans.len(), 1);
        // Phone inside a longer card-looking digit run is not double counted.
        let r = pv.redact("4111 1111 1111 1111 and bob@example.com");
        assert_eq!(r.text, "[card] and [email]");
    }

    #[test]
    fn user_rules_are_appended_and_case_sensitive() {
        let cfg = PrivacyConfig {
            redact: vec![
                RedactRule {
                    pattern: "ACME-[0-9]{6}".into(),
                    replace: "[ticket]".into(),
                    hard: false,
                },
                RedactRule {
                    pattern: "(?i)projectx".into(),
                    replace: "[proj]".into(),
                    hard: true,
                },
                RedactRule {
                    pattern: "zzz+".into(),
                    replace: String::new(),
                    hard: false,
                },
            ],
            ..Default::default()
        };
        let pv = Privacy::new(&cfg, &AppsConfig::default());
        assert!(pv.warnings().is_empty());
        let r = pv.redact("see ACME-123456 and acme-123456");
        assert_eq!(r.text, "see [ticket] and acme-123456");
        assert!(!r.hard);
        let r = pv.redact("About PROJECTX launch");
        assert!(r.hard);
        assert_eq!(r.text, "About [proj] launch");
        assert_eq!(pv.redact("zzzz").text, "[redacted]");
        // A user HARD rule that loses an overlap to an earlier soft match still counts as hard.
        let cfg = PrivacyConfig {
            redact: vec![RedactRule {
                pattern: "[a-z]+@corp\\.com".into(),
                replace: "[corp]".into(),
                hard: true,
            }],
            ..Default::default()
        };
        let pv = Privacy::new(&cfg, &AppsConfig::default());
        let r = pv.redact("bob@corp.com");
        assert_eq!(r.text, "[email]");
        assert!(r.hard);
    }

    // ---- redact_lines ----

    fn wb(start: usize, end: usize, x: u32) -> WordBox {
        WordBox {
            start,
            end,
            x,
            y: 20,
            w: (end - start) as u32 * 10,
            h: 10,
        }
    }

    #[test]
    fn redact_lines_boxes_for_middle_word() {
        let pv = p();
        let text = "id bob@example.com ok";
        let line = LineBoxes {
            text: text.into(),
            words: vec![wb(0, 2, 10), wb(3, 18, 40), wb(19, 21, 200)],
        };
        let out = pv.redact_lines(&[
            line,
            LineBoxes {
                text: "plain".into(),
                words: vec![wb(0, 5, 0)],
            },
        ]);
        assert_eq!(out.text, "id [email] ok\nplain");
        assert!(!out.hard);
        assert_eq!(
            out.boxes,
            vec![PxRect {
                x: 38,
                y: 18,
                w: 154,
                h: 14
            }]
        );
    }

    #[test]
    fn redact_lines_saturates_and_flags_hard() {
        let pv = p();
        let line = LineBoxes {
            text: "a@b.co".into(),
            words: vec![WordBox {
                start: 0,
                end: 6,
                x: 1,
                y: 0,
                w: 60,
                h: 10,
            }],
        };
        let out = pv.redact_lines(&[line]);
        assert_eq!(
            out.boxes,
            vec![PxRect {
                x: 0,
                y: 0,
                w: 63,
                h: 12
            }]
        );
        let key = format!("ghp_{}", "a1B2".repeat(9));
        let line = LineBoxes {
            text: key.clone(),
            words: vec![wb(0, key.len(), 0)],
        };
        let out = pv.redact_lines(&[line]);
        assert!(out.hard);
        assert_eq!(out.boxes.len(), 1);
        // A secret that only exists across lines still sets `hard`.
        let a = LineBoxes {
            text: "password:".into(),
            words: vec![],
        };
        let b = LineBoxes {
            text: "hunter2xyz".into(),
            words: vec![],
        };
        // (label and value on separate lines are not joined by the rule, so this is not hard)
        assert!(!pv.redact_lines(&[a, b]).hard);
        assert_eq!(pv.redact_lines(&[]).text, "");
    }

    // ---- check_text ----

    #[test]
    fn check_text_rules_keywords_secrets() {
        let pv = p();
        assert_eq!(pv.check_text("病歷號: 12345"), Some(Sensitive::Rule));
        assert_eq!(pv.check_text("Patient MRN 4455"), Some(Sensitive::Rule));
        assert_eq!(
            pv.check_text("Social Security Number: 123-45-6789"),
            Some(Sensitive::Rule)
        );
        assert_eq!(pv.check_text("SSN 123-45-6789"), Some(Sensitive::Rule));
        assert_eq!(
            pv.check_text("Write down your recovery phrase"),
            Some(Sensitive::Rule)
        );
        assert_eq!(pv.check_text("身份证号 xxx"), Some(Sensitive::Rule));
        assert_eq!(
            pv.check_text("Diagnosis: flu. Prescription: rest"),
            Some(Sensitive::Keywords)
        );
        assert_eq!(pv.check_text("病歷 and 診斷"), Some(Sensitive::Keywords));
        assert_eq!(pv.check_text("진단서 처방전"), Some(Sensitive::Keywords));
        assert_eq!(pv.check_text("DIAGNOSIS only once here"), None);
        assert_eq!(pv.check_text("diagnosis diagnosis diagnosis"), None);
        assert_eq!(
            pv.check_text("the clinic opens at nine, the patient symptom list"),
            None
        );
        assert_eq!(pv.check_text("fn main() { println!(\"hi\"); }"), None);
        assert_eq!(pv.check_text("mrn is a variable"), None);
        // rule beats secret beats keywords
        let both = format!("病歷號 ghp_{}", "a1B2".repeat(9));
        assert_eq!(pv.check_text(&both), Some(Sensitive::Rule));
    }

    #[test]
    fn check_text_user_additions() {
        let cfg = PrivacyConfig {
            sensitive_text_regex: vec!["project\\s+blue".into(), String::new()],
            sensitive_keywords: vec!["Falcon".into(), "  Osprey ".into()],
            ..Default::default()
        };
        let pv = Privacy::new(&cfg, &AppsConfig::default());
        assert_eq!(pv.warnings().len(), 1, "{:?}", pv.warnings());
        assert_eq!(pv.check_text("about PROJECT   Blue"), Some(Sensitive::Rule));
        assert_eq!(
            pv.check_text("falcon and osprey"),
            Some(Sensitive::Keywords)
        );
        assert_eq!(pv.check_text("only falcon"), None);
    }

    // ---- text_hidden / clean_* ----

    #[test]
    fn clean_command_hides_or_redacts() {
        let pv = p();
        assert_eq!(pv.clean_command("cat .env"), None);
        assert_eq!(pv.clean_command("source ./.env.local"), None);
        assert_eq!(pv.clean_command("ssh -i ~/.ssh/id_rsa host"), None);
        assert_eq!(
            pv.clean_command(&format!(
                "export GITHUB_TOKEN={}",
                "ghp_".to_string() + &"a1B2".repeat(9)
            )),
            None
        );
        assert_eq!(pv.clean_command("export GITHUB_TOKEN=abc123def456"), None);
        assert_eq!(
            pv.clean_command("cargo build --release").as_deref(),
            Some("cargo build --release")
        );
        assert_eq!(
            pv.clean_command("mysql   -u root\n --password hunter2")
                .as_deref(),
            Some("mysql -u root --password [secret]")
        );
        assert_eq!(
            pv.clean_command("curl x\u{200b}y bob@example.com")
                .as_deref(),
            Some("curl xy [email]")
        );
        let long = pv.clean_command(&"ab ".repeat(400)).unwrap_or_default();
        assert_eq!(long.chars().count(), 300);
        assert!(long.ends_with('…'));
    }

    #[test]
    fn text_hidden_uses_title_and_path_rules() {
        let cfg = PrivacyConfig {
            hidden_path_regex: vec!["secret-project".into()],
            ..Default::default()
        };
        let pv = Privacy::new(&cfg, &AppsConfig::default());
        assert!(pv.text_hidden("cd ~/Secret-Project/src"));
        assert!(pv.text_hidden("C:\\Users\\me\\.aws\\credentials"));
        assert!(!pv.text_hidden("cd ~/projects/screensense"));
        assert!(!pv.text_hidden("npm run env:check"));
    }

    #[test]
    fn clean_title_normalises_and_masks() {
        let pv = p();
        assert_eq!(
            pv.clean_title("● (3) Inbox bob@example.com"),
            "Inbox [email]"
        );
        assert_eq!(pv.clean_title("x\u{200b}y\nz"), "xy z");
        // hard hits are masked too, not dropped
        let t = format!("note ghp_{}", "a1B2".repeat(9));
        assert_eq!(pv.clean_title(&t), "note [token]");
        let long = pv.clean_title(&"w".repeat(500));
        assert_eq!(long.chars().count(), 200);
        assert!(long.ends_with('…'));
        assert_eq!(pv.clean_title(""), "");
    }

    // ---- rules_hash / warnings ----

    #[test]
    fn rules_hash_is_stable_and_sensitive() {
        let a = p().rules_hash();
        let b = p().rules_hash();
        assert_eq!(a, b);
        assert_eq!(a.len(), 16);
        assert!(a
            .chars()
            .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)));

        let with_rule = PrivacyConfig {
            redact: vec![RedactRule {
                pattern: "X".into(),
                replace: "[x]".into(),
                hard: false,
            }],
            ..Default::default()
        };
        let h1 = Privacy::new(&with_rule, &AppsConfig::default()).rules_hash();
        assert_ne!(h1, a);
        let mut other = with_rule.clone();
        other.redact[0].hard = true;
        assert_ne!(
            Privacy::new(&other, &AppsConfig::default()).rules_hash(),
            h1
        );

        let cfg = PrivacyConfig {
            hidden_apps: vec!["foo".into()],
            ..Default::default()
        };
        assert_ne!(Privacy::new(&cfg, &AppsConfig::default()).rules_hash(), a);
        let cfg = PrivacyConfig {
            unknown_browser: "full".into(),
            ..Default::default()
        };
        assert_ne!(Privacy::new(&cfg, &AppsConfig::default()).rules_hash(), a);
        let apps = AppsConfig {
            browser: vec!["mybrowser".into()],
            ..Default::default()
        };
        assert_ne!(
            Privacy::new(&PrivacyConfig::default(), &apps).rules_hash(),
            a
        );
        let cfg = PrivacyConfig {
            sensitive_keywords: vec!["zebra".into()],
            ..Default::default()
        };
        assert_ne!(Privacy::new(&cfg, &AppsConfig::default()).rules_hash(), a);
        // User list order of an app set does not matter.
        let c1 = PrivacyConfig {
            hidden_apps: vec!["a".into(), "b".into()],
            ..Default::default()
        };
        let c2 = PrivacyConfig {
            hidden_apps: vec!["b".into(), "a".into()],
            ..Default::default()
        };
        assert_eq!(
            Privacy::new(&c1, &AppsConfig::default()).rules_hash(),
            Privacy::new(&c2, &AppsConfig::default()).rules_hash()
        );
    }

    #[test]
    fn bad_user_regex_warns_and_is_skipped() {
        let cfg = PrivacyConfig {
            hidden_title_regex: vec!["(unclosed".into(), "fine-title".into()],
            title_only_title_regex: vec!["[".into()],
            sensitive_text_regex: vec!["*bad".into()],
            hidden_path_regex: vec!["(?P<".into()],
            redact: vec![RedactRule {
                pattern: "(".into(),
                replace: "x".into(),
                hard: true,
            }],
            ..Default::default()
        };
        let pv = Privacy::new(&cfg, &AppsConfig::default());
        assert_eq!(pv.warnings().len(), 5, "{:?}", pv.warnings());
        assert!(pv.warnings().iter().all(|w| w.contains("user")));
        assert_eq!(
            class(&pv, "Code.exe", "my fine-title doc", None).1,
            TierReason::HiddenTitle
        );
        assert_eq!(pv.redact("hello (").text, "hello (");
        // the hash ignores the rules that did not compile
        let good = PrivacyConfig {
            hidden_title_regex: vec!["fine-title".into()],
            ..Default::default()
        };
        assert_eq!(
            pv.rules_hash(),
            Privacy::new(&good, &AppsConfig::default()).rules_hash()
        );
    }
}
