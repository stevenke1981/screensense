//! SQLite storage: the only state shared between the daemon, the prompt hook,
//! the MCP server and the shell hook.
//!
//! * One file, `<data>/screensense.db`, in WAL mode with `busy_timeout` 3 s.
//! * Schema versions live in `PRAGMA user_version`; [`MIGRATIONS`] is an ordered
//!   list, so a later version is just another entry.
//! * Every multi-step update is one transaction. Read-then-write operations
//!   use `BEGIN IMMEDIATE` so two processes cannot interleave.
//! * Image files are deleted *after* the transaction that drops their row has
//!   committed, and never outside the data directory.

use crate::config::RetentionConfig;
use crate::types::*;
use rusqlite::types::Value;
use rusqlite::{
    params, params_from_iter, Connection, OptionalExtension, Row, Transaction, TransactionBehavior,
};
use std::collections::HashSet;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use crate::Error;

const BUSY_TIMEOUT: Duration = Duration::from_millis(3000);
/// How long `open` keeps retrying its one-time setup when the file is contended.
const SETUP_RETRY: Duration = Duration::from_secs(5);
/// Orphan image files younger than this are left alone (a writer may be between
/// "file written" and "row inserted").
const ORPHAN_MIN_AGE_S: f64 = 600.0;
/// Hook sessions idle for longer than this are pruned by `sweep`.
const SESSION_MAX_IDLE_S: f64 = 2.0 * 86_400.0;
/// Characters of context kept on each side of a search match.
const SNIPPET_SIDE: usize = 80;

/// Ordered schema steps. `MIGRATIONS[n]` upgrades `user_version` n → n + 1.
const MIGRATIONS: &[&str] = &[
    // ---- version 1 ----
    r#"
CREATE TABLE meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
) WITHOUT ROWID;

CREATE TABLE spans (
    id    INTEGER PRIMARY KEY AUTOINCREMENT,
    start REAL NOT NULL,
    "end" REAL NOT NULL,
    app   TEXT NOT NULL,
    title TEXT NOT NULL,
    tier  TEXT NOT NULL
);
CREATE INDEX spans_start ON spans(start);
CREATE INDEX spans_end   ON spans("end");

CREATE TABLE frames (
    id     INTEGER PRIMARY KEY AUTOINCREMENT,
    ts     REAL NOT NULL,
    app    TEXT NOT NULL,
    title  TEXT NOT NULL,
    text   TEXT NOT NULL,
    image  TEXT,
    reason TEXT NOT NULL
);
CREATE INDEX frames_ts ON frames(ts);

CREATE VIRTUAL TABLE frames_fts USING fts5(
    text, title,
    content = 'frames', content_rowid = 'id',
    tokenize = 'trigram'
);
CREATE TRIGGER frames_ai AFTER INSERT ON frames BEGIN
    INSERT INTO frames_fts(rowid, text, title) VALUES (new.id, new.text, new.title);
END;
CREATE TRIGGER frames_ad AFTER DELETE ON frames BEGIN
    INSERT INTO frames_fts(frames_fts, rowid, text, title)
    VALUES ('delete', old.id, old.text, old.title);
END;
CREATE TRIGGER frames_au AFTER UPDATE OF text, title ON frames BEGIN
    INSERT INTO frames_fts(frames_fts, rowid, text, title)
    VALUES ('delete', old.id, old.text, old.title);
    INSERT INTO frames_fts(rowid, text, title) VALUES (new.id, new.text, new.title);
END;

CREATE TABLE errors (
    id        INTEGER PRIMARY KEY AUTOINCREMENT,
    ts        REAL NOT NULL,
    last_ts   REAL NOT NULL,
    bump_ts   REAL NOT NULL,
    key       TEXT NOT NULL,
    source    TEXT NOT NULL,
    app       TEXT NOT NULL,
    title     TEXT NOT NULL,
    lines     TEXT NOT NULL DEFAULT '[]',
    count     INTEGER NOT NULL DEFAULT 1,
    score     INTEGER NOT NULL DEFAULT 0,
    frame_id  INTEGER,
    cmd       TEXT,
    exit_code INTEGER
);
CREATE INDEX errors_last_ts     ON errors(last_ts);
CREATE INDEX errors_key_last_ts ON errors(key, last_ts);

CREATE TABLE commands (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    ts          REAL NOT NULL,
    shell       TEXT NOT NULL,
    cwd         TEXT NOT NULL,
    cmd         TEXT NOT NULL,
    exit_code   INTEGER,
    duration_ms INTEGER
);
CREATE INDEX commands_ts ON commands(ts);

CREATE TABLE insights (
    id        INTEGER PRIMARY KEY AUTOINCREMENT,
    ts        REAL NOT NULL,
    "trigger" TEXT NOT NULL,
    model     TEXT NOT NULL,
    json      TEXT NOT NULL
);
CREATE INDEX insights_ts ON insights(ts);

CREATE TABLE sessions (
    id      TEXT PRIMARY KEY,
    last_ts REAL NOT NULL,
    full_ts REAL NOT NULL
) WITHOUT ROWID;
"#,
];

/// A frame found by [`Store::frames_search`].
#[derive(Clone, Debug, PartialEq)]
pub struct FrameHit {
    pub frame: Frame,
    /// ≤ ~160 chars around the first match, single line.
    pub snippet: String,
}

/// What a retention sweep removed.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct SweepStats {
    /// Image references dropped (age, size cap, deleted frames) plus orphan files removed.
    pub images_deleted: u64,
    /// Bytes of image files actually removed from disk.
    pub image_bytes_freed: u64,
    pub frames_deleted: u64,
    pub spans_deleted: u64,
    pub commands_deleted: u64,
    pub errors_deleted: u64,
    pub insights_deleted: u64,
}

/// Outcome of [`Store::error_upsert`].
#[derive(Clone, Debug, PartialEq)]
pub struct ErrorUpsert {
    pub id: i64,
    /// A row was created (first sighting).
    pub is_new: bool,
    /// Count went up (new or a re-appearance).
    pub bumped: bool,
    pub count: u32,
}

/// Handle on the database. Cheap to open; one per process (or thread).
pub struct Store {
    conn: Connection,
    data_dir: Option<PathBuf>,
}

fn lim(n: usize) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

/// Run `f`, retrying for a few seconds while SQLite reports the database busy.
fn retry_busy<T>(mut f: impl FnMut() -> crate::Result<T>) -> crate::Result<T> {
    let deadline = std::time::Instant::now() + SETUP_RETRY;
    let mut delay = Duration::from_millis(2);
    loop {
        match f() {
            Err(Error::Db(rusqlite::Error::SqliteFailure(e, _)))
                if matches!(
                    e.code,
                    rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
                ) && std::time::Instant::now() < deadline =>
            {
                std::thread::sleep(delay);
                delay = (delay * 2).min(Duration::from_millis(50));
            }
            other => return other,
        }
    }
}

fn user_version(conn: &Connection) -> crate::Result<i64> {
    Ok(conn.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))?)
}

fn newer_error(found: i64, known: i64) -> Error {
    Error::Other(format!(
        "database was created by a newer screensense (schema version {found}, \
         this build understands up to {known}); upgrade screensense"
    ))
}

fn migrate(conn: &Connection) -> crate::Result<()> {
    let known = lim(MIGRATIONS.len());
    let current = user_version(conn)?;
    if current > known {
        return Err(newer_error(current, known));
    }
    if current == known {
        return Ok(());
    }
    // Take the write lock first, then re-read: another process may have migrated meanwhile.
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    let current = user_version(&tx)?;
    if current > known {
        return Err(newer_error(current, known));
    }
    let done = usize::try_from(current.max(0)).unwrap_or(0);
    for (i, sql) in MIGRATIONS.iter().enumerate().skip(done) {
        tx.execute_batch(sql)?;
        tx.execute_batch(&format!("PRAGMA user_version = {}", i + 1))?;
    }
    tx.commit()?;
    Ok(())
}

// ---- row mapping ---------------------------------------------------------

const SPAN_COLS: &str = r#"id, start, "end", app, title, tier"#;
const FRAME_COLS: &str = "id, ts, app, title, text, image, reason";
const FRAME_COLS_F: &str = "f.id, f.ts, f.app, f.title, f.text, f.image, f.reason";
const ERROR_COLS: &str =
    "id, ts, last_ts, key, source, app, title, lines, count, score, frame_id, cmd, exit_code, bump_ts";
const COMMAND_COLS: &str = "id, ts, shell, cwd, cmd, exit_code, duration_ms";

fn span_row(r: &Row) -> rusqlite::Result<Span> {
    Ok(Span {
        id: r.get(0)?,
        start: r.get(1)?,
        end: r.get(2)?,
        app: r.get(3)?,
        title: r.get(4)?,
        tier: Tier::parse(&r.get::<_, String>(5)?),
    })
}

fn frame_row(r: &Row) -> rusqlite::Result<Frame> {
    Ok(Frame {
        id: r.get(0)?,
        ts: r.get(1)?,
        app: r.get(2)?,
        title: r.get(3)?,
        text: r.get(4)?,
        image: r.get(5)?,
        reason: r.get(6)?,
    })
}

fn error_row(r: &Row) -> rusqlite::Result<ErrorRec> {
    let lines: String = r.get(7)?;
    let count: i64 = r.get(8)?;
    let score: i64 = r.get(9)?;
    let exit_code: Option<i64> = r.get(12)?;
    Ok(ErrorRec {
        id: r.get(0)?,
        ts: r.get(1)?,
        last_ts: r.get(2)?,
        bump_ts: r.get(13)?,
        key: r.get(3)?,
        source: ErrorSource::parse(&r.get::<_, String>(4)?),
        app: r.get(5)?,
        title: r.get(6)?,
        lines: serde_json::from_str(&lines).unwrap_or_default(),
        count: u32::try_from(count.max(0)).unwrap_or(u32::MAX),
        score: u8::try_from(score.clamp(0, 255)).unwrap_or(u8::MAX),
        frame_id: r.get(10)?,
        cmd: r.get(11)?,
        exit_code: exit_code.and_then(|c| i32::try_from(c).ok()),
    })
}

fn command_row(r: &Row) -> rusqlite::Result<CommandRec> {
    let exit_code: Option<i64> = r.get(5)?;
    Ok(CommandRec {
        id: r.get(0)?,
        ts: r.get(1)?,
        shell: r.get(2)?,
        cwd: r.get(3)?,
        cmd: r.get(4)?,
        exit_code: exit_code.and_then(|c| i32::try_from(c).ok()),
        duration_ms: r.get(6)?,
    })
}

fn insight_from(id: i64, ts: Ts, trigger: String, model: String, json: &str) -> Insight {
    match serde_json::from_str::<Insight>(json) {
        Ok(mut i) => {
            i.id = id;
            i.ts = ts;
            i
        }
        // A damaged blob still yields a row with what the columns know.
        Err(_) => Insight {
            id,
            ts,
            trigger,
            model,
            ..Insight::default()
        },
    }
}

fn insight_row(r: &Row) -> rusqlite::Result<Insight> {
    let json: String = r.get(4)?;
    Ok(insight_from(
        r.get(0)?,
        r.get(1)?,
        r.get(2)?,
        r.get(3)?,
        &json,
    ))
}

// ---- file helpers --------------------------------------------------------

/// A stored image path is only ever deleted when it is a plain relative path with
/// no `..`, root, or drive component, on any platform's separator rules.
fn is_safe_rel(rel: &str) -> bool {
    if rel.is_empty() || rel.contains('\0') {
        return false;
    }
    if rel.starts_with('/') || rel.starts_with('\\') {
        return false;
    }
    let b = rel.as_bytes();
    if b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':' {
        return false;
    }
    if rel.split(['/', '\\']).any(|s| s == "..") {
        return false;
    }
    let p = Path::new(rel);
    if p.is_absolute() || p.has_root() {
        return false;
    }
    p.components()
        .all(|c| matches!(c, Component::Normal(_) | Component::CurDir))
}

/// Canonical form used to compare stored paths with paths found on disk.
fn norm_rel(s: &str) -> String {
    s.split(['/', '\\'])
        .filter(|p| !p.is_empty() && *p != ".")
        .collect::<Vec<_>>()
        .join("/")
}

struct FileInfo {
    path: PathBuf,
    mtime: Option<f64>,
}

fn walk_files(dir: &Path, depth: u32, out: &mut Vec<FileInfo>) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    for entry in rd.flatten() {
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_dir() {
            if depth > 0 {
                walk_files(&entry.path(), depth - 1, out);
            }
        } else if ft.is_file() {
            let Ok(md) = entry.metadata() else { continue };
            let mtime = md
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs_f64());
            out.push(FileInfo {
                path: entry.path(),
                mtime,
            });
        }
    }
}

// ---- search helpers ------------------------------------------------------

/// Terms shorter than this cannot be matched by the trigram tokenizer.
const TRIGRAM_MIN: usize = 3;

fn is_short(term: &str) -> bool {
    term.chars().count() < TRIGRAM_MIN
}

/// Turn a user query into a safe FTS5 MATCH expression for the trigram tokenizer:
/// each whitespace-separated term becomes a double-quoted phrase (embedded `"`
/// doubled), terms ANDed.
///
/// Terms shorter than 3 characters are left out of the expression (the tokenizer
/// cannot match them; [`Store::frames_search`] applies them as `LIKE` filters).
/// Returns `None` when no term has ≥ 3 chars — `frames_search` then uses `LIKE`
/// for the whole query.
pub fn fts_query(query: &str) -> Option<String> {
    let parts: Vec<String> = query
        .split_whitespace()
        .filter(|t| !is_short(t))
        .map(|t| format!("\"{}\"", t.replace('"', "\"\"")))
        .collect();
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" AND "))
    }
}

