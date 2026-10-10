//! Estimated-token budgeting for the digest.
//!
//! The estimate is deliberately a little pessimistic and needs no tokenizer:
//! ASCII characters count 1/4 token each (rounded up over the whole string)
//! and every non-ASCII character counts as one whole token.

const ELLIPSIS: char = '…';

/// (ascii chars, non-ascii chars)
fn counts(text: &str) -> (usize, usize) {
    let ascii = text.chars().filter(char::is_ascii).count();
    (ascii, text.chars().count() - ascii)
}

fn tokens(ascii: usize, non_ascii: usize) -> usize {
    ascii.div_ceil(4) + non_ascii
}

/// Rough token estimate: ASCII characters count 1/4 token each (rounded up over the whole
/// string), every non-ASCII character counts 1 token.
pub fn estimate_tokens(text: &str) -> usize {
    let (a, n) = counts(text);
    tokens(a, n)
}

/// Longest prefix of `text` that fits `max_tokens` when `extra_ascii` more ASCII characters are
/// appended to it (0 for plain truncation, 1 for a trailing newline). The second value is
/// whether anything was cut. When cut, the result ends in '…', which is part of the budget.
fn cut(text: &str, max_tokens: usize, extra_ascii: usize) -> (String, bool) {
    let (a, n) = counts(text);
    if tokens(a + extra_ascii, n) <= max_tokens {
        return (text.to_string(), false);
    }
    // The ellipsis is one non-ASCII character. Prefix cost is monotonic in its length.
    let mut end = 0;
    let (mut pa, mut pn) = (0usize, 0usize);
    for (i, c) in text.char_indices() {
        let (na, nn) = if c.is_ascii() {
            (pa + 1, pn)
        } else {
            (pa, pn + 1)
        };
        if tokens(na + extra_ascii, nn + 1) > max_tokens {
            break;
        }
        pa = na;
        pn = nn;
        end = i + c.len_utf8();
    }
    if tokens(extra_ascii, 1) > max_tokens {
        return (String::new(), true); // not even the bare ellipsis fits
    }
    let mut out = String::with_capacity(end + ELLIPSIS.len_utf8());
    out.push_str(&text[..end]);
    out.push(ELLIPSIS);
    (out, true)
}

/// Longest prefix of `text` (cut on a char boundary) whose estimate is ≤ `max_tokens`; appends
/// '…' when something was cut (the ellipsis is included in the budget). `max_tokens = 0` → "".
pub fn truncate_to_tokens(text: &str, max_tokens: usize) -> String {
    cut(text, max_tokens, 0).0
}

/// Accumulates lines under a limit. Each line costs its own estimate plus its newline.
pub struct Budget {
    limit: usize,
    used: usize,
    lines: Vec<String>,
}

impl Budget {
    pub fn new(limit: usize) -> Budget {
        Budget {
            limit,
            used: 0,
            lines: Vec::new(),
        }
    }

    pub fn used(&self) -> usize {
        self.used
    }

    /// Tokens left; 0 once forced lines have pushed `used` past the limit.
    pub fn remaining(&self) -> usize {
        self.limit.saturating_sub(self.used)
    }

    fn cost(line: &str) -> usize {
        let (a, n) = counts(line);
        tokens(a + 1, n)
    }

    fn add(&mut self, line: String, cost: usize) {
        self.used += cost;
        self.lines.push(line);
    }

    /// Append the line only if it fits entirely. Returns whether it was added.
    pub fn try_push(&mut self, line: &str) -> bool {
        let cost = Self::cost(line);
        if cost > self.remaining() {
            return false;
        }
        self.add(line.to_string(), cost);
        true
    }

    /// Append regardless of the limit (headers, fence markers: structure that must not be
    /// dropped).
    pub fn push_forced(&mut self, line: &str) {
        self.add(line.to_string(), Self::cost(line));
    }

