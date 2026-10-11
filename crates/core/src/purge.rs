//! Retroactive privacy: when the rules get stricter, what was recorded under the old
//! rules is re-checked and deleted (or hidden). Also the persistent "this window turned
//! out to be sensitive" marks.
//!
//! The digest re-checks everything again when it reads, but only deletion makes the
//! promise "tightening a rule removes matching history from disk" true.

use crate::privacy::{self, Privacy};
use crate::store::Store;
use crate::types::*;

/// Rows are re-checked in pages of this many.
const PAGE: usize = 500;
/// `meta` key holding the rules hash the stored data was last purged against.
const HASH_KEY: &str = "config.rules_hash";
/// Prefix of the persistent sensitive-window marks. "config." keeps them through `Store::wipe`.
const MARK_PREFIX: &str = "config.sensitive.";

#[derive(Clone, Debug, Default, PartialEq)]
pub struct PurgeStats {
    pub spans_hidden: u64,
    pub spans_downgraded: u64,
    pub frames_deleted: u64,
    pub errors_deleted: u64,
    pub commands_deleted: u64,
}

/// Re-check every stored row against the CURRENT rules and delete/hide what would no longer be recorded.
///
/// * spans: a window that is now Hidden loses its title (and its frames and errors); one that is
///   now TitleOnly loses its frames; titles are re-cleaned. A span is never widened.
/// * frames: deleted when the window is no longer Full, when the text trips a content check, or
///   when a (new) soft rule would now mask part of it, because the stored text and image pixels
///   predate the rule.
/// * errors: deleted when their window is no longer Full (screen errors only; shell errors have
///   no window), when any line or the command is hidden or would now be masked.
/// * commands: deleted when the cleaned command differs from the stored one, or the cwd is hidden.
///
/// Running it twice in a row changes nothing the second time.
pub fn purge(store: &Store, privacy: &Privacy) -> crate::Result<PurgeStats> {
    if !privacy.is_usable() {
        return Err(crate::Error::other(
            "privacy rules are not usable; no purge performed",
        ));
    }
    store.invalidate_privacy()?;
    let mut st = PurgeStats::default();

    let mut after = 0i64;
    loop {
        let page = store.spans_page(after, PAGE)?;
        let Some(last) = page.last() else { break };
        after = last.id;
        for s in &page {
            purge_span(store, privacy, s, &mut st)?;
        }
    }

    let mut after = 0i64;
    loop {
        let page = store.frames_page(after, PAGE)?;
        let Some(last) = page.last() else { break };
        after = last.id;
        for f in &page {
            let window_ok = privacy.classify(&f.app, &f.title, Some(false)).tier == Tier::Full;
            if !window_ok
                || privacy.check_text(&f.text).is_some()
                || !privacy.redact(&f.text).spans.is_empty()
            {
                store.frame_delete(f.id)?;
                st.frames_deleted += 1;
            }
        }
    }

    let mut after = 0i64;
    loop {
        let page = store.errors_page(after, PAGE)?;
        let Some(last) = page.last() else { break };
        after = last.id;
        for e in &page {
            if error_must_go(privacy, e) {
                store.error_delete(e.id)?;
                st.errors_deleted += 1;
            }
        }
    }

    let mut after = 0i64;
    loop {
        let page = store.commands_page(after, PAGE)?;
        let Some(last) = page.last() else { break };
        after = last.id;
        for c in &page {
            let clean = privacy.clean_command(&c.cmd);
            if clean.as_deref() != Some(c.cmd.as_str()) || privacy.text_hidden(&c.cwd) {
                store.command_delete(c.id)?;
                st.commands_deleted += 1;
            }
        }
    }
    store.retry_image_deletions()?;
    store.scrub_deleted()?;
    store.meta_set(HASH_KEY, &privacy.rules_hash())?;
    Ok(st)
}