fn like_pattern(term: &str) -> String {
    let mut s = String::with_capacity(term.len() + 2);
    s.push('%');
    for c in term.chars() {
        if matches!(c, '%' | '_' | '\\') {
            s.push('\\');
        }
        s.push(c);
    }
    s.push('%');
    s
}

fn fold(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

fn find_chars(hay: &[char], needle: &[char]) -> Option<usize> {
    if needle.is_empty() || needle.len() > hay.len() {
        return None;
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

fn cut_snippet(chars: &[char], pos: usize, len: usize) -> String {
    let start = pos.saturating_sub(SNIPPET_SIDE);
    let end = (pos + len + SNIPPET_SIDE).min(chars.len());
    let raw: String = chars[start..end].iter().collect();
    let body = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out = String::with_capacity(body.len() + 6);
    if start > 0 {
        out.push('…');
    }
    out.push_str(&body);
    if end < chars.len() {
        out.push('…');
    }
    out
}

/// ~160 chars around the earliest occurrence of any term (case-insensitive) in the
/// text, else in the title, else the start of the text. Works on `char`s, so CJK
/// and other multi-byte text is never split mid-character.
fn make_snippet(text: &str, title: &str, terms: &[&str]) -> String {
    let needles: Vec<Vec<char>> = terms
        .iter()
        .map(|t| t.chars().map(fold).collect())
        .collect();
    for hay in [text, title] {
        let chars: Vec<char> = hay.chars().collect();
        let folded: Vec<char> = chars.iter().map(|c| fold(*c)).collect();
        let mut best: Option<(usize, usize)> = None;
        for n in &needles {
            if let Some(p) = find_chars(&folded, n) {
                if best.is_none_or(|(bp, _)| p < bp) {
                    best = Some((p, n.len()));
                }
            }
        }
        if let Some((p, n)) = best {
            return cut_snippet(&chars, p, n);
        }
    }
    let src = if text.trim().is_empty() { title } else { text };
    let chars: Vec<char> = src.chars().take(SNIPPET_SIDE * 2).collect();
    cut_snippet(&chars, 0, 0)
}

// ---- Store ---------------------------------------------------------------

impl Store {
    /// Open (creating the directory and file if needed) `<data_dir>/screensense.db`, apply migrations,
    /// set WAL, synchronous=NORMAL, busy_timeout=3000ms.
    pub fn open(data_dir: &Path) -> crate::Result<Store> {
        fs::create_dir_all(data_dir)?;
        let conn = Connection::open(crate::paths::db_path(data_dir))?;
        conn.busy_timeout(BUSY_TIMEOUT)?;
        // The very first moments of a fresh file (switching to WAL, creating the schema)
        // can report "database is locked" without consulting the busy handler when
        // several processes start at once, so this setup retries on its own.
        retry_busy(|| {
            // Refuse a newer database before changing anything about it.
            let found = user_version(&conn)?;
            let known = lim(MIGRATIONS.len());
            if found > known {
                return Err(newer_error(found, known));
            }
            conn.pragma_update_and_check(None, "journal_mode", "WAL", |r| r.get::<_, String>(0))?;
            conn.pragma_update(None, "synchronous", "NORMAL")?;
            conn.pragma_update(None, "secure_delete", "ON")?;
            migrate(&conn)?;
            conn.execute(
                "INSERT INTO frames_fts(frames_fts, rank) VALUES('secure-delete', 1)",
                [],
            )?;
            Ok(())
        })?;
        Ok(Store {
            conn,
            data_dir: Some(data_dir.to_path_buf()),
        })
    }

    /// In-memory database for tests (no data dir; image deletion is a no-op).
    pub fn open_in_memory() -> crate::Result<Store> {
        let conn = Connection::open_in_memory()?;
        conn.busy_timeout(BUSY_TIMEOUT)?;
        migrate(&conn)?;
        conn.pragma_update(None, "secure_delete", "ON")?;
        conn.execute(
            "INSERT INTO frames_fts(frames_fts, rank) VALUES('secure-delete', 1)",
            [],
        )?;
        Ok(Store {
            conn,
            data_dir: None,
        })
    }

    pub fn data_dir(&self) -> Option<&Path> {
        self.data_dir.as_deref()
    }

    fn immediate(&self) -> crate::Result<Transaction<'_>> {
        Ok(Transaction::new_unchecked(
            &self.conn,
            TransactionBehavior::Immediate,
        )?)
    }

    fn query_vec<T, P: rusqlite::Params>(
        &self,
        sql: &str,
        p: P,
        f: fn(&Row) -> rusqlite::Result<T>,
    ) -> crate::Result<Vec<T>> {
        let mut st = self.conn.prepare_cached(sql)?;
        let rows = st.query_map(p, f)?;
        Ok(rows.collect::<rusqlite::Result<Vec<T>>>()?)
    }

    fn query_opt<T, P: rusqlite::Params>(
        &self,
        sql: &str,
        p: P,
        f: fn(&Row) -> rusqlite::Result<T>,
    ) -> crate::Result<Option<T>> {
        let mut st = self.conn.prepare_cached(sql)?;
        Ok(st.query_row(p, f).optional()?)
    }

    /// Delete an image file named by a stored relative path. Refuses anything that
    /// could leave the data dir; a missing file is fine. Failed removal is reported
    /// without revealing the stored path; callers preserve references for retry.
    fn remove_image(&self, rel: &str) -> crate::Result<u64> {
        let Some(dir) = &self.data_dir else {
            return Ok(0);
        };
        if !is_safe_rel(rel) {
            return Err(crate::Error::other(
                "unsafe stored image path; image was not deleted",
            ));
        }
        let path = dir.join(rel);
        let size = fs::symlink_metadata(&path).map(|m| m.len()).unwrap_or(0);
        match fs::remove_file(&path) {
            Ok(()) => Ok(size),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(0),
            Err(e) => Err(crate::Error::other(format!(
                "stored image could not be deleted ({}); image may remain on disk",
                e.kind()
            ))),
        }
    }

    /// Cross-process generation for privacy changes, pause, stop and purge.
    pub fn privacy_revision(&self) -> crate::Result<u64> {
        Ok(self
            .meta_get("control.privacy_revision")?
            .and_then(|s| s.parse().ok())
            .unwrap_or(0))
    }

    fn bump_privacy_revision(&self) -> crate::Result<()> {
        self.conn.execute("INSERT INTO meta(key, value) VALUES('control.privacy_revision', '1') ON CONFLICT(key) DO UPDATE SET value = CAST(value AS INTEGER) + 1", [])?;
        Ok(())
    }

    pub fn invalidate_privacy(&self) -> crate::Result<()> {
        let tx = self.immediate()?;
        self.bump_privacy_revision()?;
        self.conn.execute("DELETE FROM insights", [])?;
        tx.commit()?;
        Ok(())
    }

    fn recording_allowed(&self, revision: u64, now: Ts) -> crate::Result<bool> {
        let control = self.control()?;
        Ok(self.privacy_revision()? == revision
            && !control.stop
            && !control.paused_until.is_some_and(|t| t > now))
    }

    /// Serialize a final write with pause/purge. The closure must not open another transaction.
    pub fn recording_write<T>(
        &self,
        revision: u64,
        now: Ts,
        write: impl FnOnce() -> crate::Result<T>,
    ) -> crate::Result<Option<T>> {
        let tx = self.immediate()?;
        if !self.recording_allowed(revision, now)? {
            return Ok(None);
        }
        let value = write()?;
        tx.commit()?;
        Ok(Some(value))
    }

    /// Scrub FTS shadow pages and truncate the WAL after a successful privacy deletion.
    pub fn scrub_deleted(&self) -> crate::Result<()> {
        self.conn
            .execute("INSERT INTO frames_fts(frames_fts) VALUES('optimize')", [])?;
        let busy: i64 = self
            .conn
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| r.get(0))?;
        if busy != 0 {
            return Err(crate::Error::other("privacy rows deleted; database checkpoint is busy, retry purge after closing readers"));
        }
        Ok(())
    }

    // ---- meta (key/value) ----

    pub fn meta_get(&self, key: &str) -> crate::Result<Option<String>> {
        self.query_opt("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
    }

    pub fn meta_set(&self, key: &str, value: &str) -> crate::Result<()> {
        self.conn
            .prepare_cached(
                "INSERT INTO meta(key, value) VALUES (?1, ?2) \
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            )?
            .execute(params![key, value])?;
        Ok(())
    }

    pub fn meta_delete(&self, key: &str) -> crate::Result<()> {
        self.conn
            .prepare_cached("DELETE FROM meta WHERE key = ?1")?
            .execute([key])?;
        Ok(())
    }

    /// Daemon liveness: JSON of `Heartbeat` under key "heartbeat". A corrupt value reads as None.
    pub fn heartbeat(&self) -> crate::Result<Option<Heartbeat>> {
        Ok(self
            .meta_get("heartbeat")?
            .and_then(|s| serde_json::from_str(&s).ok()))
    }

    pub fn set_heartbeat(&self, hb: &Heartbeat) -> crate::Result<()> {
        self.meta_set("heartbeat", &serde_json::to_string(hb)?)
    }

    /// CLI→daemon requests under keys "control.stop" ("1"/absent) and "control.paused_until" (float text).
    pub fn control(&self) -> crate::Result<Control> {
        let stop = self.meta_get("control.stop")?.as_deref() == Some("1");
        let paused_until = self
            .meta_get("control.paused_until")?
            .and_then(|s| s.trim().parse::<f64>().ok())
            .filter(|t| t.is_finite());
        Ok(Control { stop, paused_until })
    }

    pub fn set_stop(&self, stop: bool) -> crate::Result<()> {
        let tx = self.immediate()?;
        if stop {
            self.bump_privacy_revision()?;
            self.meta_set("control.stop", "1")?;
        } else {
            self.meta_delete("control.stop")?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn set_paused_until(&self, until: Option<Ts>) -> crate::Result<()> {
        let tx = self.immediate()?;
        self.bump_privacy_revision()?;
        match until {
            Some(t) => self.meta_set("control.paused_until", &t.to_string())?,
            None => self.meta_delete("control.paused_until")?,
        }
        tx.commit()?;
        Ok(())
    }

    // ---- spans (foreground-window timeline) ----

    /// Insert a span with start == end == `start`.
    pub fn span_open(&self, start: Ts, app: &str, title: &str, tier: Tier) -> crate::Result<i64> {
        let mut st = self.conn.prepare_cached(
            r#"INSERT INTO spans(start, "end", app, title, tier) VALUES (?1, ?1, ?2, ?3, ?4)"#,
        )?;
        Ok(st.insert(params![start, app, title, tier.as_str()])?)
    }

    /// Move a span's end forward to `end` (never backward).
    pub fn span_touch(&self, id: i64, end: Ts) -> crate::Result<()> {
        self.conn
            .prepare_cached(r#"UPDATE spans SET "end" = MAX("end", ?2) WHERE id = ?1"#)?
            .execute(params![id, end])?;
        Ok(())
    }

    pub fn span_set_end(&self, id: i64, end: Ts) -> crate::Result<()> {
        self.conn.execute(
            "UPDATE spans SET \"end\" = MAX(start, ?2) WHERE id = ?1",
            params![id, end],
        )?;
        Ok(())
    }

    /// Spans overlapping [since, until] (edges inclusive), ordered by start ascending.
    pub fn spans_between(&self, since: Ts, until: Ts) -> crate::Result<Vec<Span>> {
        self.query_vec(
            &format!(
                r#"SELECT {SPAN_COLS} FROM spans WHERE "end" >= ?1 AND start <= ?2 ORDER BY start, id"#
            ),
            params![since, until],
            span_row,
        )
    }

    pub fn span_latest(&self) -> crate::Result<Option<Span>> {
        self.query_opt(
            &format!("SELECT {SPAN_COLS} FROM spans ORDER BY start DESC, id DESC LIMIT 1"),
            [],
            span_row,
        )
    }

    /// Delete spans shorter than `min_s` that ended before `before` (housekeeping for flicker).
    pub fn spans_delete_short(&self, min_s: f64, before: Ts) -> crate::Result<u64> {
        let n = self
            .conn
            .prepare_cached(r#"DELETE FROM spans WHERE "end" - start < ?1 AND "end" < ?2"#)?
            .execute(params![min_s, before])?;
        Ok(n as u64)
    }

    // ---- frames ----

    pub fn frame_insert(&self, f: &NewFrame) -> crate::Result<i64> {
        let mut st = self.conn.prepare_cached(
            "INSERT INTO frames(ts, app, title, text, image, reason) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )?;
        Ok(st.insert(params![f.ts, f.app, f.title, f.text, f.image, f.reason])?)
    }

    pub fn frame_get(&self, id: i64) -> crate::Result<Option<Frame>> {
        self.query_opt(
            &format!("SELECT {FRAME_COLS} FROM frames WHERE id = ?1"),
            [id],
            frame_row,
        )
    }

    /// Most recent frame with ts >= since.
    pub fn frame_latest(&self, since: Ts) -> crate::Result<Option<Frame>> {
        self.query_opt(
            &format!(
                "SELECT {FRAME_COLS} FROM frames WHERE ts >= ?1 ORDER BY ts DESC, id DESC LIMIT 1"
            ),
            [since],
            frame_row,
        )
    }

    /// Most recent frame that has an image, ts >= since.
    pub fn frame_latest_with_image(&self, since: Ts) -> crate::Result<Option<Frame>> {
        self.query_opt(
            &format!(
                "SELECT {FRAME_COLS} FROM frames WHERE ts >= ?1 AND image IS NOT NULL \
                 ORDER BY ts DESC, id DESC LIMIT 1"
            ),
            [since],
            frame_row,
        )
    }

    /// Newest first.
    pub fn frames_since(&self, since: Ts, limit: usize) -> crate::Result<Vec<Frame>> {
        self.query_vec(
            &format!(
                "SELECT {FRAME_COLS} FROM frames WHERE ts >= ?1 ORDER BY ts DESC, id DESC LIMIT ?2"
            ),
            params![since, lim(limit)],
            frame_row,
        )
    }

    /// Substring search over frame text and title, ts >= since, newest first.
    ///
    /// Terms (whitespace-separated, all must match) of 3+ characters go through the
    /// FTS5 trigram index; shorter terms, which the tokenizer cannot match, are
    /// applied as escaped `LIKE '%term%'` filters. Matching is case-insensitive.
    pub fn frames_search(
        &self,
        query: &str,
        since: Ts,
        limit: usize,
    ) -> crate::Result<Vec<FrameHit>> {
        let terms: Vec<&str> = query.split_whitespace().collect();
        if terms.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let fts = fts_query(query);
        let mut args: Vec<Value> = Vec::new();
        let mut sql = format!("SELECT {FRAME_COLS_F} FROM ");
        if let Some(m) = &fts {
            sql.push_str("frames_fts JOIN frames f ON f.id = frames_fts.rowid WHERE frames_fts MATCH ?1 AND f.ts >= ?2");
            args.push(Value::Text(m.clone()));
        } else {
            sql.push_str("frames f WHERE f.ts >= ?1");
        }
        args.push(Value::Real(since));
        for t in &terms {
            if fts.is_some() && !is_short(t) {
                continue;
            }
            args.push(Value::Text(like_pattern(t)));
            let n = args.len();
            sql.push_str(&format!(
                " AND (f.text LIKE ?{n} ESCAPE '\\' OR f.title LIKE ?{n} ESCAPE '\\')"
            ));
        }
        args.push(Value::Integer(lim(limit)));
        sql.push_str(&format!(
            " ORDER BY f.ts DESC, f.id DESC LIMIT ?{}",
            args.len()
        ));

        let mut st = self.conn.prepare_cached(&sql)?;
        let frames = st
            .query_map(params_from_iter(args.iter()), frame_row)?
            .collect::<rusqlite::Result<Vec<Frame>>>()?;
        Ok(frames
            .into_iter()
            .map(|frame| {
                let snippet = make_snippet(&frame.text, &frame.title, &terms);
                FrameHit { frame, snippet }
            })
            .collect())
    }

    /// Delete one frame, its FTS row and its image file (if any, and if a data dir exists).
    pub fn frame_delete(&self, id: i64) -> crate::Result<()> {
        let tx = self.immediate()?;
        let image: Option<String> = tx
            .query_row("SELECT image FROM frames WHERE id = ?1", [id], |r| r.get(0))
            .optional()?
            .flatten();
        if let Some(rel) = image {
            self.remove_image(&rel)?;
        }
        tx.execute("DELETE FROM frames WHERE id = ?1", [id])?;
        tx.commit()?;
        Ok(())
    }

    /// Delete the image before clearing its reference; failed removal remains retryable.
    pub fn frame_drop_image(&self, id: i64) -> crate::Result<()> {
        let tx = self.immediate()?;
        let image: Option<String> = tx
            .query_row("SELECT image FROM frames WHERE id = ?1", [id], |r| r.get(0))
            .optional()?
            .flatten();
        if let Some(rel) = image {
            self.remove_image(&rel)?;
        }
        tx.execute("UPDATE frames SET image = NULL WHERE id = ?1", [id])?;
        tx.commit()?;
        Ok(())
    }

    // ---- errors ----

    /// If a row with the same `key` has last_ts within `repeat_window_s` of e.ts:
    ///   - if e.ts - last_ts <= dedupe_s → same sighting: update last_ts (and lines/frame_id/score if
    ///     the new score is higher); is_new=false, bumped=false.
    ///   - else → re-appearance: count += 1, update last_ts, lines, frame_id, score=max; bumped=true.
    ///
    /// Otherwise insert a new row with count = 1; is_new=true, bumped=true.
    pub fn error_upsert(
        &self,
        e: &NewError,
        dedupe_s: f64,
        repeat_window_s: f64,
    ) -> crate::Result<ErrorUpsert> {
        self.error_upsert_inner(e, dedupe_s, repeat_window_s, None)
    }

    pub fn error_upsert_guarded(
        &self,
        e: &NewError,
        dedupe_s: f64,
        repeat_window_s: f64,
        revision: u64,
        now: Ts,
    ) -> crate::Result<ErrorUpsert> {
        self.error_upsert_inner(e, dedupe_s, repeat_window_s, Some((revision, now)))
    }

    fn error_upsert_inner(
        &self,
        e: &NewError,
        dedupe_s: f64,
        repeat_window_s: f64,
        guard: Option<(u64, Ts)>,
    ) -> crate::Result<ErrorUpsert> {
        let lines = serde_json::to_string(&e.lines)?;
        let score = i64::from(e.score);
        let tx = self.immediate()?;
        if let Some((revision, now)) = guard {
            if !self.recording_allowed(revision, now)? {
                return Err(crate::Error::other(
                    "recording stopped or privacy rules changed",
                ));
            }
        }
        let existing: Option<(i64, f64, i64, i64)> = tx
            .prepare_cached(
                "SELECT id, last_ts, count, score FROM errors \
                 WHERE key = ?1 AND last_ts >= ?2 ORDER BY last_ts DESC, id DESC LIMIT 1",
            )?
            .query_row(params![e.key, e.ts - repeat_window_s], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })
            .optional()?;
        let clamp = |c: i64| u32::try_from(c.max(0)).unwrap_or(u32::MAX);
        let out = match existing {
            None => {
                let id = tx
                    .prepare_cached(
                        "INSERT INTO errors(ts, last_ts, bump_ts, key, source, app, title, lines, \
                         count, score, frame_id, cmd, exit_code) \
                         VALUES (?1, ?1, ?1, ?2, ?3, ?4, ?5, ?6, 1, ?7, ?8, ?9, ?10)",
                    )?
                    .insert(params![
                        e.ts,
                        e.key,
                        e.source.as_str(),
                        e.app,
                        e.title,
                        lines,
                        score,
                        e.frame_id,
                        e.cmd,
                        e.exit_code
                    ])?;
                ErrorUpsert {
                    id,
                    is_new: true,
                    bumped: true,
                    count: 1,
                }
            }
            Some((id, last_ts, count, old_score)) if e.ts - last_ts <= dedupe_s => {
                if score > old_score {
                    tx.prepare_cached(
                        "UPDATE errors SET last_ts = MAX(last_ts, ?2), lines = ?3, \
                         frame_id = COALESCE(?4, frame_id), score = ?5 WHERE id = ?1",
                    )?
                    .execute(params![id, e.ts, lines, e.frame_id, score])?;
                } else {
                    tx.prepare_cached(
                        "UPDATE errors SET last_ts = MAX(last_ts, ?2) WHERE id = ?1",
                    )?
                    .execute(params![id, e.ts])?;
                }
                ErrorUpsert {
                    id,
                    is_new: false,
                    bumped: false,
                    count: clamp(count),
                }
            }
            Some((id, _, count, _)) => {
                tx.prepare_cached(
                    "UPDATE errors SET last_ts = MAX(last_ts, ?2), bump_ts = MAX(bump_ts, ?2), \
                     lines = ?3, frame_id = COALESCE(?4, frame_id), score = MAX(score, ?5), \
                     count = count + 1, cmd = COALESCE(?6, cmd), \
                     exit_code = COALESCE(?7, exit_code) WHERE id = ?1",
                )?
                .execute(params![
                    id,
                    e.ts,
                    lines,
                    e.frame_id,
                    score,
                    e.cmd,
                    e.exit_code
                ])?;
                ErrorUpsert {
                    id,
                    is_new: false,
                    bumped: true,
                    count: clamp(count.saturating_add(1)),
                }
            }
        };
        tx.commit()?;
        Ok(out)
    }

    /// Errors whose last_ts >= since, ordered by score desc then last_ts desc.
    pub fn errors_since(&self, since: Ts, limit: usize) -> crate::Result<Vec<ErrorRec>> {
        self.query_vec(
            &format!(
                "SELECT {ERROR_COLS} FROM errors WHERE last_ts >= ?1 \
                 ORDER BY score DESC, last_ts DESC, id DESC LIMIT ?2"
            ),
            params![since, lim(limit)],
            error_row,
        )
    }

    pub fn error_get(&self, id: i64) -> crate::Result<Option<ErrorRec>> {
        self.query_opt(
            &format!("SELECT {ERROR_COLS} FROM errors WHERE id = ?1"),
            [id],
            error_row,
        )
    }

    pub fn error_delete(&self, id: i64) -> crate::Result<()> {
        self.conn
            .prepare_cached("DELETE FROM errors WHERE id = ?1")?
            .execute([id])?;
        Ok(())
    }

    // ---- shell commands ----

    pub fn command_insert(&self, c: &NewCommand) -> crate::Result<i64> {
        let mut st = self.conn.prepare_cached(
            "INSERT INTO commands(ts, shell, cwd, cmd, exit_code, duration_ms) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )?;
        Ok(st.insert(params![
            c.ts,
            c.shell,
            c.cwd,
            c.cmd,
            c.exit_code,
            c.duration_ms
        ])?)
    }

    /// Newest first.
    pub fn commands_since(&self, since: Ts, limit: usize) -> crate::Result<Vec<CommandRec>> {
        self.query_vec(
            &format!(
                "SELECT {COMMAND_COLS} FROM commands WHERE ts >= ?1 ORDER BY ts DESC, id DESC LIMIT ?2"
            ),
            params![since, lim(limit)],
            command_row,
        )
    }

    pub fn command_delete(&self, id: i64) -> crate::Result<()> {
        self.conn
            .prepare_cached("DELETE FROM commands WHERE id = ?1")?
            .execute([id])?;
        Ok(())
    }

    // ---- insights ----

    /// Stores the whole Insight as JSON; returns the id. `insight.id` is ignored on insert.
    pub fn insight_insert(&self, insight: &Insight) -> crate::Result<i64> {
        let json = serde_json::to_string(insight)?;
        let mut st = self.conn.prepare_cached(
            r#"INSERT INTO insights(ts, "trigger", model, json) VALUES (?1, ?2, ?3, ?4)"#,
        )?;
        Ok(st.insert(params![insight.ts, insight.trigger, insight.model, json])?)
    }

    /// With id/ts filled from the row.
    pub fn insight_latest(&self, since: Ts) -> crate::Result<Option<Insight>> {
        self.query_opt(
            r#"SELECT id, ts, "trigger", model, json FROM insights WHERE ts >= ?1
               ORDER BY ts DESC, id DESC LIMIT 1"#,
            [since],
            insight_row,
        )
    }

    /// Newest first.
    pub fn insights_since(&self, since: Ts, limit: usize) -> crate::Result<Vec<Insight>> {
        self.query_vec(
            r#"SELECT id, ts, "trigger", model, json FROM insights WHERE ts >= ?1
               ORDER BY ts DESC, id DESC LIMIT ?2"#,
            params![since, lim(limit)],
            insight_row,
        )
    }

    pub fn insights_count_since(&self, since: Ts) -> crate::Result<u32> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM insights WHERE ts >= ?1",
            [since],
            |r| r.get(0),
        )?;
        Ok(u32::try_from(n.max(0)).unwrap_or(u32::MAX))
    }

    // ---- hook sessions ----

    pub fn session_get(&self, id: &str) -> crate::Result<Option<Session>> {
        self.query_opt(
            "SELECT id, last_ts, full_ts FROM sessions WHERE id = ?1",
            [id],
            |r| {
                Ok(Session {
                    id: r.get(0)?,
                    last_ts: r.get(1)?,
                    full_ts: r.get(2)?,
                })
            },
        )
    }

    /// Upsert.
    pub fn session_set(&self, s: &Session) -> crate::Result<()> {
        self.conn
            .prepare_cached(
                "INSERT INTO sessions(id, last_ts, full_ts) VALUES (?1, ?2, ?3) \
                 ON CONFLICT(id) DO UPDATE SET last_ts = excluded.last_ts, full_ts = excluded.full_ts",
            )?
            .execute(params![s.id, s.last_ts, s.full_ts])?;
        Ok(())
    }

    pub fn sessions_prune(&self, older_than: Ts) -> crate::Result<u64> {
        let n = self
            .conn
            .prepare_cached("DELETE FROM sessions WHERE last_ts < ?1")?
            .execute([older_than])?;
        Ok(n as u64)
    }

    // ---- whole-table iteration for the retroactive privacy purge (ordered by id) ----

    /// Rows with id > after_id, up to `limit`. The purge pages through with after_id = last seen id.
    pub fn spans_page(&self, after_id: i64, limit: usize) -> crate::Result<Vec<Span>> {
        self.query_vec(
            &format!("SELECT {SPAN_COLS} FROM spans WHERE id > ?1 ORDER BY id LIMIT ?2"),
            params![after_id, lim(limit)],
            span_row,
        )
    }

    pub fn frames_page(&self, after_id: i64, limit: usize) -> crate::Result<Vec<Frame>> {
        self.query_vec(
            &format!("SELECT {FRAME_COLS} FROM frames WHERE id > ?1 ORDER BY id LIMIT ?2"),
            params![after_id, lim(limit)],
            frame_row,
        )
    }

    pub fn errors_page(&self, after_id: i64, limit: usize) -> crate::Result<Vec<ErrorRec>> {
        self.query_vec(
            &format!("SELECT {ERROR_COLS} FROM errors WHERE id > ?1 ORDER BY id LIMIT ?2"),
            params![after_id, lim(limit)],
            error_row,
        )
    }

    pub fn commands_page(&self, after_id: i64, limit: usize) -> crate::Result<Vec<CommandRec>> {
        self.query_vec(
            &format!("SELECT {COMMAND_COLS} FROM commands WHERE id > ?1 ORDER BY id LIMIT ?2"),
            params![after_id, lim(limit)],
            command_row,
        )
    }

    /// Rewrite a span's privacy fields (used when a rule now hides it: title "", tier Hidden).
    pub fn span_set_privacy(&self, id: i64, title: &str, tier: Tier) -> crate::Result<()> {
        self.conn
            .prepare_cached("UPDATE spans SET title = ?2, tier = ?3 WHERE id = ?1")?
            .execute(params![id, title, tier.as_str()])?;
        Ok(())
    }

    pub fn span_delete(&self, id: i64) -> crate::Result<()> {
        self.conn
            .prepare_cached("DELETE FROM spans WHERE id = ?1")?
            .execute([id])?;
        Ok(())
    }

    /// Delete every frame and error (and frame images) for this exact app + title.
    /// Returns the number of rows (frames + errors) removed.
    pub fn forget_window(&self, app: &str, title: &str) -> crate::Result<u64> {
        let removed = {
            let tx = self.immediate()?;
            let images: Vec<String> = {
                let mut st = tx.prepare_cached(
                    "SELECT image FROM frames WHERE app = ?1 AND title = ?2 AND image IS NOT NULL",
                )?;
                let rows = st.query_map(params![app, title], |r| r.get::<_, String>(0))?;
                rows.collect::<rusqlite::Result<Vec<_>>>()?
            };
            let frames = tx.execute(
                "DELETE FROM frames WHERE app = ?1 AND title = ?2",
                params![app, title],
            )?;
            let errors = tx.execute(
                "DELETE FROM errors WHERE app = ?1 AND title = ?2",
                params![app, title],
            )?;
            for rel in &images {
                self.remove_image(rel)?;
            }
            tx.commit()?;
            (frames + errors) as u64
        };
        Ok(removed)
    }

    pub fn window_record_counts(&self, app: &str, title: &str) -> crate::Result<(u64, u64)> {
        let frames: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM frames WHERE app = ?1 AND title = ?2",
            params![app, title],
            |r| r.get(0),
        )?;
        let errors: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM errors WHERE app = ?1 AND title = ?2",
            params![app, title],
            |r| r.get(0),
        )?;
        Ok((frames as u64, errors as u64))
    }

    // ---- retention ----

    /// Apply RetentionConfig at time `now`: null out + delete image files older than images_hours and,
    /// oldest first, until the images total ≤ max_images_mb; delete frames older than frames_days;
    /// spans/commands/errors older than timeline_days; insights older than insights_days; prune
    /// sessions idle > 2 days; remove empty image day-directories; then `PRAGMA wal_checkpoint(TRUNCATE)`
    /// and `PRAGMA optimize`. Also delete orphan files under `<data>/images` that no frame references
    /// and that are older than 10 minutes.
    ///
    /// The size cap needs a data dir (to measure files) and is skipped for in-memory stores.
    pub fn sweep(&self, now: Ts, cfg: &RetentionConfig) -> crate::Result<SweepStats> {
        let mut st = SweepStats::default();
        let mut doomed: Vec<String> = Vec::new();
        {
            let tx = self.immediate()?;

            // 1. Images past their age.
            let image_cut = now - cfg.images_hours * 3600.0;
            {
                let mut q = tx.prepare_cached(
                    "SELECT image FROM frames WHERE image IS NOT NULL AND ts < ?1",
                )?;
                let rows = q.query_map([image_cut], |r| r.get::<_, String>(0))?;
                for r in rows {
                    doomed.push(r?);
                }
            }
            tx.execute(
                "UPDATE frames SET image = NULL WHERE image IS NOT NULL AND ts < ?1",
                [image_cut],
            )?;

            // 2. Frames past their age (their images go with them).
            let frame_cut = now - cfg.frames_days * 86_400.0;
            {
                let mut q = tx.prepare_cached(
                    "SELECT image FROM frames WHERE image IS NOT NULL AND ts < ?1",
                )?;
                let rows = q.query_map([frame_cut], |r| r.get::<_, String>(0))?;
                for r in rows {
                    doomed.push(r?);
                }
            }
            st.frames_deleted = tx.execute("DELETE FROM frames WHERE ts < ?1", [frame_cut])? as u64;

            // 3. Size cap, oldest first.
            if let Some(dir) = &self.data_dir {
                let limit = cfg.max_images_mb.saturating_mul(1024 * 1024);
                let rows: Vec<(i64, String)> = {
                    let mut q = tx.prepare_cached(
                        "SELECT id, image FROM frames WHERE image IS NOT NULL ORDER BY ts, id",
                    )?;
                    let it = q.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
                    it.collect::<rusqlite::Result<Vec<_>>>()?
                };
                // `None` = the file is gone: the reference is dangling and goes too.
                let sized: Vec<(i64, String, Option<u64>)> = rows
                    .into_iter()
                    .map(|(id, rel)| {
                        let size = if is_safe_rel(&rel) {
                            fs::metadata(dir.join(&rel)).ok().map(|m| m.len())
                        } else {
                            Some(0)
                        };
                        (id, rel, size)
                    })
                    .collect();
                let mut total: u64 = sized.iter().filter_map(|(_, _, s)| *s).sum();
                let mut q = tx.prepare_cached("UPDATE frames SET image = NULL WHERE id = ?1")?;
                for (id, rel, size) in sized {
                    match size {
                        None => {
                            q.execute([id])?;
                            st.images_deleted += 1;
                        }
                        Some(sz) if total > limit => {
                            q.execute([id])?;
                            doomed.push(rel);
                            total = total.saturating_sub(sz);
                        }
                        Some(_) => {}
                    }
                }
            }

            // 4. Timeline, commands, errors, insights, sessions.
            let timeline_cut = now - cfg.timeline_days * 86_400.0;
            st.spans_deleted =
                tx.execute(r#"DELETE FROM spans WHERE "end" < ?1"#, [timeline_cut])? as u64;
            st.commands_deleted =
                tx.execute("DELETE FROM commands WHERE ts < ?1", [timeline_cut])? as u64;
            st.errors_deleted =
                tx.execute("DELETE FROM errors WHERE last_ts < ?1", [timeline_cut])? as u64;
            let insight_cut = now - cfg.insights_days * 86_400.0;
            st.insights_deleted =
                tx.execute("DELETE FROM insights WHERE ts < ?1", [insight_cut])? as u64;
            tx.execute(
                "DELETE FROM sessions WHERE last_ts < ?1",
                [now - SESSION_MAX_IDLE_S],
            )?;
            // Remove files before commit, retaining references on any deletion failure.
            for rel in &doomed {
                st.image_bytes_freed += self.remove_image(rel)?;
            }
            tx.commit()?;
        }
        st.images_deleted += doomed.len() as u64;
        self.remove_orphans(now, &mut st)?;
        self.remove_empty_day_dirs();

        // Housekeeping is best effort: a busy checkpoint must not fail the sweep.
        let _ = self
            .conn
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()));
        let _ = self.conn.execute_batch("PRAGMA optimize");
        Ok(st)
    }

    /// Delete files under `<data>/images` that no frame references and that are
    /// older than 10 minutes.
    fn remove_orphans(&self, now: Ts, st: &mut SweepStats) -> crate::Result<()> {
        let Some(dir) = &self.data_dir else {
            return Ok(());
        };
        let root = crate::paths::images_dir(dir);
        let mut files = Vec::new();
        walk_files(&root, 4, &mut files);
        if files.is_empty() {
            return Ok(());
        }
        let referenced: HashSet<String> = {
            let mut q = self
                .conn
                .prepare_cached("SELECT image FROM frames WHERE image IS NOT NULL")?;
            let rows = q.query_map([], |r| r.get::<_, String>(0))?;
            let mut set = HashSet::new();
            for r in rows {
                set.insert(norm_rel(&r?));
            }
            set
        };
        for f in files {
            let Some(mtime) = f.mtime else { continue };
            if now - mtime < ORPHAN_MIN_AGE_S {
                continue;
            }
            let Ok(rel) = f.path.strip_prefix(dir) else {
                continue;
            };
            let rel = rel
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/");
            if referenced.contains(&rel) {
                continue;
            }
            st.image_bytes_freed += self.remove_image(&rel)?;
            st.images_deleted += 1;
        }
        Ok(())
    }

    /// Remove empty directories directly under `<data>/images` (the day folders).
    fn remove_empty_day_dirs(&self) {
        let Some(dir) = &self.data_dir else { return };
        let Ok(rd) = fs::read_dir(crate::paths::images_dir(dir)) else {
            return;
        };
        for entry in rd.flatten() {
            let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
            if !is_dir {
                continue;
            }
            let empty = fs::read_dir(entry.path())
                .map(|mut d| d.next().is_none())
                .unwrap_or(false);
            if empty {
                // Fails harmlessly if a writer just put a file in it.
                let _ = fs::remove_dir(entry.path());
            }
        }
    }

    /// Delete recorded history and images; preserve configuration, sensitive marks and privacy controls.
    /// For `screensense purge --all`.
    pub fn wipe(&self) -> crate::Result<()> {
        let tx = self.immediate()?;
        for table in [
            "spans", "frames", "errors", "commands", "insights", "sessions",
        ] {
            tx.execute(&format!("DELETE FROM {table}"), [])?;
        }
        self.bump_privacy_revision()?;
        tx.execute("DELETE FROM meta WHERE substr(key, 1, 7) <> 'config.' AND substr(key, 1, 8) <> 'control.'", [])?;
        let files = (|| -> crate::Result<()> {
            if let Some(dir) = &self.data_dir {
                let images = crate::paths::images_dir(dir);
                match fs::remove_dir_all(&images) {
                    Ok(()) => {},
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {},
                    Err(e) => return Err(crate::Error::other(format!("history rows deleted but images could not be removed ({}); retry purge --all", e.kind()))),
                }
                fs::create_dir_all(&images)?;
            }
            Ok(())
        })();
        // Keep row erasure and cancellation even if locked files need a later retry.
        tx.commit()?;
        self.scrub_deleted()?;
        files?;
        self.conn.execute_batch("VACUUM")?;
        self.scrub_deleted()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn release_regression_wipe_preserves_privacy_control() {
        let s = Store::open_in_memory().unwrap();
        s.set_paused_until(Some(1_900_000_000.0)).unwrap();
        s.set_stop(true).unwrap();
        s.wipe().unwrap();
        assert_eq!(s.control().unwrap().paused_until, Some(1_900_000_000.0));
        assert!(s.control().unwrap().stop);
    }

    #[test]
    fn release_regression_image_removal_failure_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path()).unwrap();
        std::fs::create_dir_all(dir.path().join("images/a.jpg/blocked")).unwrap();
        let id = s
            .frame_insert(&frame_with_image(1.0, "a", "t", "images/a.jpg"))
            .unwrap();
        assert!(s.frame_delete(id).is_err());
        assert!(
            s.frame_get(id).unwrap().is_some(),
            "failed removal retains its retry reference"
        );
        fs::remove_dir(dir.path().join("images/a.jpg/blocked")).unwrap();
        fs::remove_dir(dir.path().join("images/a.jpg")).unwrap();
        fs::write(dir.path().join("images/a.jpg"), b"synthetic image").unwrap();
        s.frame_delete(id).unwrap();
        assert!(s.frame_get(id).unwrap().is_none());
        assert!(!dir.path().join("images/a.jpg").exists());
    }

    #[test]
    fn release_regression_privacy_delete_scrubs_database_and_fts_pages() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path()).unwrap();
        let marker = "SYNTHETIC_ERASURE_7e6afc29_".repeat(80);
        let id = s
            .frame_insert(&nf(1.0, "wezterm", "fixture", &marker))
            .unwrap();
        assert_eq!(
            s.frames_search("SYNTHETIC_ERASURE", 0.0, 10).unwrap().len(),
            1
        );
        s.frame_delete(id).unwrap();
        s.scrub_deleted().unwrap();
        assert!(s
            .frames_search("SYNTHETIC_ERASURE", 0.0, 10)
            .unwrap()
            .is_empty());
        for name in ["screensense.db", "screensense.db-wal"] {
            if let Ok(bytes) = fs::read(dir.path().join(name)) {
                assert!(
                    !bytes
                        .windows(b"SYNTHETIC_ERASURE_7e6afc29".len())
                        .any(|w| w == b"SYNTHETIC_ERASURE_7e6afc29"),
                    "{name}: erased fixture bytes remain"
                );
            }
        }
    }

    #[cfg(windows)]
    #[test]
    fn release_regression_wipe_reports_locked_image_and_can_retry() {
        use std::os::windows::fs::OpenOptionsExt;
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path()).unwrap();
        write_file(dir.path(), "images/d/locked.jpg", 20);
        let handle = fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(dir.path().join("images/d/locked.jpg"))
            .unwrap();
        s.set_paused_until(Some(1_900_000_000.0)).unwrap();
        let err = s.wipe().unwrap_err().to_string();
        assert!(err.contains("images could not be removed"), "{err}");
        assert!(!err.contains("locked.jpg"));
        assert!(dir.path().join("images/d/locked.jpg").exists());
        assert_eq!(s.control().unwrap().paused_until, Some(1_900_000_000.0));
        drop(handle);
        s.wipe().unwrap();
        assert!(!dir.path().join("images/d/locked.jpg").exists());
    }
    use super::*;
    use std::time::SystemTime;

    fn mem() -> Store {
        Store::open_in_memory().unwrap()
    }

    fn nf(ts: Ts, app: &str, title: &str, text: &str) -> NewFrame {
        NewFrame {
            ts,
            app: app.into(),
            title: title.into(),
            text: text.into(),
            image: None,
            reason: "change".into(),
        }
    }

    fn ne(ts: Ts, key: &str, score: u8) -> NewError {
        NewError {
            ts,
            key: key.into(),
            source: ErrorSource::Ocr,
            app: "term".into(),
            title: "t".into(),
            lines: vec![format!("line for {key}")],
            score,
            frame_id: None,
            cmd: None,
            exit_code: None,
        }
    }

    fn nc(ts: Ts, cmd: &str) -> NewCommand {
        NewCommand {
            ts,
            shell: "bash".into(),
            cwd: "/tmp".into(),
            cmd: cmd.into(),
            exit_code: Some(0),
            duration_ms: Some(12),
        }
    }

    fn write_file(dir: &Path, rel: &str, bytes: usize) {
        let p = dir.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, vec![b'x'; bytes]).unwrap();
    }

    fn set_age(dir: &Path, rel: &str, age_s: f64) {
        let t = SystemTime::now() - Duration::from_secs_f64(age_s);
        fs::File::options()
            .write(true)
            .open(dir.join(rel))
            .unwrap()
            .set_modified(t)
            .unwrap();
    }

    fn frame_with_image(ts: Ts, app: &str, title: &str, rel: &str) -> NewFrame {
        NewFrame {
            image: Some(rel.into()),
            ..nf(ts, app, title, "some text here")
        }
    }

    fn search_ids(s: &Store, q: &str) -> Vec<i64> {
        s.frames_search(q, 0.0, 100)
            .unwrap()
            .into_iter()
            .map(|h| h.frame.id)
            .collect()
    }

    // ---- open / migrations ----

    #[test]
    fn migrations_are_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("nested").join("data");
        {
            let s = Store::open(&data).unwrap();
            assert_eq!(user_version(&s.conn).unwrap(), MIGRATIONS.len() as i64);
            assert_eq!(s.data_dir(), Some(data.as_path()));
            s.meta_set("k", "v").unwrap();
        }
        let s2 = Store::open(&data).unwrap();
        assert_eq!(user_version(&s2.conn).unwrap(), MIGRATIONS.len() as i64);
        assert_eq!(s2.meta_get("k").unwrap().as_deref(), Some("v"));
        assert!(data.join("screensense.db").exists());
        let mode: String = s2
            .conn
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mode, "wal");
        let busy: i64 = s2
            .conn
            .query_row("PRAGMA busy_timeout", [], |r| r.get(0))
            .unwrap();
        assert_eq!(busy, 3000);
    }

    #[test]
    fn newer_database_is_refused_and_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("screensense.db");
        {
            let c = Connection::open(&path).unwrap();
            c.execute_batch("CREATE TABLE future(x); PRAGMA user_version = 99;")
                .unwrap();
        }
        let err = Store::open(dir.path()).err().expect("must refuse");
        let msg = err.to_string();
        assert!(msg.contains("newer screensense"), "{msg}");
        let c = Connection::open(&path).unwrap();
        assert_eq!(user_version(&c).unwrap(), 99);
        let tables: i64 = c
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(tables, 1);
        let mode: String = c
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap();
        assert_ne!(mode, "wal");
    }

    #[test]
    fn schema_has_expected_objects() {
        let s = mem();
        let names: HashSet<String> = s
            .query_vec(
                "SELECT name FROM sqlite_master WHERE name NOT LIKE 'sqlite_%'",
                [],
                |r| r.get(0),
            )
            .unwrap()
            .into_iter()
            .collect();
        for n in [
            "meta",
            "spans",
            "frames",
            "frames_fts",
            "errors",
            "commands",
            "insights",
            "sessions",
            "spans_start",
            "spans_end",
            "frames_ts",
            "errors_last_ts",
            "errors_key_last_ts",
            "commands_ts",
            "insights_ts",
            "frames_ai",
            "frames_ad",
            "frames_au",
        ] {
            assert!(names.contains(n), "missing {n}");
        }
    }

    // ---- meta / heartbeat / control ----

    #[test]
    fn meta_round_trip() {
        let s = mem();
        assert_eq!(s.meta_get("a").unwrap(), None);
        s.meta_set("a", "1").unwrap();
        assert_eq!(s.meta_get("a").unwrap().as_deref(), Some("1"));
        s.meta_set("a", "2").unwrap();
        assert_eq!(s.meta_get("a").unwrap().as_deref(), Some("2"));
        s.meta_delete("a").unwrap();
        assert_eq!(s.meta_get("a").unwrap(), None);
        s.meta_delete("a").unwrap();
    }

    #[test]
    fn heartbeat_round_trip_and_corrupt() {
        let s = mem();
        assert_eq!(s.heartbeat().unwrap(), None);
        let hb = Heartbeat {
            ts: 1234.5,
            pid: 42,
            version: "0.1.0".into(),
            platform: "x11".into(),
            ocr: "tesseract".into(),
            can_capture: true,
            idle_since: Some(1200.0),
            paused_until: None,
            current: Some(Current {
                app: "code".into(),
                title: "main.rs".into(),
                tier: Tier::Full,
                since: 1000.0,
            }),
            notes: vec!["note".into()],
        };
        s.set_heartbeat(&hb).unwrap();
        assert_eq!(s.heartbeat().unwrap(), Some(hb));
        s.meta_set("heartbeat", "{not json").unwrap();
        assert_eq!(s.heartbeat().unwrap(), None);
    }

    #[test]
    fn control_flags() {
        let s = mem();
        assert_eq!(s.control().unwrap(), Control::default());
        s.set_stop(true).unwrap();
        s.set_paused_until(Some(1234.5)).unwrap();
        assert_eq!(
            s.control().unwrap(),
            Control {
                stop: true,
                paused_until: Some(1234.5)
            }
        );
        s.set_stop(false).unwrap();
        assert_eq!(s.meta_get("control.stop").unwrap(), None);
        s.set_paused_until(None).unwrap();
        assert_eq!(s.control().unwrap(), Control::default());
        s.meta_set("control.paused_until", "garbage").unwrap();
        assert_eq!(s.control().unwrap().paused_until, None);
        s.meta_set("control.stop", "0").unwrap();
        assert!(!s.control().unwrap().stop);
        s.set_paused_until(Some(1_760_000_000.123456)).unwrap();
        assert_eq!(
            s.control().unwrap().paused_until,
            Some(1_760_000_000.123456)
        );
    }

    // ---- spans ----

    #[test]
    fn span_open_touch_between() {
        let s = mem();
        let id = s.span_open(100.0, "code", "main.rs", Tier::Full).unwrap();
        let sp = s.span_latest().unwrap().unwrap();
        assert_eq!((sp.id, sp.start, sp.end), (id, 100.0, 100.0));
        assert_eq!(sp.tier, Tier::Full);
        s.span_touch(id, 150.0).unwrap();
        s.span_touch(id, 120.0).unwrap(); // never backward
        assert_eq!(s.span_latest().unwrap().unwrap().end, 150.0);

        // [100,150]: both edges inclusive.
        assert_eq!(s.spans_between(150.0, 200.0).unwrap().len(), 1);
        assert_eq!(s.spans_between(50.0, 100.0).unwrap().len(), 1);
        assert_eq!(s.spans_between(151.0, 200.0).unwrap().len(), 0);
        assert_eq!(s.spans_between(50.0, 99.9).unwrap().len(), 0);
        assert_eq!(s.spans_between(110.0, 120.0).unwrap().len(), 1);
        assert_eq!(s.spans_between(0.0, 1000.0).unwrap().len(), 1);
        // touching a missing span is not an error
        s.span_touch(9999, 1.0).unwrap();
    }

    #[test]
    fn spans_ordering_latest_and_delete_short() {
        let s = mem();
        let b = s.span_open(200.0, "b", "tb", Tier::TitleOnly).unwrap();
        let a = s.span_open(100.0, "a", "ta", Tier::Full).unwrap();
        let c = s.span_open(300.0, "c", "", Tier::Hidden).unwrap();
        s.span_touch(a, 190.0).unwrap();
        s.span_touch(b, 201.0).unwrap(); // 1 s long
        s.span_touch(c, 400.0).unwrap();
        let apps: Vec<_> = s
            .spans_between(0.0, 1000.0)
            .unwrap()
            .into_iter()
            .map(|x| x.app)
            .collect();
        assert_eq!(apps, ["a", "b", "c"]);
        assert_eq!(s.span_latest().unwrap().unwrap().id, c);
        assert_eq!(s.span_latest().unwrap().unwrap().tier, Tier::Hidden);
        // b is short and ended before 250; c is long; a is long.
        assert_eq!(s.spans_delete_short(5.0, 250.0).unwrap(), 1);
        assert_eq!(s.spans_between(0.0, 1000.0).unwrap().len(), 2);
        // short but not yet ended before the cutoff: kept
        let d = s.span_open(500.0, "d", "", Tier::Full).unwrap();
        assert_eq!(s.spans_delete_short(5.0, 400.0).unwrap(), 0);
        s.span_delete(d).unwrap();
        assert_eq!(s.spans_between(450.0, 600.0).unwrap().len(), 0);
        assert!(mem().span_latest().unwrap().is_none());
    }

    #[test]
    fn span_privacy_rewrite() {
        let s = mem();
        let id = s
            .span_open(1.0, "bank", "Account 1234", Tier::Full)
            .unwrap();
        s.span_set_privacy(id, "", Tier::Hidden).unwrap();
        let sp = s.span_latest().unwrap().unwrap();
        assert_eq!(sp.title, "");
        assert_eq!(sp.tier, Tier::Hidden);
    }

    // ---- frames ----

    #[test]
    fn frame_insert_get_latest_since() {
        let s = mem();
        assert!(s.frame_latest(0.0).unwrap().is_none());
        let mut f1 = nf(10.0, "a", "t1", "first");
        f1.image = Some("images/d/1.jpg".into());
        f1.reason = "first".into();
        let i1 = s.frame_insert(&f1).unwrap();
        let i2 = s.frame_insert(&nf(20.0, "b", "t2", "second")).unwrap();
        let i3 = s.frame_insert(&nf(30.0, "c", "t3", "third")).unwrap();
        assert!(i1 < i2 && i2 < i3);

        let g = s.frame_get(i1).unwrap().unwrap();
        assert_eq!(g.app, "a");
        assert_eq!(g.image.as_deref(), Some("images/d/1.jpg"));
        assert_eq!(g.reason, "first");
        assert!(s.frame_get(999).unwrap().is_none());

        assert_eq!(s.frame_latest(0.0).unwrap().unwrap().id, i3);
        assert_eq!(s.frame_latest(30.0).unwrap().unwrap().id, i3);
        assert!(s.frame_latest(30.1).unwrap().is_none());
        assert_eq!(s.frame_latest_with_image(0.0).unwrap().unwrap().id, i1);
        assert!(s.frame_latest_with_image(11.0).unwrap().is_none());

        let ids: Vec<_> = s
            .frames_since(15.0, 10)
            .unwrap()
            .into_iter()
            .map(|f| f.id)
            .collect();
        assert_eq!(ids, [i3, i2]);
        assert_eq!(s.frames_since(0.0, 1).unwrap()[0].id, i3);
        assert_eq!(s.frames_since(0.0, 0).unwrap().len(), 0);
    }

    #[test]
    fn frames_with_equal_ts_order_by_id() {
        let s = mem();
        let a = s.frame_insert(&nf(5.0, "a", "", "x")).unwrap();
        let b = s.frame_insert(&nf(5.0, "a", "", "y")).unwrap();
        assert_eq!(s.frame_latest(0.0).unwrap().unwrap().id, b);
        let ids: Vec<_> = s
            .frames_since(0.0, 5)
            .unwrap()
            .into_iter()
            .map(|f| f.id)
            .collect();
        assert_eq!(ids, [b, a]);
    }

    // ---- search ----

    #[test]
    fn search_english_case_insensitive_title_and_since_limit() {
        let s = mem();
        let a = s
            .frame_insert(&nf(10.0, "code", "main.rs", "fn main() { let Value = 1; }"))
            .unwrap();
        let b = s
            .frame_insert(&nf(
                20.0,
                "term",
                "build",
                "error: could not compile `value`",
            ))
            .unwrap();
        let c = s
            .frame_insert(&nf(30.0, "web", "Compile Docs", "nothing relevant"))
            .unwrap();
        assert_eq!(search_ids(&s, "value"), [b, a]);
        assert_eq!(search_ids(&s, "VALUE"), [b, a]);
        assert_eq!(search_ids(&s, "could not compile"), [b]);
        assert_eq!(search_ids(&s, "compile"), [c, b]); // title and text
        assert!(search_ids(&s, "absent-string").is_empty());
        assert_eq!(s.frames_search("value", 15.0, 10).unwrap().len(), 1);
        assert_eq!(s.frames_search("compile", 0.0, 1).unwrap()[0].frame.id, c);
        assert!(s.frames_search("value", 0.0, 0).unwrap().is_empty());
        // all terms must match
        assert_eq!(search_ids(&s, "error value"), [b]);
        assert!(search_ids(&s, "error nothing").is_empty());
        // empty query
        assert!(s.frames_search("   ", 0.0, 10).unwrap().is_empty());
    }

    #[test]
    fn search_cjk_substring() {
        let s = mem();
        let id = s
            .frame_insert(&nf(
                1.0,
                "term",
                "終端機",
                "今天的專案在執行 cargo 時建置失敗了，請檢查相依套件。",
            ))
            .unwrap();
        s.frame_insert(&nf(2.0, "term", "其他", "一切順利，建置成功"))
            .unwrap();
        assert_eq!(search_ids(&s, "建置失敗"), [id]);
        assert_eq!(search_ids(&s, "專案在執行"), [id]);
        assert_eq!(search_ids(&s, "終端機"), [id]); // title
        let hit = &s.frames_search("建置失敗", 0.0, 5).unwrap()[0];
        assert!(hit.snippet.contains("建置失敗"), "{}", hit.snippet);
        // two-char CJK goes through LIKE
        assert_eq!(search_ids(&s, "失敗"), [id]);
        assert_eq!(search_ids(&s, "建置").len(), 2);
    }

    #[test]
    fn search_short_queries_one_and_two_chars() {
        let s = mem();
        let a = s.frame_insert(&nf(1.0, "a", "", "xx ab yy")).unwrap();
        let b = s
            .frame_insert(&nf(2.0, "a", "has AB in title", "plain"))
            .unwrap();
        let c = s.frame_insert(&nf(3.0, "a", "", "nothing here")).unwrap();
        assert_eq!(search_ids(&s, "ab"), [b, a]);
        assert_eq!(search_ids(&s, "AB"), [b, a]);
        assert_eq!(search_ids(&s, "x"), [a]);
        assert_eq!(search_ids(&s, "g"), [c]);
        assert!(search_ids(&s, "q").is_empty());
        // short + long terms mix: FTS for the long one, LIKE for the short one
        assert_eq!(search_ids(&s, "ab yy"), [a]);
        assert_eq!(search_ids(&s, "plain ab"), [b]);
        assert!(search_ids(&s, "nothing ab").is_empty());
    }

    #[test]
    fn search_like_wildcards_are_literal() {
        let s = mem();
        let pct = s
            .frame_insert(&nf(1.0, "a", "", "progress 50% done"))
            .unwrap();
        let und = s
            .frame_insert(&nf(2.0, "a", "", "snake_case name"))
            .unwrap();
        let bs = s.frame_insert(&nf(3.0, "a", "", r"path C:\dir")).unwrap();
        s.frame_insert(&nf(4.0, "a", "", "plain words only"))
            .unwrap();
        assert_eq!(search_ids(&s, "%"), [pct]);
        assert_eq!(search_ids(&s, "0%"), [pct]);
        assert_eq!(search_ids(&s, "_"), [und]);
        assert_eq!(search_ids(&s, "e_"), [und]);
        assert_eq!(search_ids(&s, r"\"), [bs]);
        assert_eq!(search_ids(&s, r"c:\"), [bs]);
        // long terms with wildcard chars go through FTS and are literal there too
        assert_eq!(search_ids(&s, "50%"), [pct]);
        assert_eq!(search_ids(&s, "snake_case"), [und]);
        assert!(search_ids(&s, "sna_e_ase").is_empty());
    }

    #[test]
    fn search_hostile_queries_do_not_error() {
        let s = mem();
        let id = s
            .frame_insert(&nf(
                1.0,
                "a",
                "t",
                r#"say "hello" NEAR foo* -bar col:val AND OR NOT"#,
            ))
            .unwrap();
        for q in [
            r#"""#,
            r#""hello""#,
            r#"say "hello"#,
            "NEAR",
            "NEAR(a b)",
            "foo*",
            "-bar",
            "col:val",
            "AND",
            "OR NOT",
            "(",
            ")))",
            "{a}",
            "^start",
            "'quote'",
            "a AND b OR c",
            "\u{0}x",
            "\"\"\"",
        ] {
            let r = s.frames_search(q, 0.0, 10);
            assert!(r.is_ok(), "query {q:?} failed: {:?}", r.err());
        }
        assert_eq!(search_ids(&s, r#""hello""#), [id]);
        assert_eq!(search_ids(&s, "foo*"), [id]);
        assert_eq!(search_ids(&s, "-bar"), [id]);
        assert_eq!(search_ids(&s, "col:val"), [id]);
        assert_eq!(search_ids(&s, "NEAR foo"), [id]);
    }

    #[test]
    fn search_snippet_is_single_line_and_bounded() {
        let s = mem();
        let long = format!(
            "{}\nneedle   in\ta\r\nhaystack {}",
            "left ".repeat(100),
            "right ".repeat(100)
        );
        s.frame_insert(&nf(1.0, "a", "t", &long)).unwrap();
        let hit = &s.frames_search("NEEDLE", 0.0, 5).unwrap()[0];
        assert!(
            hit.snippet.contains("needle in a haystack"),
            "{}",
            hit.snippet
        );
        assert!(!hit.snippet.contains('\n') && !hit.snippet.contains('\r'));
        assert!(!hit.snippet.contains('\t'));
        assert!(hit.snippet.chars().count() <= 2 * SNIPPET_SIDE + 6 + 2);
        assert!(hit.snippet.starts_with('…') && hit.snippet.ends_with('…'));

        // match at the very start: no leading ellipsis
        s.frame_insert(&nf(2.0, "a", "t", "needle then more"))
            .unwrap();
        let hit = &s.frames_search("needle", 0.0, 5).unwrap()[0];
        assert_eq!(hit.snippet, "needle then more");

        // CJK is cut on char boundaries
        let cjk = format!("{}建置失敗{}", "字".repeat(200), "尾".repeat(200));
        s.frame_insert(&nf(3.0, "a", "t", &cjk)).unwrap();
        let hit = &s.frames_search("建置失敗", 0.0, 1).unwrap()[0];
        assert!(hit.snippet.contains("建置失敗"));
        assert!(hit.snippet.chars().count() <= 2 * SNIPPET_SIDE + 4 + 2);

        // title-only match still produces a snippet
        s.frame_insert(&nf(4.0, "a", "unique-title-xyz", "body"))
            .unwrap();
        let hit = &s.frames_search("unique-title", 0.0, 1).unwrap()[0];
        assert!(hit.snippet.contains("unique-title-xyz"));
    }

    #[test]
    fn deleted_frames_are_not_searchable() {
        let s = mem();
        let id = s
            .frame_insert(&nf(1.0, "a", "title-abc", "searchable-text"))
            .unwrap();
        let keep = s
            .frame_insert(&nf(2.0, "a", "t", "searchable-other"))
            .unwrap();
        assert_eq!(search_ids(&s, "searchable"), [keep, id]);
        s.frame_delete(id).unwrap();
        assert_eq!(search_ids(&s, "searchable"), [keep]);
        assert!(search_ids(&s, "title-abc").is_empty());
        assert!(s.frame_get(id).unwrap().is_none());
        // deleting twice / a missing id is fine
        s.frame_delete(id).unwrap();
        s.frame_delete(12345).unwrap();
        // the index stays consistent
        s.conn
            .execute(
                "INSERT INTO frames_fts(frames_fts) VALUES('integrity-check')",
                [],
            )
            .unwrap();
    }

    #[test]
    fn updated_frame_text_is_reindexed() {
        let s = mem();
        let id = s.frame_insert(&nf(1.0, "a", "t", "before-edit")).unwrap();
        s.conn
            .execute("UPDATE frames SET text = 'after-edit' WHERE id = ?1", [id])
            .unwrap();
        assert!(search_ids(&s, "before-edit").is_empty());
        assert_eq!(search_ids(&s, "after-edit"), [id]);
        // dropping the image (UPDATE of another column) keeps the index intact
        s.frame_drop_image(id).unwrap();
        assert_eq!(search_ids(&s, "after-edit"), [id]);
        s.conn
            .execute(
                "INSERT INTO frames_fts(frames_fts) VALUES('integrity-check')",
                [],
            )
            .unwrap();
    }

    #[test]
    fn fts_query_builds_safe_expressions() {
        assert_eq!(fts_query("hello"), Some("\"hello\"".into()));
        assert_eq!(
            fts_query("foo   bar\tbaz"),
            Some("\"foo\" AND \"bar\" AND \"baz\"".into())
        );
        assert_eq!(
            fts_query(r#"say "hi""#),
            Some("\"say\" AND \"\"\"hi\"\"\"".into())
        );
        assert_eq!(fts_query("NEAR"), Some("\"NEAR\"".into()));
        assert_eq!(fts_query(""), None);
        assert_eq!(fts_query("  "), None);
        assert_eq!(fts_query("ab"), None);
        assert_eq!(fts_query("a b"), None);
        assert_eq!(fts_query("失敗"), None);
        assert_eq!(fts_query("建置失"), Some("\"建置失\"".into()));
        // short terms are dropped from the expression (frames_search filters them with LIKE)
        assert_eq!(fts_query("ab cde"), Some("\"cde\"".into()));
    }

    #[test]
    fn like_pattern_escapes() {
        assert_eq!(like_pattern("a%b_c\\d"), "%a\\%b\\_c\\\\d%");
    }

    // ---- errors ----

    #[test]
    fn error_upsert_lifecycle() {
        let s = mem();
        let (dedupe, window) = (30.0, 600.0);

        // first sighting
        let mut e = ne(1000.0, "k1", 40);
        e.frame_id = Some(1);
        let r1 = s.error_upsert(&e, dedupe, window).unwrap();
        assert!(r1.is_new && r1.bumped);
        assert_eq!(r1.count, 1);

        // same sighting 10 s later, lower score: only last_ts moves
        let mut e2 = ne(1010.0, "k1", 20);
        e2.lines = vec!["different".into()];
        e2.frame_id = Some(2);
        let r2 = s.error_upsert(&e2, dedupe, window).unwrap();
        assert_eq!(r2.id, r1.id);
        assert!(!r2.is_new && !r2.bumped);
        assert_eq!(r2.count, 1);
        let rec = s.error_get(r1.id).unwrap().unwrap();
        assert_eq!(rec.ts, 1000.0);
        assert_eq!(rec.last_ts, 1010.0);
        assert_eq!(rec.score, 40);
        assert_eq!(rec.frame_id, Some(1));
        assert_eq!(rec.lines, ["line for k1"]);

        // same sighting, higher score: lines/frame/score replaced, still no bump
        let mut e3 = ne(1020.0, "k1", 70);
        e3.lines = vec!["better".into()];
        e3.frame_id = Some(3);
        let r3 = s.error_upsert(&e3, dedupe, window).unwrap();
        assert!(!r3.is_new && !r3.bumped);
        let rec = s.error_get(r1.id).unwrap().unwrap();
        assert_eq!((rec.score, rec.frame_id), (70, Some(3)));
        assert_eq!(rec.lines, ["better"]);
        assert_eq!(rec.count, 1);

        // re-appearance 100 s later (> dedupe, < window): count bumps, score keeps the max
        let mut e4 = ne(1120.0, "k1", 10);
        e4.lines = vec!["again".into()];
        e4.frame_id = Some(4);
        e4.cmd = Some("cargo test".into());
        e4.exit_code = Some(101);
        let r4 = s.error_upsert(&e4, dedupe, window).unwrap();
        assert_eq!(r4.id, r1.id);
        assert!(!r4.is_new && r4.bumped);
        assert_eq!(r4.count, 2);
        let rec = s.error_get(r1.id).unwrap().unwrap();
        assert_eq!(rec.count, 2);
        assert_eq!(rec.last_ts, 1120.0);
        assert_eq!(rec.score, 70);
        assert_eq!(rec.lines, ["again"]);
        assert_eq!(rec.frame_id, Some(4));
        assert_eq!(rec.cmd.as_deref(), Some("cargo test"));
        assert_eq!(rec.exit_code, Some(101));

        // outside the repeat window: a second row
        let r5 = s
            .error_upsert(&ne(1120.0 + 601.0, "k1", 50), dedupe, window)
            .unwrap();
        assert!(r5.is_new && r5.bumped);
        assert_ne!(r5.id, r1.id);
        assert_eq!(r5.count, 1);
        assert_eq!(s.errors_since(0.0, 10).unwrap().len(), 2);

        // a different key never merges
        let r6 = s
            .error_upsert(&ne(1120.0 + 601.0, "k2", 50), dedupe, window)
            .unwrap();
        assert!(r6.is_new);

        // a re-appearance merges into the NEWEST row of the key
        let r7 = s
            .error_upsert(&ne(1721.0 + 100.0, "k1", 5), dedupe, window)
            .unwrap();
        assert_eq!(r7.id, r5.id);
        assert_eq!(r7.count, 2);
    }

    #[test]
    fn error_upsert_out_of_order_does_not_move_last_ts_back() {
        let s = mem();
        let r = s.error_upsert(&ne(1000.0, "k", 10), 30.0, 600.0).unwrap();
        let r2 = s.error_upsert(&ne(990.0, "k", 10), 30.0, 600.0).unwrap();
        assert_eq!(r2.id, r.id);
        assert!(!r2.bumped);
        assert_eq!(s.error_get(r.id).unwrap().unwrap().last_ts, 1000.0);
    }

    #[test]
    fn errors_since_ordering_and_fields() {
        let s = mem();
        let mut shell = ne(100.0, "low-old", 10);
        shell.source = ErrorSource::Shell;
        shell.cmd = Some("make".into());
        shell.exit_code = Some(2);
        let a = s.error_upsert(&shell, 5.0, 60.0).unwrap().id;
        let b = s
            .error_upsert(&ne(200.0, "low-new", 10), 5.0, 60.0)
            .unwrap()
            .id;
        let c = s
            .error_upsert(&ne(50.0, "high-oldest", 90), 5.0, 60.0)
            .unwrap()
            .id;
        let ids: Vec<_> = s
            .errors_since(0.0, 10)
            .unwrap()
            .into_iter()
            .map(|e| e.id)
            .collect();
        assert_eq!(ids, [c, b, a]);
        let ids: Vec<_> = s
            .errors_since(100.0, 10)
            .unwrap()
            .into_iter()
            .map(|e| e.id)
            .collect();
        assert_eq!(ids, [b, a]);
        assert_eq!(s.errors_since(0.0, 2).unwrap().len(), 2);
        let rec = s.error_get(a).unwrap().unwrap();
        assert_eq!(rec.source, ErrorSource::Shell);
        assert_eq!(rec.exit_code, Some(2));
        assert_eq!(rec.cmd.as_deref(), Some("make"));
        s.error_delete(a).unwrap();
        assert!(s.error_get(a).unwrap().is_none());
        s.error_delete(a).unwrap();
    }

    #[test]
    fn corrupt_error_lines_read_as_empty() {
        let s = mem();
        let id = s.error_upsert(&ne(1.0, "k", 1), 1.0, 1.0).unwrap().id;
        s.conn
            .execute("UPDATE errors SET lines = '{broken' WHERE id = ?1", [id])
            .unwrap();
        assert!(s.error_get(id).unwrap().unwrap().lines.is_empty());
        assert_eq!(s.errors_since(0.0, 5).unwrap().len(), 1);
        assert_eq!(s.errors_page(0, 5).unwrap().len(), 1);
    }

    // ---- commands ----

    #[test]
    fn commands_insert_and_since() {
        let s = mem();
        let a = s.command_insert(&nc(10.0, "ls")).unwrap();
        let mut failing = nc(20.0, "cargo test");
        failing.exit_code = Some(101);
        failing.duration_ms = None;
        let b = s.command_insert(&failing).unwrap();
        let mut unknown = nc(30.0, "sleep");
        unknown.exit_code = None;
        let c = s.command_insert(&unknown).unwrap();
        let all = s.commands_since(0.0, 10).unwrap();
        assert_eq!(all.iter().map(|c| c.id).collect::<Vec<_>>(), [c, b, a]);
        assert_eq!(all[1].exit_code, Some(101));
        assert_eq!(all[1].duration_ms, None);
        assert_eq!(all[2].shell, "bash");
        assert_eq!(all[0].exit_code, None);
        assert_eq!(s.commands_since(15.0, 10).unwrap().len(), 2);
        assert_eq!(s.commands_since(0.0, 1).unwrap()[0].id, c);
        s.command_delete(b).unwrap();
        assert_eq!(s.commands_since(0.0, 10).unwrap().len(), 2);
    }

    // ---- insights ----

    #[test]
    fn insight_round_trip() {
        let s = mem();
        assert!(s.insight_latest(0.0).unwrap().is_none());
        let ins = Insight {
            id: 777, // ignored
            ts: 100.0,
            trigger: "idle".into(),
            model: "qwen".into(),
            doing: "writing tests".into(),
            state: "ok".into(),
            suggestions: vec!["a".into(), "b".into()],
            notable: "n".into(),
        };
        let id = s.insight_insert(&ins).unwrap();
        assert_ne!(id, 777);
        let got = s.insight_latest(0.0).unwrap().unwrap();
        assert_eq!(got, Insight { id, ..ins.clone() });
        let id2 = s
            .insight_insert(&Insight {
                ts: 200.0,
                doing: "newer".into(),
                ..Insight::default()
            })
            .unwrap();
        assert_eq!(s.insight_latest(0.0).unwrap().unwrap().id, id2);
        assert_eq!(s.insight_latest(150.0).unwrap().unwrap().id, id2);
        assert!(s.insight_latest(250.0).unwrap().is_none());
        let all = s.insights_since(0.0, 10).unwrap();
        assert_eq!(all.iter().map(|i| i.id).collect::<Vec<_>>(), [id2, id]);
        assert_eq!(s.insights_since(0.0, 1).unwrap().len(), 1);
        assert_eq!(s.insights_count_since(0.0).unwrap(), 2);
        assert_eq!(s.insights_count_since(150.0).unwrap(), 1);
        assert_eq!(s.insights_count_since(300.0).unwrap(), 0);
        // damaged blob: the row survives with its columns
        s.conn
            .execute("UPDATE insights SET json = 'oops' WHERE id = ?1", [id])
            .unwrap();
        let got = s.insights_since(0.0, 10).unwrap();
        let old = got.iter().find(|i| i.id == id).unwrap();
        assert_eq!(
            (old.ts, old.trigger.as_str(), old.model.as_str()),
            (100.0, "idle", "qwen")
        );
    }

    // ---- sessions ----

    #[test]
    fn sessions_upsert_and_prune() {
        let s = mem();
        assert!(s.session_get("x").unwrap().is_none());
        let mut sess = Session {
            id: "x".into(),
            last_ts: 10.0,
            full_ts: 5.0,
        };
        s.session_set(&sess).unwrap();
        assert_eq!(s.session_get("x").unwrap(), Some(sess.clone()));
        sess.last_ts = 20.0;
        sess.full_ts = 20.0;
        s.session_set(&sess).unwrap();
        assert_eq!(s.session_get("x").unwrap(), Some(sess));
        s.session_set(&Session {
            id: "y".into(),
            last_ts: 100.0,
            full_ts: 100.0,
        })
        .unwrap();
        assert_eq!(s.sessions_prune(50.0).unwrap(), 1);
        assert!(s.session_get("x").unwrap().is_none());
        assert!(s.session_get("y").unwrap().is_some());
        assert_eq!(s.sessions_prune(50.0).unwrap(), 0);
    }

    // ---- paging ----

    #[test]
    fn paging_is_increasing_and_terminates() {
        let s = mem();
        for i in 0..25 {
            let t = f64::from(i);
            s.span_open(t, "a", "t", Tier::Full).unwrap();
            s.frame_insert(&nf(t, "a", "t", "text")).unwrap();
            s.error_upsert(&ne(t, &format!("k{i}"), 1), 1.0, 1.0)
                .unwrap();
            s.command_insert(&nc(t, "ls")).unwrap();
        }
        fn drain<T>(mut next: impl FnMut(i64) -> Vec<T>, id: impl Fn(&T) -> i64) -> Vec<i64> {
            let mut seen = Vec::new();
            let mut after = 0;
            let mut guard = 0;
            loop {
                let page = next(after);
                if page.is_empty() {
                    break;
                }
                assert!(page.len() <= 10);
                for r in &page {
                    assert!(id(r) > after);
                    after = id(r);
                    seen.push(after);
                }
                guard += 1;
                assert!(guard < 10, "paging did not terminate");
            }
            seen
        }
        let spans = drain(|a| s.spans_page(a, 10).unwrap(), |r| r.id);
        let frames = drain(|a| s.frames_page(a, 10).unwrap(), |r| r.id);
        let errors = drain(|a| s.errors_page(a, 10).unwrap(), |r| r.id);
        let cmds = drain(|a| s.commands_page(a, 10).unwrap(), |r| r.id);
        for v in [spans, frames, errors, cmds] {
            assert_eq!(v.len(), 25);
            assert!(v.windows(2).all(|w| w[0] < w[1]));
        }
        assert!(s.frames_page(i64::MAX, 10).unwrap().is_empty());
    }

    // ---- images ----

    #[test]
    fn safe_rel_rules() {
        for ok in [
            "images/2026-10-10/135803_114.jpg",
            "a.jpg",
            "./a/b.jpg",
            "a/./b",
        ] {
            assert!(is_safe_rel(ok), "{ok}");
        }
        for bad in [
            "",
            "../x",
            "a/../../x",
            "a/..",
            "..",
            "/etc/passwd",
            "\\windows\\x",
            "C:\\x",
            "c:/x",
            "a\\..\\b",
            "a\0b",
        ] {
            assert!(!is_safe_rel(bad), "{bad:?}");
        }
    }

    #[test]
    fn frame_delete_removes_image_file() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path()).unwrap();
        write_file(dir.path(), "images/2026-10-10/a.jpg", 10);
        write_file(dir.path(), "images/2026-10-10/b.jpg", 10);
        let a = s
            .frame_insert(&frame_with_image(1.0, "x", "y", "images/2026-10-10/a.jpg"))
            .unwrap();
        let b = s
            .frame_insert(&frame_with_image(2.0, "x", "y", "images/2026-10-10/b.jpg"))
            .unwrap();
        s.frame_delete(a).unwrap();
        assert!(!dir.path().join("images/2026-10-10/a.jpg").exists());
        assert!(dir.path().join("images/2026-10-10/b.jpg").exists());
        // dropping only the image keeps the text
        s.frame_drop_image(b).unwrap();
        assert!(!dir.path().join("images/2026-10-10/b.jpg").exists());
        let f = s.frame_get(b).unwrap().unwrap();
        assert_eq!(f.image, None);
        assert_eq!(f.text, "some text here");
        assert_eq!(search_ids(&s, "some text"), [b]);
        // a missing file is not an error
        let c = s
            .frame_insert(&frame_with_image(
                3.0,
                "x",
                "y",
                "images/2026-10-10/gone.jpg",
            ))
            .unwrap();
        s.frame_delete(c).unwrap();
        s.frame_drop_image(c).unwrap();
    }

    #[test]
    fn path_traversal_is_not_deleted() {
        let root = tempfile::tempdir().unwrap();
        let data = root.path().join("data");
        let s = Store::open(&data).unwrap();
        let outside = root.path().join("outside.txt");
        fs::write(&outside, "precious").unwrap();
        let abs = outside.to_string_lossy().into_owned();
        let f1 = s
            .frame_insert(&frame_with_image(1.0, "x", "y", "../outside.txt"))
            .unwrap();
        let f2 = s
            .frame_insert(&frame_with_image(2.0, "x", "y", &abs))
            .unwrap();
        let f3 = s
            .frame_insert(&frame_with_image(3.0, "x", "y", "images/../../outside.txt"))
            .unwrap();
        assert!(s.frame_delete(f1).is_err());
        assert!(s.frame_drop_image(f2).is_err());
        assert!(outside.exists());
        assert!(s.frame_delete(f2).is_err());
        assert!(s.frame_delete(f3).is_err());
        // forget_window and sweep (age + frames) are guarded too
        s.frame_insert(&frame_with_image(4.0, "w", "t", "../outside.txt"))
            .unwrap();
        s.frame_insert(&frame_with_image(5.0, "w", "t", &abs))
            .unwrap();
        assert!(s.forget_window("w", "t").is_err());
        s.frame_insert(&frame_with_image(6.0, "w", "t", "../outside.txt"))
            .unwrap();
        s.frame_insert(&frame_with_image(7.0, "w", "t", &abs))
            .unwrap();
        assert!(s.sweep(1e9, &RetentionConfig::default()).is_err());
        assert!(outside.exists());
        assert_eq!(fs::read_to_string(&outside).unwrap(), "precious");
    }

    #[test]
    fn in_memory_image_ops_are_noops() {
        let s = mem();
        let id = s
            .frame_insert(&frame_with_image(1.0, "a", "b", "images/x.jpg"))
            .unwrap();
        s.frame_drop_image(id).unwrap();
        assert_eq!(s.frame_get(id).unwrap().unwrap().image, None);
        assert!(s.data_dir().is_none());
    }

    #[test]
    fn forget_window_is_exact() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path()).unwrap();
        for n in ["a", "b", "c"] {
            write_file(dir.path(), &format!("images/d/{n}.jpg"), 5);
        }
        s.frame_insert(&frame_with_image(1.0, "bank", "Login", "images/d/a.jpg"))
            .unwrap();
        s.frame_insert(&nf(2.0, "bank", "Login", "no image"))
            .unwrap();
        let other_title = s
            .frame_insert(&frame_with_image(3.0, "bank", "Home", "images/d/b.jpg"))
            .unwrap();
        let other_app = s
            .frame_insert(&frame_with_image(4.0, "mail", "Login", "images/d/c.jpg"))
            .unwrap();
        let mut e = ne(1.0, "ek", 5);
        e.app = "bank".into();
        e.title = "Login".into();
        s.error_upsert(&e, 1.0, 10.0).unwrap();
        let mut e = ne(1.0, "ek2", 5);
        e.app = "bank".into();
        e.title = "Home".into();
        let keep_err = s.error_upsert(&e, 1.0, 10.0).unwrap().id;

        assert_eq!(s.forget_window("bank", "Login").unwrap(), 3);
        assert!(!dir.path().join("images/d/a.jpg").exists());
        assert!(dir.path().join("images/d/b.jpg").exists());
        assert!(dir.path().join("images/d/c.jpg").exists());
        let left: Vec<_> = s
            .frames_page(0, 10)
            .unwrap()
            .into_iter()
            .map(|f| f.id)
            .collect();
        assert_eq!(left, [other_title, other_app]);
        assert_eq!(s.errors_page(0, 10).unwrap().len(), 1);
        assert_eq!(s.errors_page(0, 10).unwrap()[0].id, keep_err);
        assert!(search_ids(&s, "no image").is_empty());
        assert_eq!(s.forget_window("bank", "Login").unwrap(), 0);
    }

    // ---- retention ----

    fn cfg() -> RetentionConfig {
        RetentionConfig {
            images_hours: 6.0,
            max_images_mb: 300,
            frames_days: 7.0,
            timeline_days: 30.0,
            insights_days: 90.0,
        }
    }

    const H: f64 = 3600.0;
    const D: f64 = 86_400.0;

    #[test]
    fn sweep_images_by_age() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path()).unwrap();
        let now = crate::now();
        write_file(dir.path(), "images/d1/old.jpg", 100);
        write_file(dir.path(), "images/d2/new.jpg", 100);
        let old = s
            .frame_insert(&frame_with_image(
                now - 7.0 * H,
                "a",
                "t",
                "images/d1/old.jpg",
            ))
            .unwrap();
        let new = s
            .frame_insert(&frame_with_image(
                now - 1.0 * H,
                "a",
                "t",
                "images/d2/new.jpg",
            ))
            .unwrap();
        let st = s.sweep(now, &cfg()).unwrap();
        assert_eq!(st.images_deleted, 1);
        assert_eq!(st.image_bytes_freed, 100);
        assert_eq!(st.frames_deleted, 0);
        assert!(!dir.path().join("images/d1/old.jpg").exists());
        assert!(dir.path().join("images/d2/new.jpg").exists());
        // text survives, searchable; empty day dir is gone
        let f = s.frame_get(old).unwrap().unwrap();
        assert_eq!(f.image, None);
        assert_eq!(search_ids(&s, "some text").len(), 2);
        assert!(!dir.path().join("images/d1").exists());
        assert!(dir.path().join("images/d2").exists());
        assert!(s.frame_get(new).unwrap().unwrap().image.is_some());
        // idempotent
        assert_eq!(s.sweep(now, &cfg()).unwrap(), SweepStats::default());
    }

    #[test]
    fn sweep_image_size_cap_oldest_first() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path()).unwrap();
        let now = crate::now();
        let kb400 = 400 * 1024;
        // inserted out of order on purpose
        for (n, age) in [("c", 10.0), ("a", 3000.0), ("b", 2000.0)] {
            write_file(dir.path(), &format!("images/d/{n}.jpg"), kb400);
            s.frame_insert(&frame_with_image(
                now - age,
                "x",
                "y",
                &format!("images/d/{n}.jpg"),
            ))
            .unwrap();
        }
        let mut c = cfg();
        c.max_images_mb = 1; // 3 x 400 KB = 1.17 MB > 1 MB: drop only the oldest
        let st = s.sweep(now, &c).unwrap();
        assert_eq!(st.images_deleted, 1);
        assert_eq!(st.image_bytes_freed, kb400 as u64);
        assert!(!dir.path().join("images/d/a.jpg").exists());
        assert!(dir.path().join("images/d/b.jpg").exists());
        assert!(dir.path().join("images/d/c.jpg").exists());
        c.max_images_mb = 0;
        let st = s.sweep(now, &c).unwrap();
        assert_eq!(st.images_deleted, 2);
        assert!(s
            .frames_page(0, 10)
            .unwrap()
            .iter()
            .all(|f| f.image.is_none()));
        assert_eq!(s.frames_page(0, 10).unwrap().len(), 3);
    }

    #[test]
    fn sweep_drops_dangling_image_refs() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path()).unwrap();
        let now = crate::now();
        let id = s
            .frame_insert(&frame_with_image(now, "a", "t", "images/d/missing.jpg"))
            .unwrap();
        s.sweep(now, &cfg()).unwrap();
        assert_eq!(s.frame_get(id).unwrap().unwrap().image, None);
    }

    #[test]
    fn sweep_deletes_each_table_by_its_age() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path()).unwrap();
        let now = crate::now();

        // frames: 7-day rule
        write_file(dir.path(), "images/d/f.jpg", 7);
        s.frame_insert(&frame_with_image(now - 8.0 * D, "a", "t", "images/d/f.jpg"))
            .unwrap();
        s.frame_insert(&nf(now - 6.0 * D, "a", "t", "recent frame"))
            .unwrap();
        // spans / commands / errors: 30-day rule
        let old_span = s.span_open(now - 40.0 * D, "a", "t", Tier::Full).unwrap();
        s.span_touch(old_span, now - 39.0 * D).unwrap();
        let straddle = s.span_open(now - 40.0 * D, "a", "t", Tier::Full).unwrap();
        s.span_touch(straddle, now - 10.0 * D).unwrap(); // ended recently: kept
        s.span_open(now - 1.0 * D, "a", "t", Tier::Full).unwrap();
        s.command_insert(&nc(now - 31.0 * D, "old")).unwrap();
        s.command_insert(&nc(now - 29.0 * D, "new")).unwrap();
        s.error_upsert(&ne(now - 35.0 * D, "old", 1), 1.0, 10.0)
            .unwrap();
        s.error_upsert(&ne(now - 5.0 * D, "new", 1), 1.0, 10.0)
            .unwrap();
        // an error first seen long ago but seen again recently stays (last_ts rule)
        let id = s
            .error_upsert(&ne(now - 35.0 * D, "recurring", 1), 1.0, 10.0)
            .unwrap()
            .id;
        s.conn
            .execute(
                "UPDATE errors SET last_ts = ?2 WHERE id = ?1",
                params![id, now - D],
            )
            .unwrap();
        // insights: 90-day rule
        s.insight_insert(&Insight {
            ts: now - 91.0 * D,
            ..Insight::default()
        })
        .unwrap();
        s.insight_insert(&Insight {
            ts: now - 89.0 * D,
            ..Insight::default()
        })
        .unwrap();
        // sessions: idle > 2 days
        s.session_set(&Session {
            id: "old".into(),
            last_ts: now - 3.0 * D,
            full_ts: 0.0,
        })
        .unwrap();
        s.session_set(&Session {
            id: "new".into(),
            last_ts: now - 1.0 * D,
            full_ts: 0.0,
        })
        .unwrap();

        let st = s.sweep(now, &cfg()).unwrap();
        assert_eq!(
            st,
            SweepStats {
                images_deleted: 1,
                image_bytes_freed: 7,
                frames_deleted: 1,
                spans_deleted: 1,
                commands_deleted: 1,
                errors_deleted: 1,
                insights_deleted: 1,
            }
        );
        assert!(!dir.path().join("images/d/f.jpg").exists());
        assert_eq!(s.frames_page(0, 10).unwrap().len(), 1);
        assert_eq!(s.spans_page(0, 10).unwrap().len(), 2);
        assert_eq!(s.commands_page(0, 10).unwrap().len(), 1);
        assert_eq!(s.errors_page(0, 10).unwrap().len(), 2);
        assert_eq!(s.insights_since(0.0, 10).unwrap().len(), 1);
        assert!(s.session_get("old").unwrap().is_none());
        assert!(s.session_get("new").unwrap().is_some());
        // FTS rows of the deleted frame are gone
        assert_eq!(search_ids(&s, "recent frame").len(), 1);
        assert!(search_ids(&s, "some text here").is_empty());
        s.conn
            .execute(
                "INSERT INTO frames_fts(frames_fts) VALUES('integrity-check')",
                [],
            )
            .unwrap();
    }

    #[test]
    fn sweep_frames_days_shorter_than_images_hours_still_removes_files() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path()).unwrap();
        let now = crate::now();
        write_file(dir.path(), "images/d/f.jpg", 9);
        s.frame_insert(&frame_with_image(now - 2.0 * H, "a", "t", "images/d/f.jpg"))
            .unwrap();
        let mut c = cfg();
        c.frames_days = 1.0 / 24.0; // 1 hour
        let st = s.sweep(now, &c).unwrap();
        assert_eq!(st.frames_deleted, 1);
        assert_eq!(st.images_deleted, 1);
        assert_eq!(st.image_bytes_freed, 9);
        assert!(!dir.path().join("images/d/f.jpg").exists());
    }

    #[test]
    fn sweep_orphans_and_empty_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path()).unwrap();
        let now = crate::now();
        // referenced, old mtime -> kept
        write_file(dir.path(), "images/d/kept.jpg", 10);
        set_age(dir.path(), "images/d/kept.jpg", 3600.0);
        s.frame_insert(&frame_with_image(now, "a", "t", "images/d/kept.jpg"))
            .unwrap();
        // orphan older than 10 minutes -> removed
        write_file(dir.path(), "images/d/orphan_old.jpg", 25);
        set_age(dir.path(), "images/d/orphan_old.jpg", 11.0 * 60.0);
        // fresh orphan -> kept (writer may be mid-insert)
        write_file(dir.path(), "images/d/orphan_new.jpg", 25);
        // orphan whose day dir becomes empty -> dir removed too
        write_file(dir.path(), "images/empty-after/o.jpg", 3);
        set_age(dir.path(), "images/empty-after/o.jpg", 3600.0);
        // already empty day dir
        fs::create_dir_all(dir.path().join("images/empty-now")).unwrap();
        // a file outside images/ is never touched
        write_file(dir.path(), "notes.txt", 5);
        set_age(dir.path(), "notes.txt", 7200.0);

        let st = s.sweep(now, &cfg()).unwrap();
        assert_eq!(st.images_deleted, 2);
        assert_eq!(st.image_bytes_freed, 28);
        assert!(dir.path().join("images/d/kept.jpg").exists());
        assert!(!dir.path().join("images/d/orphan_old.jpg").exists());
        assert!(dir.path().join("images/d/orphan_new.jpg").exists());
        assert!(!dir.path().join("images/empty-after").exists());
        assert!(!dir.path().join("images/empty-now").exists());
        assert!(dir.path().join("images").exists());
        assert!(dir.path().join("notes.txt").exists());
        // passing a later `now` makes the fresh orphan old enough
        let st = s.sweep(now + 20.0 * 60.0, &cfg()).unwrap();
        assert_eq!(st.images_deleted, 1);
        assert!(!dir.path().join("images/d/orphan_new.jpg").exists());
    }

    #[test]
    fn sweep_works_in_memory() {
        let s = mem();
        let now = 10_000_000.0;
        s.frame_insert(&frame_with_image(now - 100.0 * D, "a", "t", "images/x.jpg"))
            .unwrap();
        s.frame_insert(&frame_with_image(now - 7.0 * H, "a", "t", "images/y.jpg"))
            .unwrap();
        let st = s.sweep(now, &cfg()).unwrap();
        assert_eq!(st.frames_deleted, 1);
        assert_eq!(st.image_bytes_freed, 0);
        assert_eq!(s.frames_page(0, 10).unwrap().len(), 1);
        assert_eq!(s.frames_page(0, 10).unwrap()[0].image, None);
    }

    #[test]
    fn wipe_removes_history_but_preserves_configuration_and_control() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path()).unwrap();
        write_file(dir.path(), "images/d/a.jpg", 5);
        let id = s
            .frame_insert(&frame_with_image(1.0, "a", "t", "images/d/a.jpg"))
            .unwrap();
        s.span_open(1.0, "a", "t", Tier::Full).unwrap();
        s.error_upsert(&ne(1.0, "k", 1), 1.0, 1.0).unwrap();
        s.command_insert(&nc(1.0, "ls")).unwrap();
        s.insight_insert(&Insight {
            ts: 1.0,
            ..Insight::default()
        })
        .unwrap();
        s.session_set(&Session {
            id: "s".into(),
            last_ts: 1.0,
            full_ts: 1.0,
        })
        .unwrap();
        s.meta_set("config.rules_hash", "abc").unwrap();
        s.meta_set("config.x", "y").unwrap();
        s.meta_set("heartbeat", "{}").unwrap();
        s.set_stop(true).unwrap();
        write_file(dir.path(), "images/d/orphan.jpg", 5);

        s.wipe().unwrap();

        assert!(s.frame_get(id).unwrap().is_none());
        assert!(s.frames_page(0, 10).unwrap().is_empty());
        assert!(s.spans_page(0, 10).unwrap().is_empty());
        assert!(s.errors_page(0, 10).unwrap().is_empty());
        assert!(s.commands_page(0, 10).unwrap().is_empty());
        assert_eq!(s.insights_count_since(0.0).unwrap(), 0);
        assert!(s.session_get("s").unwrap().is_none());
        assert!(search_ids(&s, "some text").is_empty());
        assert_eq!(
            s.meta_get("config.rules_hash").unwrap().as_deref(),
            Some("abc")
        );
        assert_eq!(s.meta_get("config.x").unwrap().as_deref(), Some("y"));
        assert_eq!(s.meta_get("heartbeat").unwrap(), None);
        assert!(s.control().unwrap().stop);
        assert!(!dir.path().join("images/d").exists());
        // still usable afterwards
        let n = s.frame_insert(&nf(2.0, "a", "t", "fresh start")).unwrap();
        assert_eq!(search_ids(&s, "fresh start"), [n]);
        mem().wipe().unwrap();
    }

    // ---- concurrency ----

    #[test]
    fn two_handles_can_write_concurrently() {
        let dir = tempfile::tempdir().unwrap();
        let a = Store::open(dir.path()).unwrap();
        let b = Store::open(dir.path()).unwrap();
        const N: usize = 200;
        let t = std::thread::spawn(move || {
            for i in 0..N {
                b.command_insert(&nc(i as f64, &format!("cmd {i}")))
                    .unwrap();
                if i % 20 == 0 {
                    b.meta_set("from_b", &i.to_string()).unwrap();
                }
            }
            b
        });
        for i in 0..N {
            a.frame_insert(&nf(i as f64, "app", "title", &format!("frame number {i}")))
                .unwrap();
            if i % 20 == 0 {
                a.error_upsert(&ne(i as f64, &format!("k{i}"), 1), 1.0, 1.0)
                    .unwrap();
            }
        }
        let b = t.join().unwrap();
        for s in [&a, &b] {
            assert_eq!(s.frames_since(0.0, 1000).unwrap().len(), N);
            assert_eq!(s.commands_since(0.0, 1000).unwrap().len(), N);
            assert_eq!(s.errors_since(0.0, 1000).unwrap().len(), N / 20);
            assert_eq!(search_ids(s, "frame number 199").len(), 1);
        }
        assert_eq!(a.meta_get("from_b").unwrap().as_deref(), Some("180"));
        assert_eq!(
            a.frames_since(0.0, 1000).unwrap().len() + b.commands_since(0.0, 1000).unwrap().len(),
            2 * N
        );
    }

    #[test]
    fn concurrent_open_of_a_fresh_directory() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_path_buf();
        let handles: Vec<_> = (0..4)
            .map(|i| {
                let p = path.clone();
                std::thread::spawn(move || {
                    let s = Store::open(&p).unwrap();
                    s.meta_set(&format!("k{i}"), "v").unwrap();
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        let s = Store::open(&path).unwrap();
        for i in 0..4 {
            assert!(s.meta_get(&format!("k{i}")).unwrap().is_some());
        }
    }
}
