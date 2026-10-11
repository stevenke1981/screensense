//! Error detection: find error lines in on-screen (OCR) text, build dedupe keys
//! and score what was found.
//!
//! Everything here is pure: no I/O, no clock. Patterns are compiled once into
//! [`RegexSet`]s. Two tiers exist:
//!
//! * STRONG patterns are specific enough to trust anywhere, including in an
//!   editor where source code is full of the word "Error".
//! * WEAK patterns (`ERROR ...`, `[ERROR]`, HTTP 5xx, localised "failed"
//!   phrases) are only used for terminals, browsers and other apps.
//!
//! A small NEGATIVE set removes success summaries (`0 failed`, `error: 0`) and
//! quoted error-code constants (`'ENOENT'`) before either tier is consulted.

use std::collections::HashSet;
use std::sync::LazyLock;

use regex::{Regex, RegexSet};

use crate::types::{AppClass, ErrorSource};

/// Most lines returned by [`find_errors`].
const MAX_LINES: usize = 8;
/// Longest line returned, in characters.
const MAX_LINE_CHARS: usize = 240;
/// Only this many characters of a line are examined by the patterns.
const MATCH_CHARS: usize = 1000;
/// Longest [`line_key`] / [`shell_error_key`], in characters.
const KEY_CHARS: usize = 120;

const STRONG_PATTERNS: &[&str] = &[
    // Python
    r"Traceback \(most recent call last\)",
    r"^[\w.]*(?:Error|Exception): .{3,}",
    // Rust
    // Tolerates OCR misreads of the brackets and of zero: `errorLE0308]:`, `error[EO308]:`.
    r"^error(?:.?E[0-9O]{4}.?)?: ",
    r"^thread '.*' panicked at",
    r"^panicked at",
    r"^test .+ \.\.\. FAILED\b",
    // Go
    r"^panic: ",
    r"^fatal error: ",
    // C / C++ / linker
    r": (?:fatal )?error: ",
    r"undefined reference to",
    r"Segmentation fault",
    r"core dumped",
    // TypeScript
    r"(?i)error TS\d{3,5}",
    r":\d+:\d+ - error",
    r"Found [1-9]\d* errors?",
    r"^Type error: ",
    // JS / Node
    r"npm ERR!",
    r"^npm error\b",
    r"^Uncaught \w*Error",
    r"Unhandled ?(?:Promise ?)?Rejection",
    r"Unhandled Runtime Error",
    r"\bE(?:ADDRINUSE|NOENT|CONNREFUSED|ACCES|PERM|TIMEDOUT|CONNRESET)\b",
    // Builds
    r"Failed to compile",
    r"Build (?:failed|FAILED)",
    r"Build error occurred",
    r"Compilation failed",
    r"BUILD FAILED",
    r"編譯失敗|建置失敗|编译失败|构建失败",
    // Process exit
    r"exit(?:ed)? with (?:exit )?code [1-9]",
    r"^Exit code:? [1-9]",
    r"exit status [1-9]",
    // git
    r"^fatal: ",
    // PowerShell / cmd
    r"(?i)is not recognized as (?:the name of )?an? (?:internal or external command|cmdlet)",
    r"無法辨識.*(?:命令|[Cc]mdlet|指令|詞彙)|不是內部或外部命令|不是内部或外部命令|无法将.*识别为",
    r"CommandNotFoundException",
    r"ParserError",
    // POSIX shells
    r"command not found",
    r"^-?[a-z][a-z0-9_.+-]*: (?:.*: )?(?i:no such file or directory|permission denied)",
    // Java / Kotlin
    r"^Exception in thread",
    r"^Caused by: ",
    // .NET
    r"^Unhandled [Ee]xception",
    r"(?i)error CS\d{4}",
    r"(?i)error MSB\d{4}",
    // Docker / Kubernetes
    r"Error response from daemon",
    r"CrashLoopBackOff",
    r"ImagePullBackOff",
    r"ErrImagePull",
    // Test runners
    r"^FAILED \S+",
    r"^--- FAIL: ",
    r"\b[1-9]\d* (?:tests? )?(?:failed|failing)\b",
    r"test result: FAILED",
];