fn purge_span(
    store: &Store,
    privacy: &Privacy,
    s: &Span,
    st: &mut PurgeStats,
) -> crate::Result<()> {
    let live = privacy.classify(&s.app, &s.title, Some(false)).tier;
    match live {
        Tier::Hidden => {
            if s.tier != Tier::Hidden || !s.title.is_empty() {
                store.span_set_privacy(s.id, "", Tier::Hidden)?;
                let (frames, errors) = store.window_record_counts(&s.app, &s.title)?;
                store.forget_window(&s.app, &s.title)?;
                st.frames_deleted += frames;
                st.errors_deleted += errors;
                st.spans_hidden += 1;
            }
        }
        Tier::TitleOnly if s.tier == Tier::Full => {
            let cleaned = privacy.clean_title(&s.title);
            store.span_set_privacy(s.id, &cleaned, Tier::TitleOnly)?;
            let (frames, errors) = store.window_record_counts(&s.app, &s.title)?;
            store.forget_window(&s.app, &s.title)?;
            st.frames_deleted += frames;
            st.errors_deleted += errors;
            if cleaned != s.title {
                store.forget_window(&s.app, &cleaned)?;
            }
            st.spans_downgraded += 1;
        }
        _ => {
            // Not stricter, but the title itself may now be masked by a new rule.
            if s.tier != Tier::Hidden {
                let cleaned = privacy.clean_title(&s.title);
                if cleaned != s.title {
                    store.span_set_privacy(s.id, &cleaned, s.tier)?;
                }
            }
        }
    }
    Ok(())
}

fn error_must_go(privacy: &Privacy, e: &ErrorRec) -> bool {
    // Shell errors come from the shell hook and have no window.
    if e.source == ErrorSource::Ocr
        && privacy.classify(&e.app, &e.title, Some(false)).tier != Tier::Full
    {
        return true;
    }
    if e.lines
        .iter()
        .any(|l| privacy.text_hidden(l) || !privacy.redact(l).spans.is_empty())
    {
        return true;
    }
    if !e.lines.is_empty() && privacy.check_text(&e.lines.join("\n")).is_some() {
        return true;
    }
    if let Some(cmd) = &e.cmd {
        if privacy.clean_command(cmd).as_deref() != Some(cmd.as_str()) {
            return true;
        }
    }
    false
}

/// Run purge() only when privacy.rules_hash() differs from meta "config.rules_hash"; store the new hash
/// AFTER a successful purge (a crash mid-purge must re-run it). First run (no stored hash) also purges.
pub fn purge_if_rules_changed(
    store: &Store,
    privacy: &Privacy,
) -> crate::Result<Option<PurgeStats>> {
    let hash = privacy.rules_hash();
    if store.meta_get(HASH_KEY)?.as_deref() == Some(hash.as_str()) {
        return Ok(None);
    }
    let stats = purge(store, privacy)?;
    store.meta_set(HASH_KEY, &hash)?;
    Ok(Some(stats))
}

// ---------------------------------------------------------------------------
// Persistent sensitive-window marks
// ---------------------------------------------------------------------------

/// FNV-1a 64, the same function `privacy` uses for the rules hash.
fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// `config.sensitive.<16 hex>`: app name and title are normalised the way `classify` does, so
/// `Code.exe` / `code` and `(3) Inbox` / `Inbox` are the same window.
fn mark_key(app: &str, title: &str) -> String {
    let app = app.trim();
    let app = app.rsplit(['/', '\\']).next().unwrap_or(app).to_lowercase();
    let app = app.strip_suffix(".exe").unwrap_or(&app).to_string();
    let title = privacy::normalize_title(&privacy::strip_invisible(title));
    let title = title.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut bytes = app.into_bytes();
    bytes.push(0);
    bytes.extend_from_slice(title.as_bytes());
    format!("{MARK_PREFIX}{:016x}", fnv1a64(&bytes))
}

/// A window whose on-screen content was judged sensitive is remembered so it is treated as Hidden
/// from then on, across restarts. Stored in meta under "config.sensitive.<16-hex FNV-1a of app \0 title>" = ts.
/// (The "config." prefix makes the mark survive `Store::wipe`.) Keys hold a hash, never the title itself.
pub fn mark_sensitive(store: &Store, app: &str, title: &str, now: Ts) -> crate::Result<()> {
    store.invalidate_privacy()?;
    store.meta_set(&mark_key(app, title), &now.to_string())
}