    /// Append as much of the line as fits (cut like [`truncate_to_tokens`], so a cut line ends in
    /// '…'). Returns false, adding nothing, if no part of the line fits.
    pub fn push_truncated(&mut self, line: &str) -> bool {
        let (text, was_cut) = cut(line, self.remaining(), 1);
        if was_cut && text.trim_end_matches(ELLIPSIS).is_empty() {
            return false; // nothing but an ellipsis (or nothing at all) would be left
        }
        let cost = Self::cost(&text);
        if cost > self.remaining() {
            return false;
        }
        self.add(text, cost);
        true
    }

    /// Lines joined with '\n'.
    pub fn finish(self) -> String {
        self.lines.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimates() {
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("a"), 1);
        assert_eq!(estimate_tokens("abcd"), 1);
        assert_eq!(estimate_tokens("abcde"), 2);
        assert_eq!(estimate_tokens(&"a".repeat(400)), 100);
        assert_eq!(estimate_tokens("你好世界"), 4);
        assert_eq!(estimate_tokens("😀😀"), 2);
        // 8 ASCII = 2, 2 CJK = 2
        assert_eq!(estimate_tokens("hello wo你好"), 2 + 2);
        assert_eq!(estimate_tokens("…"), 1);
        // Rounded up over the whole string, not per word.
        assert_eq!(estimate_tokens("ab cd ef"), 2);
    }

    const SAMPLES: &[&str] = &[
        "the quick brown fox jumps over the lazy dog, again and again and again",
        "錯誤：無法連線到資料庫，請稍後再試。這是一段很長的中文訊息。",
        "error[E0308]: 型別不符 mismatched types — expected `u32`, found `&str` 😀😀",
        "😀😀😀😀😀😀😀😀😀😀",
        "a",
        "é",
        "",
    ];

    #[test]
    fn truncate_never_exceeds_limit_or_splits_chars() {
        for text in SAMPLES {
            let full = estimate_tokens(text);
            for max in 0..=full + 3 {
                let out = truncate_to_tokens(text, max);
                assert!(
                    estimate_tokens(&out) <= max,
                    "{text:?} max={max} -> {out:?}"
                );
                if max >= full {
                    assert_eq!(&out, text, "fits, so unchanged: max={max}");
                } else if max == 0 {
                    assert_eq!(out, "");
                } else {
                    assert!(out.ends_with('…'), "{text:?} max={max} -> {out:?}");
                    let body = out.trim_end_matches('…');
                    assert!(text.starts_with(body), "{text:?} max={max} -> {out:?}");
                }
            }
        }
    }