const WEAK_PATTERNS: &[&str] = &[
    r"^(?:FAIL|FAILED|ERROR|FATAL|CRITICAL)\b",
    r"^\[?[\d:.,TZ -]{8,}\]?\s+(?:ERROR|FATAL|CRITICAL)\b",
    r"\[ERROR\]",
    r"\bERR!",
    r"Internal Server Error",
    r"\b50[0234] (?:Internal|Bad Gateway|Service Unavailable|Gateway Time)",
    r"Application error: a (?:client|server)-side exception",
    r"Access is denied|拒絕存取|拒绝访问",
    r"發生錯誤|執行失敗|無法連線|連線逾時|发生错误|执行失败|无法连接|连接超时",
    // Not the plain multiplication sign: browser tabs have close buttons.
    r"^[✖❌✗]\s*\S.{8,}",
];

const NEGATIVE_PATTERNS: &[&str] = &[
    // "error: 0", "errors = 0", "failed: 0"
    r"(?i)^\W*(?:errors?|failed|failures?|failing)\s*[:=]\s*(?:0+|none)\b",
    // "0 errors", "0 failed", "no errors found"
    r"(?i)^\W*(?:0|no|zero) +(?:errors?|failed|failures?|failing)\b",
    // cargo / pytest style success summaries
    r"(?i)test result: ok\b",
    // quoted constants in source: `code === 'ENOENT'`
    r#"['"`]E(?:ADDRINUSE|NOENT|CONNREFUSED|ACCES|PERM|TIMEDOUT|CONNRESET)['"`]"#,
];

static STRONG: LazyLock<RegexSet> =
    LazyLock::new(|| RegexSet::new(STRONG_PATTERNS).expect("built-in STRONG patterns compile"));
static WEAK: LazyLock<RegexSet> =
    LazyLock::new(|| RegexSet::new(WEAK_PATTERNS).expect("built-in WEAK patterns compile"));
static NEGATIVE: LazyLock<RegexSet> =
    LazyLock::new(|| RegexSet::new(NEGATIVE_PATTERNS).expect("built-in NEGATIVE patterns compile"));

/// An absolute path (`/a/b/`, `c:\a\b\`, `~/a/`) at a token start; group 1 is the boundary.
static RE_PATH: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(^|[\s'"`(\[=<])(?:[a-z]:[\\/]|~[\\/]|[\\/]{1,2})(?:[^\s/\\'"`()<>\[\]]+[\\/])+"#)
        .expect("built-in path regex compiles")
});
/// `2026-10-10t13:58` -> the `t` is a separator, not an identifier prefix.
static RE_ISO_T: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(\d{2})t(\d{2}:)").expect("built-in ISO regex compiles"));
static RE_HEX_0X: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"0x[0-9a-f]+").expect("built-in 0x regex compiles"));
static RE_HEX_RUN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b[0-9a-f]{8,}\b").expect("built-in hex regex compiles"));

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Strong,
    Weak,
}

