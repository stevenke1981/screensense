//! Is this browser window a private / incognito window?
//!
//! The title of a Chrome/Brave/Opera/Vivaldi incognito window does not say so,
//! but the browser UI exposes it through UI Automation (the profile button,
//! the window's accessible name). We walk the browser chrome with a hard
//! node/depth/time budget and look for known markers.
//!
//! The answer is tri-state and fail-closed: `None` ("could not tell") is
//! always safe, the caller does not capture on `None`.

use std::collections::VecDeque;
use std::ffi::c_void;
use std::time::{Duration, Instant};

use windows::core::Interface;
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};
use windows::Win32::UI::Accessibility::{
    CUIAutomation, IUIAutomation, IUIAutomation6, IUIAutomationElement, IUIAutomationTreeWalker,
    UIA_DocumentControlTypeId,
};

/// Visit at most this many elements.
const MAX_NODES: usize = 400;
/// Do not descend below this depth (root = 0).
const MAX_DEPTH: u32 = 10;
/// Wall-clock budget for the whole walk. It runs on the capture thread and
/// must not stall it.
const BUDGET: Duration = Duration::from_millis(600);
/// Per-call UIA timeouts (ms), so a hung browser cannot block one call for the
/// default 20 s. Best effort: needs Windows 10+ (`IUIAutomation6`).
const CALL_TIMEOUT_MS: u32 = 400;
/// A walk that saw fewer named elements than this probably hit a browser whose
/// accessibility tree is not built yet (Chromium builds it lazily), so
/// "no marker found" proves nothing.
const MIN_NAMED_FOR_NEGATIVE: usize = 30;
/// Element names are only inspected up to this many characters.
const MAX_NAME_CHARS: usize = 1024;

/// Lower-cased markers. Latin ones are matched case-insensitively (the name is
/// lower-cased first); CJK/Hangul have no case so they match as-is.
/// The last group goes beyond the required list: more locales for the same
/// concept. A false positive here only means a window is not captured.
const MARKERS: &[&str] = &[
    "(incognito)",
    "incognito",
    "incognito window",
    "inprivate",
    "[inprivate]",
    "private browsing",
    "private window",
    "(無痕模式)",
    "無痕視窗",
    "无痕模式",
    "无痕窗口",
    "(シークレット)",
    "シークレット ウィンドウ",
    "시크릿",
    "navegación privada",
    "navigation privée",
    "privates fenster",
    "navegação anónima",
    "navegação anônima",
    // extras
    "incógnito",
    "janela anónima",
    "janela anônima",
    "inkognito",
];

/// Does this accessible name announce a private-browsing window?
pub(crate) fn is_private_marker(name: &str) -> bool {
    let clipped: String = name.chars().take(MAX_NAME_CHARS).collect();
    let lowered = clipped.to_lowercase();
    MARKERS.iter().any(|marker| lowered.contains(marker))
}

/// The final answer for a finished (or abandoned) walk that found no marker.
/// `Some(false)` only for a complete walk over a populated tree.
fn negative_verdict(complete: bool, named: usize) -> Option<bool> {
    if complete && named >= MIN_NAMED_FOR_NEGATIVE {
        Some(false)
    } else {
        None
    }
}

/// `Some(true)` if the window behind `hwnd` shows a private-mode marker,
/// `Some(false)` if the walk finished within limits and saw enough named
/// elements to be meaningful, `None` for anything else (COM error, limits hit,
/// tree not built yet).
pub fn probe_private(hwnd: isize) -> Option<bool> {
    crate::win_misc::ensure_com();
    let hwnd = HWND(hwnd as usize as *mut c_void);
    walk(hwnd).unwrap_or(None)
}

/// A UIA "no such element" answer arrives as a successful HRESULT with a null
/// pointer, which the windows crate turns into an `Err` whose code is a
/// success code. Treat that (and `E_POINTER`) as "no more elements".
fn is_no_element(e: &windows::core::Error) -> bool {
    e.code().is_ok() || e.code() == windows::Win32::Foundation::E_POINTER
}