pub fn is_marked_sensitive(store: &Store, app: &str, title: &str) -> crate::Result<bool> {
    Ok(store.meta_get(&mark_key(app, title))?.is_some())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{PrivacyConfig, RedactRule};

    fn privacy_with(f: impl FnOnce(&mut PrivacyConfig)) -> Privacy {
        let mut cfg = PrivacyConfig::default();
        f(&mut cfg);
        Privacy::new(&cfg, &Default::default())
    }

    fn default_privacy() -> Privacy {
        Privacy::new(&Default::default(), &Default::default())
    }

    fn frame(store: &Store, ts: f64, app: &str, title: &str, text: &str) -> i64 {
        store
            .frame_insert(&NewFrame {
                ts,
                app: app.into(),
                title: title.into(),
                text: text.into(),
                image: None,
                reason: "first".into(),
            })
            .unwrap()
    }

    fn ocr_error(store: &Store, ts: f64, app: &str, title: &str, key: &str, line: &str) -> i64 {
        store
            .error_upsert(
                &NewError {
                    ts,
                    key: key.into(),
                    source: ErrorSource::Ocr,
                    app: app.into(),
                    title: title.into(),
                    lines: vec![line.into()],
                    score: 40,
                    frame_id: None,
                    cmd: None,
                    exit_code: None,
                },
                90.0,
                900.0,
            )
            .unwrap()
            .id
    }

    fn shell_error(store: &Store, ts: f64, key: &str, cmd: &str) -> i64 {
        store
            .error_upsert(
                &NewError {
                    ts,
                    key: key.into(),
                    source: ErrorSource::Shell,
                    app: String::new(),
                    title: String::new(),
                    lines: vec!["error[E0308]: mismatched types".into()],
                    score: 60,
                    frame_id: None,
                    cmd: Some(cmd.into()),
                    exit_code: Some(101),
                },
                90.0,
                900.0,
            )
            .unwrap()
            .id
    }

    fn command(store: &Store, ts: f64, cwd: &str, cmd: &str) -> i64 {
        store
            .command_insert(&NewCommand {
                ts,
                shell: "bash".into(),
                cwd: cwd.into(),
                cmd: cmd.into(),
                exit_code: Some(0),
                duration_ms: Some(10),
            })
            .unwrap()
    }

    fn seed(store: &Store) {
        store
            .span_open(100.0, "chrome.exe", "Acme Bank - Accounts", Tier::Full)
            .unwrap();
        store
            .span_open(200.0, "Code.exe", "main.rs - screensense", Tier::Full)
            .unwrap();
        frame(
            store,
            110.0,
            "chrome.exe",
            "Acme Bank - Accounts",
            "balance and transfers overview page",
        );
        frame(
            store,
            210.0,
            "Code.exe",
            "main.rs - screensense",
            "fn main() { println!(\"hello\"); }",
        );
        ocr_error(
            store,
            120.0,
            "chrome.exe",
            "Acme Bank - Accounts",
            "k1",
            "Error: transfer failed",
        );
        ocr_error(
            store,
            220.0,
            "Code.exe",
            "main.rs - screensense",
            "k2",
            "error[E0308]: mismatched types",
        );
        shell_error(store, 230.0, "k3", "cargo test");
        command(store, 240.0, "/home/me/screensense", "cargo test");
        command(
            store,
            250.0,
            "/home/me/screensense",
            "ssh acme-bank-gateway",
        );
    }

    #[test]
    fn default_rules_leave_clean_data_alone() {
        let store = Store::open_in_memory().unwrap();
        seed(&store);
        let st = purge(&store, &default_privacy()).unwrap();
        assert_eq!(st, PurgeStats::default());
        assert_eq!(store.frames_since(0.0, 100).unwrap().len(), 2);
        assert_eq!(store.errors_since(0.0, 100).unwrap().len(), 3);
        assert_eq!(store.commands_since(0.0, 100).unwrap().len(), 2);
    }

    #[test]
    fn hidden_title_rule_hides_span_and_deletes_frames_and_errors() {
        let store = Store::open_in_memory().unwrap();
        seed(&store);
        assert_eq!(store.frames_search("transfers", 0.0, 10).unwrap().len(), 1);
        let pv = privacy_with(|c| c.hidden_title_regex.push("Acme Bank".into()));
        let st = purge(&store, &pv).unwrap();
        assert_eq!(st.spans_hidden, 1);
        assert_eq!(st.spans_downgraded, 0);
        // The bank window's frame and error are gone, from the FTS index too.
        assert_eq!(
            st.frames_deleted, 1,
            "include frames removed by forget_window: {st:?}"
        );
        assert_eq!(st.errors_deleted, 1);
        assert!(store
            .frames_search("transfers", 0.0, 10)
            .unwrap()
            .is_empty());
        assert!(store.frames_search("Acme", 0.0, 10).unwrap().is_empty());
        let frames = store.frames_since(0.0, 100).unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].app, "Code.exe");
        let errs = store.errors_since(0.0, 100).unwrap();
        assert_eq!(errs.len(), 2);
        assert!(errs.iter().all(|e| e.key != "k1"));
        // The span survives with the title discarded.
        let spans = store.spans_between(0.0, 1000.0).unwrap();
        let bank = spans.iter().find(|s| s.app == "chrome.exe").unwrap();
        assert_eq!(bank.tier, Tier::Hidden);
        assert_eq!(bank.title, "");
        // Other window untouched.
        let code = spans.iter().find(|s| s.app == "Code.exe").unwrap();
        assert_eq!(code.tier, Tier::Full);
        assert_eq!(code.title, "main.rs - screensense");
    }

    #[test]
    fn title_only_rule_downgrades_span_and_drops_its_frames() {
        let store = Store::open_in_memory().unwrap();
        seed(&store);
        let pv = privacy_with(|c| c.title_only_title_regex.push("Acme Bank".into()));
        let st = purge(&store, &pv).unwrap();
        assert_eq!(st.spans_downgraded, 1);
        assert_eq!(st.spans_hidden, 0);
        let spans = store.spans_between(0.0, 1000.0).unwrap();
        let bank = spans.iter().find(|s| s.app == "chrome.exe").unwrap();
        assert_eq!(bank.tier, Tier::TitleOnly);
        assert_eq!(bank.title, "Acme Bank - Accounts");
        let frames = store.frames_since(0.0, 100).unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].app, "Code.exe");
    }

    #[test]
    fn hidden_app_rule_deletes_frames_even_without_a_span_match() {
        let store = Store::open_in_memory().unwrap();
        frame(
            &store,
            10.0,
            "weird.exe",
            "no span for this",
            "some stored screen text here",
        );
        let pv = privacy_with(|c| c.hidden_apps.push("weird.exe".into()));
        let st = purge(&store, &pv).unwrap();
        assert_eq!(st.frames_deleted, 1);
        assert!(store.frames_since(0.0, 10).unwrap().is_empty());
    }

    #[test]
    fn new_soft_redact_rule_deletes_matching_frames() {
        let store = Store::open_in_memory().unwrap();
        seed(&store);
        frame(
            &store,
            300.0,
            "Code.exe",
            "main.rs - screensense",
            "deploying for project ZEBRA-7 today",
        );
        let pv = privacy_with(|c| {
            c.redact.push(RedactRule {
                pattern: "ZEBRA-\\d".into(),
                replace: "[codename]".into(),
                hard: false,
            })
        });
        let st = purge(&store, &pv).unwrap();
        assert_eq!(st.frames_deleted, 1);
        assert_eq!(store.frames_since(0.0, 100).unwrap().len(), 2);
        assert!(store.frames_search("ZEBRA", 0.0, 10).unwrap().is_empty());
    }

    #[test]
    fn sensitive_text_in_a_frame_deletes_it() {
        let store = Store::open_in_memory().unwrap();
        frame(
            &store,
            10.0,
            "Code.exe",
            "x",
            "token = AKIAIOSFODNN7EXAMPLE in the config file",
        );
        let st = purge(&store, &default_privacy()).unwrap();
        assert_eq!(st.frames_deleted, 1);
    }

    #[test]
    fn hidden_path_rule_deletes_matching_commands_and_error_cmds() {
        let store = Store::open_in_memory().unwrap();
        seed(&store);
        shell_error(&store, 260.0, "k4", "cat ~/secrets/prod.env");
        let pv = privacy_with(|c| c.hidden_path_regex.push("secrets".into()));
        command(&store, 270.0, "/home/me/secrets", "ls");
        command(&store, 271.0, "/home/me/ok", "cat ~/secrets/prod.env");
        let st = purge(&store, &pv).unwrap();
        assert_eq!(st.commands_deleted, 2);
        assert_eq!(st.errors_deleted, 1);
        let cmds = store.commands_since(0.0, 100).unwrap();
        assert_eq!(cmds.len(), 2);
        assert!(cmds
            .iter()
            .all(|c| !c.cmd.contains("secrets") && !c.cwd.contains("secrets")));
    }

    #[test]
    fn command_that_would_now_be_masked_is_deleted() {
        let store = Store::open_in_memory().unwrap();
        command(&store, 1.0, "/x", "deploy --client acme-corp");
        command(&store, 2.0, "/x", "deploy --client other");
        let pv = privacy_with(|c| {
            c.redact.push(RedactRule {
                pattern: "acme-corp".into(),
                replace: "[client]".into(),
                hard: false,
            })
        });
        let st = purge(&store, &pv).unwrap();
        assert_eq!(st.commands_deleted, 1);
        assert_eq!(
            store.commands_since(0.0, 10).unwrap()[0].cmd,
            "deploy --client other"
        );
    }

    #[test]
    fn shell_errors_without_an_app_survive_window_rules() {
        let store = Store::open_in_memory().unwrap();
        let id = shell_error(&store, 5.0, "ks", "cargo build");
        // A rule that hides every window with an empty-ish name must not touch shell errors.
        let pv = privacy_with(|c| c.hidden_title_regex.push("^$".into()));
        let st = purge(&store, &pv).unwrap();
        assert_eq!(st.errors_deleted, 0);
        assert!(store.error_get(id).unwrap().is_some());
    }

    #[test]
    fn error_lines_that_are_now_masked_or_hidden_are_deleted() {
        let store = Store::open_in_memory().unwrap();
        ocr_error(
            &store,
            1.0,
            "Code.exe",
            "main.rs",
            "e1",
            "Error: bad client acme-corp config",
        );
        ocr_error(
            &store,
            2.0,
            "Code.exe",
            "main.rs",
            "e2",
            "Error: something else",
        );
        let pv = privacy_with(|c| {
            c.redact.push(RedactRule {
                pattern: "acme-corp".into(),
                replace: "[client]".into(),
                hard: false,
            })
        });
        let st = purge(&store, &pv).unwrap();
        assert_eq!(st.errors_deleted, 1);
        assert_eq!(store.errors_since(0.0, 10).unwrap()[0].key, "e2");
    }

    #[test]
    fn already_masked_stored_text_is_stable() {
        // Text stored after redaction contains the replacement markers; purging must not
        // treat those as new matches.
        let store = Store::open_in_memory().unwrap();
        frame(
            &store,
            1.0,
            "Code.exe",
            "x",
            "mail [email] phone [phone] key [redacted] fine",
        );
        command(
            &store,
            2.0,
            "/x",
            "curl -H token=[redacted] http://localhost",
        );
        let st = purge(&store, &default_privacy()).unwrap();
        assert_eq!(st, PurgeStats::default(), "{st:?}");
    }

    #[test]
    fn purge_is_idempotent() {
        let store = Store::open_in_memory().unwrap();
        seed(&store);
        let pv = privacy_with(|c| {
            c.hidden_title_regex.push("Acme Bank".into());
            c.hidden_path_regex.push("acme-bank".into());
        });
        let first = purge(&store, &pv).unwrap();
        assert!(
            first.spans_hidden >= 1 && first.commands_deleted >= 1,
            "{first:?}"
        );
        assert_eq!(purge(&store, &pv).unwrap(), PurgeStats::default());
    }

    #[test]
    fn titles_are_rewritten_when_cleaning_changes_them() {
        let store = Store::open_in_memory().unwrap();
        store
            .span_open(
                1.0,
                "Code.exe",
                "⠋ notes (3) for bob@example.com",
                Tier::Full,
            )
            .unwrap();
        purge(&store, &default_privacy()).unwrap();
        let s = &store.spans_between(0.0, 10.0).unwrap()[0];
        assert!(!s.title.contains("bob@example.com"), "{:?}", s.title);
        assert_eq!(s.tier, Tier::Full);
    }

    #[test]
    fn spans_are_never_widened() {
        let store = Store::open_in_memory().unwrap();
        store.span_open(1.0, "Code.exe", "", Tier::Hidden).unwrap();
        store
            .span_open(2.0, "Code.exe", "t", Tier::TitleOnly)
            .unwrap();
        let st = purge(&store, &default_privacy()).unwrap();
        assert_eq!(st, PurgeStats::default());
        let spans = store.spans_between(0.0, 10.0).unwrap();
        assert_eq!(spans[0].tier, Tier::Hidden);
        assert_eq!(spans[1].tier, Tier::TitleOnly);
    }

    #[test]
    fn pages_through_more_than_one_page() {
        let store = Store::open_in_memory().unwrap();
        for i in 0..(PAGE as i64 + 40) {
            command(
                &store,
                i as f64,
                "/x",
                if i % 2 == 0 {
                    "keep me"
                } else {
                    "drop secrets"
                },
            );
        }
        let pv = privacy_with(|c| c.hidden_path_regex.push("secrets".into()));
        let st = purge(&store, &pv).unwrap();
        assert_eq!(st.commands_deleted, (PAGE as u64 + 40) / 2);
        assert_eq!(
            store.commands_since(0.0, 10_000).unwrap().len() as u64,
            (PAGE as u64 + 40) - st.commands_deleted
        );
    }

    #[test]
    fn purge_if_rules_changed_runs_once_per_rule_set() {
        let store = Store::open_in_memory().unwrap();
        seed(&store);
        let pv = default_privacy();
        assert!(store.meta_get(HASH_KEY).unwrap().is_none());
        // First run (no stored hash) purges.
        assert!(purge_if_rules_changed(&store, &pv).unwrap().is_some());
        assert_eq!(store.meta_get(HASH_KEY).unwrap().unwrap(), pv.rules_hash());
        // Same rules: nothing to do.
        assert!(purge_if_rules_changed(&store, &pv).unwrap().is_none());
        // A rule change purges again, and acts.
        let pv2 = privacy_with(|c| c.hidden_title_regex.push("Acme Bank".into()));
        assert_ne!(pv.rules_hash(), pv2.rules_hash());
        let st = purge_if_rules_changed(&store, &pv2).unwrap().unwrap();
        assert_eq!(st.spans_hidden, 1);
        assert!(purge_if_rules_changed(&store, &pv2).unwrap().is_none());
    }

    #[test]
    fn mark_sensitive_round_trip() {
        let store = Store::open_in_memory().unwrap();
        assert!(!is_marked_sensitive(&store, "chrome.exe", "Statement").unwrap());
        mark_sensitive(&store, "chrome.exe", "Statement", 123.0).unwrap();
        assert!(is_marked_sensitive(&store, "chrome.exe", "Statement").unwrap());
        // Different title / app: not marked.
        assert!(!is_marked_sensitive(&store, "chrome.exe", "Statement 2").unwrap());
        assert!(!is_marked_sensitive(&store, "firefox.exe", "Statement").unwrap());
        // App spelling and volatile title prefixes do not matter.
        assert!(is_marked_sensitive(&store, "Chrome.EXE", "(2) Statement").unwrap());
        assert!(
            is_marked_sensitive(&store, "C:\\Program Files\\chrome.exe", "⠋ Statement").unwrap()
        );
    }

    #[test]
    fn mark_survives_wipe_and_never_stores_the_title() {
        let store = Store::open_in_memory().unwrap();
        let title = "Quarterly Salary Review.xlsx";
        mark_sensitive(&store, "excel.exe", title, 5.0).unwrap();
        store.meta_set("heartbeat", "{}").unwrap();
        store.wipe().unwrap();
        assert!(is_marked_sensitive(&store, "excel.exe", title).unwrap());
        assert!(
            store.meta_get("heartbeat").unwrap().is_none(),
            "non-config meta is wiped"
        );
        // The key is a fixed-width hash and the value a timestamp: no title anywhere.
        let key = mark_key("excel.exe", title);
        assert!(key.starts_with("config.sensitive."));
        assert_eq!(key.len(), "config.sensitive.".len() + 16);
        assert!(key["config.sensitive.".len()..]
            .chars()
            .all(|c| c.is_ascii_hexdigit()));
        assert!(!key.contains("Salary") && !key.to_lowercase().contains("excel"));
        assert_eq!(store.meta_get(&key).unwrap().unwrap(), "5");
    }

    #[test]
    fn mark_key_is_stable() {
        // FNV-1a over "code\0main.rs": pinned so the format cannot drift between releases.
        let mut bytes = b"code".to_vec();
        bytes.push(0);
        bytes.extend_from_slice(b"main.rs");
        assert_eq!(
            mark_key("Code.exe", "main.rs"),
            format!("config.sensitive.{:016x}", fnv1a64(&bytes))
        );
        assert_eq!(fnv1a64(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a64(b"a"), 0xaf63_dc4c_8601_ec8c);
    }
}