/// First `max` characters of `s` (on a char boundary).
fn truncate_chars(s: &str, max: usize) -> &str {
    match s.char_indices().nth(max) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

fn classify(line: &str) -> Option<Kind> {
    let line = line.trim();
    line.char_indices().nth(3)?; // fewer than 4 characters: nothing to classify
    let probe = truncate_chars(line, MATCH_CHARS);
    if NEGATIVE.is_match(probe) {
        return None;
    }
    if STRONG.is_match(probe) {
        Some(Kind::Strong)
    } else if WEAK.is_match(probe) {
        Some(Kind::Weak)
    } else {
        None
    }
}

/// Error lines found in OCR text, most significant first, at most 8, each trimmed and ≤ 240 chars.
///
/// `class` picks strictness: [`AppClass::Editor`] uses only STRONG patterns (source code is full
/// of the word "Error"); Terminal, Browser and Other use STRONG + WEAK. STRONG lines come before
/// WEAK lines, otherwise original order. Lines with the same [`line_key`] appear once.
pub fn find_errors(text: &str, class: AppClass) -> Vec<String> {
    let use_weak = class != AppClass::Editor;
    let mut seen: HashSet<String> = HashSet::new();
    let mut strong: Vec<String> = Vec::new();
    let mut weak: Vec<String> = Vec::new();
    for raw in text.lines() {
        let kind = match classify(raw) {
            Some(Kind::Weak) if !use_weak => continue,
            Some(k) => k,
            None => continue,
        };
        let shown = truncate_chars(raw.trim(), MAX_LINE_CHARS).trim_end();
        if !seen.insert(line_key(shown)) {
            continue;
        }
        match kind {
            Kind::Strong => strong.push(shown.to_string()),
            Kind::Weak => weak.push(shown.to_string()),
        }
        if strong.len() >= MAX_LINES {
            break;
        }
    }
    strong.extend(weak);
    strong.truncate(MAX_LINES);
    strong
}

/// true if `line` matches a STRONG pattern (and no negative one). Used for scoring.
pub fn is_strong(line: &str) -> bool {
    classify(line) == Some(Kind::Strong)
}

/// Replace free-standing digit runs with `#`. A run directly after an ASCII letter is part of
/// an identifier (`e0308`, `ts2339`, `x86`) and is kept.
fn mask_digits(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i].is_ascii_digit() {
            let mut j = i;
            while j < chars.len() && chars[j].is_ascii_digit() {
                j += 1;
            }
            let in_identifier = i > 0 && chars[i - 1].is_ascii_alphabetic();
            if in_identifier {
                out.extend(&chars[i..j]);
            } else {
                out.push('#');
            }
            i = j;
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    out
}

/// Normalised dedupe key: lowercase; `0x…` and runs of ≥ 8 hex digits → `h`; other free-standing
/// digit runs → `#` (digits glued to a letter, like the `0308` in `E0308`, are kept so error
/// codes stay distinguishable); the directory part of absolute paths dropped
/// (`/home/me/proj/src/main.rs:12:5` and `C:\Users\me\proj\src\main.rs:12:5` both become
/// `main.rs:#:#`); whitespace collapsed; at most 120 characters.
pub fn line_key(line: &str) -> String {
    let s = line.to_lowercase();
    let s = RE_PATH.replace_all(&s, "${1}");
    let s = RE_ISO_T.replace_all(&s, "${1} ${2}");
    let s = RE_HEX_0X.replace_all(&s, "h");
    let s = RE_HEX_RUN.replace_all(&s, "h");
    let s = mask_digits(&s);
    let collapsed = s.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate_chars(&collapsed, KEY_CHARS).to_string()
}

/// Key for a failed shell command: the command's first two words (the first one reduced to its
/// file name, `.exe` dropped) and the exit code, e.g. `cargo test` exit 101 →
/// `"shell:cargo test:101"`. Lowercased, at most 120 characters.
pub fn shell_error_key(cmd: &str, exit_code: i32) -> String {
    let mut words = cmd.split_whitespace();
    let first = words
        .next()
        .map(|w| {
            let base = w.rsplit(['/', '\\']).next().unwrap_or(w).to_lowercase();
            base.strip_suffix(".exe")
                .map(str::to_string)
                .unwrap_or(base)
        })
        .unwrap_or_default();
    let mut head = first;
    if let Some(second) = words.next() {
        head.push(' ');
        head.push_str(&second.to_lowercase());
    }
    let key = format!("shell:{head}:{exit_code}");
    truncate_chars(&key, KEY_CHARS).to_string()
}

/// Importance of a sighting, 0–100. The formula:
///
/// ```text
/// base    = 60            source is Shell (a non-zero exit is certain)
///         = 40            source is Ocr and the first line is STRONG
///         = 20            source is Ocr otherwise (weak, or no lines)
/// repeats = min(30, 10 * (count - 1))      count 0 and 1 give 0
/// recency = 0 if age_s < 300 (5 min), -10 if age_s < 1200 (20 min), else -25
/// score   = clamp(base + repeats + recency, 0, 100)
/// ```
pub fn score(source: ErrorSource, lines: &[String], count: u32, age_s: f64) -> u8 {
    let base: i32 = match source {
        ErrorSource::Shell => 60,
        ErrorSource::Ocr => {
            if lines.first().is_some_and(|l| is_strong(l)) {
                40
            } else {
                20
            }
        }
    };
    let repeats = (10 * i64::from(count.saturating_sub(1))).min(30) as i32;
    let recency = if age_s.is_nan() || age_s < 300.0 {
        0
    } else if age_s < 1200.0 {
        -10
    } else {
        -25
    };
    (base + repeats + recency).clamp(0, 100) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn all_builtin_patterns_compile() {
        // Touch every lazily compiled regex so a bad built-in pattern fails the suite.
        assert!(STRONG.len() > 40);
        assert!(WEAK.len() >= 8);
        assert!(NEGATIVE.len() >= 3);
        assert!(!RE_PATH.as_str().is_empty());
        assert!(!RE_ISO_T.as_str().is_empty());
        assert!(!RE_HEX_0X.as_str().is_empty());
        assert!(!RE_HEX_RUN.as_str().is_empty());
        // And exercise the line_key pipeline once.
        assert_eq!(line_key("A  0xFF 12"), "a h #");
    }

    #[test]
    fn python_traceback() {
        let text = "Traceback (most recent call last):\n  File \"/home/me/app/main.py\", line 42, in <module>\n    run()\n  File \"/home/me/app/main.py\", line 17, in run\n    return int(value)\nValueError: invalid literal for int() with base 10: 'abc'\n";
        let got = find_errors(text, AppClass::Terminal);
        assert_eq!(got.len(), 2);
        assert!(got[0].starts_with("Traceback"));
        assert!(got[1].starts_with("ValueError: invalid literal"));
    }

    #[test]
    fn rustc_error_with_location_lines() {
        let text = "error[E0502]: cannot borrow `self.buf` as mutable because it is also borrowed as immutable\n  --> src/main.rs:12:9\n   |\n12 |         self.buf.push(1);\n   |         ^^^^^^^^^^^^^^^^ mutable borrow occurs here\n\nerror: aborting due to 1 previous error\n\nFor more information about this error, try `rustc --explain E0502`.\n";
        let got = find_errors(text, AppClass::Terminal);
        assert_eq!(got.len(), 2);
        assert!(got[0].starts_with("error[E0502]: cannot borrow"));
        assert!(got[1].starts_with("error: aborting"));
    }

    #[test]
    fn cargo_test_failure_summary() {
        let text = "running 3 tests\ntest tests::ok ... ok\ntest tests::bad ... FAILED\n\nfailures:\n\n---- tests::bad stdout ----\nthread 'tests::bad' panicked at src/lib.rs:10:5:\nassertion `left == right` failed\n\ntest result: FAILED. 2 passed; 1 failed; 0 ignored\nerror: test failed, to rerun pass `--lib`\n";
        let got = find_errors(text, AppClass::Terminal);
        assert!(got.iter().any(|l| l == "test tests::bad ... FAILED"));
        assert!(got
            .iter()
            .any(|l| l.starts_with("thread 'tests::bad' panicked at")));
        assert!(got.iter().any(|l| l.starts_with("test result: FAILED")));
        assert!(got.iter().any(|l| l.starts_with("error: test failed")));
        assert!(!got.iter().any(|l| l.contains("... ok")));
        assert!(!got.iter().any(|l| l.starts_with("running")));
    }

    #[test]
    fn tsc_output() {
        let text = "src/app.ts:12:5 - error TS2339: Property 'foo' does not exist on type 'Bar'.\n\n12     bar.foo();\n       ~~~\n\nFound 1 error in src/app.ts:12\n";
        let got = find_errors(text, AppClass::Terminal);
        assert_eq!(got.len(), 2);
        assert!(got[0].contains("error TS2339"));
        assert!(got[1].starts_with("Found 1 error"));
        // OCR reads the code in mixed case.
        assert!(is_strong("src/a.ts(3,4): error Ts2339: nope"));
        // Older tsc format.
        assert!(is_strong(
            "src/a.ts(3,4): error TS2304: Cannot find name 'x'."
        ));
    }

    #[test]
    fn npm_err_block() {
        let text = "npm ERR! code ELIFECYCLE\nnpm ERR! errno 1\nnpm ERR! app@1.0.0 build: `tsc`\nnpm ERR! Exit status 1\n";
        let got = find_errors(text, AppClass::Terminal);
        assert_eq!(got.len(), 4);
        assert!(got.iter().all(|l| l.starts_with("npm ERR!")));
        assert!(is_strong("npm error code ENOENT"));
    }

    #[test]
    fn go_panic() {
        let text = "panic: runtime error: index out of range [5] with length 3\n\ngoroutine 1 [running]:\nmain.main()\n\t/home/me/app/main.go:9 +0x1d\nexit status 2\n";
        let got = find_errors(text, AppClass::Terminal);
        assert_eq!(got.len(), 2);
        assert!(got[0].starts_with("panic: runtime error"));
        assert_eq!(got[1], "exit status 2");
        assert!(is_strong(
            "fatal error: all goroutines are asleep - deadlock!"
        ));
        assert!(is_strong("--- FAIL: TestAdd (0.00s)"));
    }

    #[test]
    fn gcc_linker_error() {
        let text = "/usr/bin/ld: main.o: in function `main':\nmain.c:(.text+0x15): undefined reference to `foo'\ncollect2: error: ld returned 1 exit status\n";
        let got = find_errors(text, AppClass::Terminal);
        assert_eq!(got.len(), 2);
        assert!(got[0].contains("undefined reference to `foo'"));
        assert!(got[1].starts_with("collect2: error:"));
        assert!(is_strong(
            "main.c:4:10: fatal error: foo.h: No such file or directory"
        ));
        assert!(is_strong("Segmentation fault (core dumped)"));
    }

    #[test]
    fn powershell_command_not_found_english() {
        let text = "foo : The term 'foo' is not recognized as the name of a cmdlet, function, script file, or operable program.\nAt line:1 char:1\n+ foo\n+ ~~~\n    + CategoryInfo          : ObjectNotFound: (foo:String) [], CommandNotFoundException\n    + FullyQualifiedErrorId : CommandNotFoundException\n";
        let got = find_errors(text, AppClass::Terminal);
        assert!(got
            .iter()
            .any(|l| l.contains("is not recognized as the name of a cmdlet")));
        assert!(got.iter().any(|l| l.contains("CommandNotFoundException")));
        assert!(!got.iter().any(|l| l.starts_with("At line")));
        assert!(is_strong(
            "'foo' is not recognized as an internal or external command,"
        ));
    }

    #[test]
    fn powershell_command_not_found_traditional_chinese() {
        let ps = "foo : 無法辨識 'foo' 詞彙是否為 Cmdlet、函數、指令檔或可執行程式的名稱。請檢查名稱拼字。";
        let cmd = "'foo' 不是內部或外部命令、可執行的程式或批次檔。";
        assert!(is_strong(ps));
        assert!(is_strong(cmd));
        assert_eq!(
            find_errors(&format!("{ps}\n{cmd}"), AppClass::Terminal).len(),
            2
        );
        assert!(is_strong("'foo' 不是内部或外部命令，也不是可运行的程序"));
    }

    #[test]
    fn nextjs_build_failure() {
        let text = "Failed to compile.\n\n./src/app/page.tsx:12:5\nType error: Property 'foo' does not exist on type 'Props'.\n\n  10 | export default function Page() {\n\n> Build error occurred\nError: Command \"next build\" exited with 1\n";
        let got = find_errors(text, AppClass::Terminal);
        assert!(got.iter().any(|l| l == "Failed to compile."));
        assert!(got.iter().any(|l| l.starts_with("Type error: Property")));
        assert!(got.iter().any(|l| l.contains("Build error occurred")));
        assert!(got.iter().any(|l| l.starts_with("Error: Command")));
        assert!(is_strong("Unhandled Runtime Error"));
    }

    #[test]
    fn pytest_summary() {
        let text = "============================= test session starts =============================\ncollected 5 items\n\nFAILED tests/test_api.py::test_login - AssertionError: expected 200\n========================= 1 failed, 4 passed in 0.52s =========================\n";
        let got = find_errors(text, AppClass::Terminal);
        assert_eq!(got.len(), 2);
        assert!(got[0].starts_with("FAILED tests/test_api.py::test_login"));
        assert!(got[1].contains("1 failed, 4 passed"));
    }

    #[test]
    fn docker_daemon_error() {
        let text = "docker: Error response from daemon: driver failed programming external connectivity on endpoint web: Bind for 0.0.0.0:80 failed: port is already allocated.\nSee 'docker run --help'.\n";
        let got = find_errors(text, AppClass::Terminal);
        assert_eq!(got.len(), 1);
        assert!(got[0].contains("Error response from daemon"));
        assert!(is_strong("pod/web-1   0/1   CrashLoopBackOff   5   3m"));
        assert!(is_strong("Back-off pulling image: ImagePullBackOff"));
    }

    #[test]
    fn misc_strong_families() {
        for line in [
            "Error: listen EADDRINUSE: address already in use :::3000",
            "Error: connect ECONNREFUSED 127.0.0.1:5432",
            "Uncaught TypeError: x is not a function",
            "UnhandledPromiseRejection: This error originated either by throwing",
            "Build FAILED.",
            "BUILD FAILED in 3s",
            "編譯失敗，請檢查程式碼",
            "构建失败: 退出码 1",
            "Process exited with code 1",
            "Exit code: 2",
            "fatal: not a git repository (or any of the parent directories): .git",
            "bash: foo: command not found",
            "zsh: command not found: foo",
            "bash: ./run.sh: Permission denied",
            "cat: x.txt: No such file or directory",
            "zsh: no such file or directory: ./foo",
            "Exception in thread \"main\" java.lang.NullPointerException",
            "Caused by: java.io.IOException: disk full",
            "Unhandled exception. System.InvalidOperationException: boom",
            "Program.cs(10,5): error CS0103: The name 'x' does not exist",
            "error MSB3073: The command exited with code 1.",
            "Error response from daemon: pull access denied",
            "2 failed, 3 passed",
            "3 failing",
            "ParserError: Unexpected token",
            "requests.exceptions.ConnectionError: HTTPSConnectionPool(host='x')",
        ] {
            assert!(is_strong(line), "should be strong: {line}");
        }
    }

    #[test]
    fn weak_patterns_match_for_terminal_and_browser_not_editor() {
        let weak_only = [
            "ERROR connecting to database at startup",
            "[ERROR] build step failed to start",
            "Internal Server Error",
            "502 Bad Gateway",
            "Application error: a client-side exception has occurred",
            "Access is denied.",
            "拒絕存取。",
            "發生錯誤，請稍後再試",
            "✖ Something went wrong here",
            "2026-10-10 13:58:03,123 ERROR worker crashed",
        ];
        for line in weak_only {
            assert!(!is_strong(line), "should not be strong: {line}");
            for class in [AppClass::Terminal, AppClass::Browser, AppClass::Other] {
                assert_eq!(
                    find_errors(line, class),
                    vec![line.to_string()],
                    "{class:?}: {line}"
                );
            }
            assert!(
                find_errors(line, AppClass::Editor).is_empty(),
                "editor: {line}"
            );
        }
        // The multiplication sign is a browser tab close button.
        assert!(find_errors("× Some tab title that is long", AppClass::Browser).is_empty());
    }

    #[test]
    fn editor_keeps_strong_lines_only() {
        let text = "ERROR: weak only here\nerror[E0308]: mismatched types\nInternal Server Error\n";
        assert_eq!(
            find_errors(text, AppClass::Editor),
            s(&["error[E0308]: mismatched types"])
        );
        let all = find_errors(text, AppClass::Terminal);
        assert_eq!(all.len(), 3);
        assert_eq!(all[0], "error[E0308]: mismatched types");
    }

    #[test]
    fn strong_lines_come_first_and_order_is_kept() {
        let text =
            "ERROR a weak one\nerror: first strong\n[ERROR] second weak\nerror: second strong\n";
        assert_eq!(
            find_errors(text, AppClass::Terminal),
            s(&[
                "error: first strong",
                "error: second strong",
                "ERROR a weak one",
                "[ERROR] second weak"
            ])
        );
    }

    #[test]
    fn success_lines_are_not_errors() {
        for line in [
            "0 failed",
            "0 errors",
            "0 errors, 0 warnings",
            "test result: ok. 12 passed; 0 failed; 0 ignored",
            "error: 0",
            "errors: 0",
            "Found 0 errors. Watching for file changes.",
            "No errors found.",
            "Tests: 12 passed, 0 failed",
            "Process exited with code 0",
            "exit status 0",
            "Exit code: 0",
            "Build succeeded.",
            "ab",
            "err",
            "Error",
        ] {
            for class in [
                AppClass::Terminal,
                AppClass::Editor,
                AppClass::Browser,
                AppClass::Other,
            ] {
                assert!(find_errors(line, class).is_empty(), "{class:?}: {line}");
            }
            assert!(!is_strong(line), "{line}");
        }
    }

    #[test]
    fn source_code_is_not_an_error_in_an_editor() {
        let code = "class MyError extends Error {\n  constructor(msg) { super(msg); }\n}\nthrow new Error(\"boom\");\nconst ERR_BAD = 1;\nif (e.code === 'ENOENT') {\n  return null;\n}\nexcept ValueError:\nraise ValueError(\"x\")\npub enum Error {\n    NotFound,\n}\n// Error: handled elsewhere\nlet error = None;\n";
        assert!(find_errors(code, AppClass::Editor).is_empty());
        assert!(find_errors(code, AppClass::Terminal).is_empty());
    }

    #[test]
    fn caps_on_lines_and_length() {
        let mut text = String::new();
        for i in 0..40 {
            text.push_str(&format!("error: problem number {i} {}\n", "x".repeat(i)));
        }
        let got = find_errors(&text, AppClass::Terminal);
        assert_eq!(got.len(), 8);
        let long = format!("error: {}", "y".repeat(5000));
        let got = find_errors(&long, AppClass::Terminal);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].chars().count(), 240);
        // Multibyte text is cut on a char boundary.
        let cjk = format!("error: {}", "錯".repeat(500));
        let got = find_errors(&cjk, AppClass::Terminal);
        assert_eq!(got[0].chars().count(), 240);
        // Lines are trimmed.
        assert_eq!(
            find_errors("   error: padded   ", AppClass::Terminal),
            s(&["error: padded"])
        );
    }

    #[test]
    fn find_errors_deduplicates_by_key() {
        let text = "src/a.rs:10:5: error: expected `;`\nsrc/a.rs:99:7: error: expected `;`\nsrc/a.rs:99:7: error: expected `)`\n";
        assert_eq!(find_errors(text, AppClass::Terminal).len(), 2);
    }

    #[test]
    fn line_key_ignores_numbers_hex_paths_and_timestamps() {
        assert_eq!(
            line_key("thread 'main' panicked at src/main.rs:12:5"),
            line_key("thread 'main' panicked at src/main.rs:99:1")
        );
        assert_eq!(
            line_key("segfault at 0x7ffd1234abcd ip 0x00005555deadbeef"),
            line_key("segfault at 0x7ffe9999ffff ip 0x00005555cafebabe")
        );
        assert_eq!(
            line_key("object 3f9a1c2e77 freed"),
            line_key("object aa00bb11cc freed")
        );
        assert_eq!(
            line_key("/home/me/proj/src/main.rs:12:5"),
            line_key(r"C:\Users\me\proj\src\main.rs:12:5")
        );
        assert_eq!(line_key("/home/me/proj/src/main.rs:12:5"), "main.rs:#:#");
        assert_eq!(
            line_key(r"C:\Users\me\proj\src\main.rs:12:5"),
            "main.rs:#:#"
        );
        assert_eq!(
            line_key("File \"/home/a/x.py\", line 12, in f"),
            line_key("File \"C:\\Users\\b\\w\\x.py\", line 99, in f")
        );
        assert_eq!(
            line_key("error at /home/me/a/b/c.rs:1:1"),
            line_key("error at /tmp/other/c.rs:7:3")
        );
        assert_eq!(
            line_key("[2026-10-10 13:58:03] connect refused after 15 ms"),
            line_key("[2026-10-11 09:01:44] connect refused after 230 ms")
        );
        assert_eq!(
            line_key("2026-10-10T13:58:03.123Z request failed"),
            line_key("2026-10-11T09:01:44.987Z request failed")
        );
        assert_eq!(
            line_key("error: could not compile `foo` due to 2 previous errors"),
            line_key("error: could not compile `foo` due to 5 previous errors")
        );
    }

    #[test]
    fn line_key_normalises_form() {
        assert_eq!(line_key("  Hello   WORLD \t 42 "), "hello world #");
        assert_eq!(line_key("addr 0xDEADBEEF"), "addr h");
        assert_eq!(line_key("sha 0123456789abcdef done"), "sha h done");
        assert_eq!(line_key("short abc1234 stays"), "short abc1234 stays");
        let long = "word ".repeat(100);
        assert_eq!(line_key(&long).chars().count(), 120);
        assert_eq!(line_key("錯誤 12 行"), "錯誤 # 行");
    }

    #[test]
    fn line_key_keeps_error_codes_distinct() {
        assert_ne!(
            line_key("error[E0308]: mismatched types"),
            line_key("error[E0502]: mismatched types")
        );
        assert_ne!(
            line_key("x.ts - error TS2339: nope"),
            line_key("x.ts - error TS2345: nope")
        );
        assert_ne!(
            line_key("Program.cs(1,1): error CS0103: nope"),
            line_key("Program.cs(1,1): error CS0246: nope")
        );
        assert_eq!(
            line_key("error[E0308]: mismatched types"),
            "error[e0308]: mismatched types"
        );
        // Genuinely different messages differ.
        assert_ne!(
            line_key("error: expected `;`"),
            line_key("error: expected `)`")
        );
        assert_ne!(line_key("ValueError: bad"), line_key("KeyError: bad"));
    }

    #[test]
    fn shell_key() {
        assert_eq!(shell_error_key("cargo test", 101), "shell:cargo test:101");
        assert_eq!(
            shell_error_key("cargo test --lib -- --nocapture", 101),
            "shell:cargo test:101"
        );
        assert_eq!(
            shell_error_key("  Cargo   BUILD  ", 1),
            "shell:cargo build:1"
        );
        assert_eq!(shell_error_key("make", 2), "shell:make:2");
        assert_eq!(
            shell_error_key("/usr/bin/python3 x.py", 1),
            "shell:python3 x.py:1"
        );
        assert_eq!(
            shell_error_key(r"C:\bin\Node.EXE app.js", 1),
            "shell:node app.js:1"
        );
        assert_eq!(shell_error_key("", 127), "shell::127");
        assert_ne!(
            shell_error_key("cargo test", 101),
            shell_error_key("cargo test", 1)
        );
        assert_ne!(
            shell_error_key("cargo test", 101),
            shell_error_key("cargo build", 101)
        );
        assert_eq!(shell_error_key("cargo test", -1), "shell:cargo test:-1");
        assert!(shell_error_key(&"a".repeat(500), 1).chars().count() <= 120);
    }

    #[test]
    fn score_ordering_and_bounds() {
        let strong = s(&["error: boom"]);
        let weak = s(&["ERROR boom"]);
        let shell = score(ErrorSource::Shell, &[], 1, 0.0);
        let ocr_strong = score(ErrorSource::Ocr, &strong, 1, 0.0);
        let ocr_weak = score(ErrorSource::Ocr, &weak, 1, 0.0);
        assert_eq!((shell, ocr_strong, ocr_weak), (60, 40, 20));
        assert!(shell > ocr_strong && ocr_strong > ocr_weak);
        // Repeats raise, capped at +30.
        assert_eq!(score(ErrorSource::Ocr, &strong, 2, 0.0), 50);
        assert_eq!(score(ErrorSource::Ocr, &strong, 4, 0.0), 70);
        assert_eq!(score(ErrorSource::Ocr, &strong, 99, 0.0), 70);
        assert_eq!(score(ErrorSource::Ocr, &strong, 0, 0.0), 40);
        assert_eq!(score(ErrorSource::Shell, &[], u32::MAX, 0.0), 90);
        // Age lowers.
        assert_eq!(score(ErrorSource::Ocr, &strong, 1, 299.0), 40);
        assert_eq!(score(ErrorSource::Ocr, &strong, 1, 300.0), 30);
        assert_eq!(score(ErrorSource::Ocr, &strong, 1, 1199.0), 30);
        assert_eq!(score(ErrorSource::Ocr, &strong, 1, 1200.0), 15);
        // Bounds.
        assert_eq!(score(ErrorSource::Ocr, &[], 1, 1e9), 0);
        assert_eq!(score(ErrorSource::Ocr, &weak, 1, f64::INFINITY), 0);
        assert!(score(ErrorSource::Shell, &[], 1, f64::NAN) <= 100);
        assert_eq!(score(ErrorSource::Ocr, &weak, 1, -5.0), 20);
    }

    #[test]
    fn rustc_error_survives_ocr_misreads() {
        for line in [
            "error[E0308]: mismatched types",
            "errorLE0308]: mismatched types",
            "error[EO308]: mismatched types",
            "error: could not compile `demo`",
        ] {
            assert_eq!(
                find_errors(line, AppClass::Editor),
                vec![line.to_string()],
                "{line}"
            );
        }
        assert!(find_errors("errors: none", AppClass::Terminal).is_empty());
    }
}