fn walk(hwnd: HWND) -> windows::core::Result<Option<bool>> {
    // SAFETY: CUIAutomation is the documented in-proc class for IUIAutomation;
    // COM was initialised on this thread by `ensure_com`.
    let uia: IUIAutomation =
        unsafe { CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER)? };
    if let Ok(uia6) = uia.cast::<IUIAutomation6>() {
        // SAFETY: plain value arguments on a live interface; failures are ignored.
        unsafe {
            let _ = uia6.SetConnectionTimeout(CALL_TIMEOUT_MS);
            let _ = uia6.SetTransactionTimeout(CALL_TIMEOUT_MS);
        }
    }
    // SAFETY: `hwnd` is only a value passed to UIA, which validates it.
    let root = unsafe { uia.ElementFromHandle(hwnd)? };
    // SAFETY: live interface, no arguments.
    let walker = unsafe { uia.ControlViewWalker()? };

    let deadline = Instant::now() + BUDGET;
    let mut queue: VecDeque<(IUIAutomationElement, u32)> = VecDeque::new();
    queue.push_back((root, 0));
    let mut visited = 0usize;
    let mut named = 0usize;
    let mut complete = true;

    while let Some((element, depth)) = queue.pop_front() {
        if visited >= MAX_NODES || Instant::now() >= deadline {
            complete = false;
            break;
        }
        visited += 1;

        // Never look inside the rendered web page. A page can be enormous
        // (a walk would blow the budget) and its text is chosen by whoever
        // runs the page, e.g. a page that says "Incognito". Only browser
        // chrome (tab strip, toolbar, profile button) is consulted. The
        // Document element itself is skipped too: its name is the page title.
        // SAFETY: live element interface.
        match unsafe { element.CurrentControlType() } {
            Ok(kind) if kind == UIA_DocumentControlTypeId => continue,
            Ok(_) => {}
            Err(_) => {
                // Cannot rule out that it is the page: skip it, and the
                // walk no longer counts as complete.
                complete = false;
                continue;
            }
        }

        // SAFETY: live element interface.
        match unsafe { element.CurrentName() } {
            Ok(name) => {
                let name = name.to_string();
                if !name.is_empty() {
                    named += 1;
                    if is_private_marker(&name) {
                        return Ok(Some(true));
                    }
                }
            }
            Err(_) => complete = false,
        }

        enqueue_children(
            &walker,
            &element,
            depth,
            visited,
            deadline,
            &mut queue,
            &mut complete,
        );
    }

    Ok(negative_verdict(complete, named))
}

/// Append the control-view children of `element` to `queue`, clearing
/// `complete` whenever something is left out (depth, node or time limit, or
/// an enumeration error).
fn enqueue_children(
    walker: &IUIAutomationTreeWalker,
    element: &IUIAutomationElement,
    depth: u32,
    visited: usize,
    deadline: Instant,
    queue: &mut VecDeque<(IUIAutomationElement, u32)>,
    complete: &mut bool,
) {
    // SAFETY: live walker and element interfaces.
    let mut next = unsafe { walker.GetFirstChildElement(element) };
    if depth >= MAX_DEPTH {
        // Not descending: only matters if there was something below.
        if next.is_ok() {
            *complete = false;
        }
        return;
    }
    loop {
        match next {
            Ok(child) => {
                if visited + queue.len() >= MAX_NODES || Instant::now() >= deadline {
                    *complete = false;
                    return;
                }
                // SAFETY: live walker and element interfaces.
                let sibling = unsafe { walker.GetNextSiblingElement(&child) };
                queue.push_back((child, depth + 1));
                next = sibling;
            }
            Err(e) if is_no_element(&e) => return,
            Err(_) => {
                *complete = false;
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latin_markers_match_case_insensitively() {
        assert!(is_private_marker("New Tab - Google Chrome (Incognito)"));
        assert!(is_private_marker("INCOGNITO"));
        assert!(is_private_marker("Incognito window"));
        assert!(is_private_marker("Microsoft Edge [InPrivate]"));
        assert!(is_private_marker("InPrivate"));
        assert!(is_private_marker("Mozilla Firefox Private Browsing"));
        assert!(is_private_marker("Opera Private window"));
        assert!(is_private_marker("Navegación privada"));
        assert!(is_private_marker("NAVIGATION PRIVÉE"));
        assert!(is_private_marker("Privates Fenster"));
        assert!(is_private_marker("Navegação anónima"));
        assert!(is_private_marker("Navegação anônima"));
    }

    #[test]
    fn cjk_markers_match() {
        assert!(is_private_marker("新分頁 - Google Chrome (無痕模式)"));
        assert!(is_private_marker("無痕視窗"));
        assert!(is_private_marker("无痕模式"));
        assert!(is_private_marker("无痕窗口"));
        assert!(is_private_marker("新しいタブ (シークレット)"));
        assert!(is_private_marker("シークレット ウィンドウ"));
        assert!(is_private_marker("시크릿 모드"));
    }

    #[test]
    fn ordinary_names_do_not_match() {
        assert!(!is_private_marker(""));
        assert!(!is_private_marker("Address and search bar"));
        assert!(!is_private_marker("Rust documentation - Google Chrome"));
        assert!(!is_private_marker("Private equity returns"));
        assert!(!is_private_marker("新分頁"));
        assert!(!is_private_marker("Profile 1"));
    }

    #[test]
    fn marker_beyond_the_name_cap_is_ignored() {
        let long = format!("{}Incognito", "x".repeat(MAX_NAME_CHARS));
        assert!(!is_private_marker(&long));
    }

    #[test]
    fn negative_needs_a_complete_walk_over_a_populated_tree() {
        assert_eq!(negative_verdict(true, MIN_NAMED_FOR_NEGATIVE), Some(false));
        assert_eq!(negative_verdict(true, MIN_NAMED_FOR_NEGATIVE - 1), None);
        assert_eq!(negative_verdict(false, 1000), None);
    }
}