    #[test]
    fn truncate_takes_the_longest_prefix() {
        for text in SAMPLES {
            let full = estimate_tokens(text);
            for max in 1..full {
                let out = truncate_to_tokens(text, max);
                let body = out.trim_end_matches('…');
                if let Some(next) = text[body.len()..].chars().next() {
                    let longer = format!("{body}{next}…");
                    assert!(
                        estimate_tokens(&longer) > max,
                        "{text:?} max={max}: {out:?} + {next:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn truncate_concrete_cases() {
        assert_eq!(truncate_to_tokens("hello world", 0), "");
        assert_eq!(truncate_to_tokens("hello world", 100), "hello world");
        assert_eq!(truncate_to_tokens("hello world", 3), "hello world"); // 11 chars = 3 tokens
                                                                         // 2 tokens: ellipsis (1) + 1 token of ASCII = 4 chars.
        assert_eq!(truncate_to_tokens("hello world", 2), "hell…");
        assert_eq!(truncate_to_tokens("hello world", 1), "…");
        assert_eq!(truncate_to_tokens("你好世界", 3), "你好…");
        assert_eq!(truncate_to_tokens("你好世界", 1), "…");
        assert_eq!(truncate_to_tokens("你好世界", 4), "你好世界");
    }

    #[test]
    fn try_push_refusal_leaves_state_unchanged() {
        let mut b = Budget::new(5);
        assert!(b.try_push("abcdefg")); // 8 chars with newline = 2
        let (used, rem) = (b.used(), b.remaining());
        assert_eq!((used, rem), (2, 3));
        assert!(!b.try_push(&"x".repeat(100)));
        assert_eq!((b.used(), b.remaining()), (used, rem));
        assert!(b.try_push("ok")); // 3 chars = 1
        assert_eq!(b.finish(), "abcdefg\nok");
    }

    #[test]
    fn exact_fit_is_accepted() {
        let mut b = Budget::new(2);
        assert!(b.try_push("abcdefg")); // 7 + newline = 8 chars = 2 tokens
        assert_eq!(b.remaining(), 0);
        assert!(!b.try_push("a"));
        assert!(!b.try_push(""));
    }

    #[test]
    fn forced_push_can_exceed() {
        let mut b = Budget::new(3);
        b.push_forced("<<<DATA 3f9a1c2e>>> and a long header line");
        assert!(b.used() > 3);
        assert_eq!(b.remaining(), 0);
        assert!(!b.try_push("a"));
        assert!(!b.push_truncated("anything"));
        b.push_forced("<<<END>>>");
        assert_eq!(
            b.finish(),
            "<<<DATA 3f9a1c2e>>> and a long header line\n<<<END>>>"
        );
    }

    #[test]
    fn push_truncated_cuts_to_fit() {
        let mut b = Budget::new(5);
        assert!(b.push_truncated(&"x".repeat(200)));
        assert!(b.used() <= 5);
        let out = b.finish();
        assert!(out.ends_with('…'));
        assert!(out.starts_with("xxxx"));

        // A line that fits whole is not cut.
        let mut b = Budget::new(10);
        assert!(b.push_truncated("short"));
        assert_eq!(b.finish(), "short");

        // Nothing fits: false, state unchanged.
        let mut b = Budget::new(2);
        assert!(b.try_push("abcdefg")); // uses all 2
        assert!(!b.push_truncated("more text"));
        assert_eq!((b.used(), b.remaining()), (2, 0));

        // Only an ellipsis would fit: not worth a line.
        let mut b = Budget::new(2);
        assert!(!b.push_truncated("你好世界"));
        assert_eq!(b.used(), 0);
        let mut b1 = Budget::new(1);
        assert!(!b1.push_truncated(&"x".repeat(100)));
        assert_eq!(b1.used(), 0);
        assert_eq!(b.finish(), "");
    }

    #[test]
    fn used_never_exceeds_limit_without_forced_pushes() {
        let lines = [
            "Now: Code.exe — main.rs - screensense (4m)",
            "· 13:44–13:51 WindowsTerminal.exe — cargo test (7m)",
            "錯誤：無法連線到資料庫，請稍後再試。",
            "",
            "! 13:57 shell exit 101 (x2): cargo test",
            "  error[E0502]: cannot borrow `self.buf` as mutable because it is also borrowed",
            "😀😀😀😀😀😀😀😀",
        ];
        for limit in 0..80 {
            let mut b = Budget::new(limit);
            for (i, l) in lines.iter().cycle().take(20).enumerate() {
                if i % 2 == 0 {
                    b.try_push(l);
                } else {
                    b.push_truncated(l);
                }
                assert!(b.used() <= limit, "limit={limit} used={}", b.used());
                assert_eq!(b.used() + b.remaining(), limit);
            }
        }
    }

    #[test]
    fn finish_preserves_order_and_used_matches_cost() {
        let mut b = Budget::new(100);
        b.push_forced("header");
        assert!(b.try_push("one"));
        assert!(b.push_truncated("two"));
        b.push_forced("footer");
        let expected = ["header", "one", "two", "footer"]
            .iter()
            .map(|l| estimate_tokens(&format!("{l}\n")))
            .sum::<usize>();
        assert_eq!(b.used(), expected);
        assert_eq!(b.finish(), "header\none\ntwo\nfooter");
        assert_eq!(Budget::new(10).finish(), "");
    }

    #[test]
    fn zero_budget() {
        let mut b = Budget::new(0);
        assert!(!b.try_push("a"));
        assert!(!b.push_truncated("a"));
        assert_eq!(b.remaining(), 0);
        assert_eq!(b.used(), 0);
    }
}
