//! End-to-end test of the Linux backend against a real X server.
//!
//! Gated on `SCREENSENSE_X11_TEST=1` (it needs a display; CI without one just
//! passes). Run it under a virtual display:
//!
//! ```text
//! cargo build -p screensense-platform --example demo_window
//! SCREENSENSE_X11_TEST=1 xvfb-run -a -s "-screen 0 1280x800x24" \
//!     cargo test -p screensense-platform --test x11_e2e -- --nocapture
//! ```
#![cfg(target_os = "linux")]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use screensense_platform::{create_ocr, create_platform, Image, PlatformError, WindowInfo};

/// The tests below change `DISPLAY`/`WAYLAND_DISPLAY`: run them one at a time.
static ENV_LOCK: Mutex<()> = Mutex::new(());

fn enabled() -> bool {
    if std::env::var("SCREENSENSE_X11_TEST").as_deref() == Ok("1") {
        true
    } else {
        eprintln!("x11_e2e: skipped (set SCREENSENSE_X11_TEST=1 and run under xvfb-run)");
        false
    }
}

/// Kills (and reaps) a child process when dropped.
struct KillOnDrop(Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> TempDir {
        let p = std::env::temp_dir().join(format!("ss-x11-e2e-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).expect("create temp dir");
        TempDir(p)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn wait_for<T>(what: &str, timeout: Duration, mut f: impl FnMut() -> Option<T>) -> T {
    let start = Instant::now();
    loop {
        if let Some(v) = f() {
            return v;
        }
        assert!(
            start.elapsed() < timeout,
            "timed out after {timeout:?} waiting for: {what}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Path of the demo window binary, building it if it is not there yet.
fn demo_binary() -> Option<PathBuf> {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| manifest.join("../../target"));
    let bin = target.join("debug/examples/demo_window");
    if !bin.exists() {
        eprintln!("x11_e2e: {} missing, building the example", bin.display());
        let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
        let _ = Command::new(cargo)
            .args([
                "build",
                "-p",
                "screensense-platform",
                "--example",
                "demo_window",
            ])
            .status();
    }
    bin.exists().then_some(bin)
}

fn dark_and_light(img: &Image) -> (usize, usize) {
    let mut dark = 0;
    let mut light = 0;
    for px in img.rgba.chunks_exact(4) {
        if px[..3].iter().all(|&c| c < 64) {
            dark += 1;
        }
        if px[..3].iter().all(|&c| c > 192) {
            light += 1;
        }
    }
    (dark, light)
}

fn joined_text(lines: &[screensense_platform::OcrLine]) -> String {
    lines
        .iter()
        .map(|l| {
            l.words
                .iter()
                .map(|w| w.text.as_str())
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn x11_end_to_end() {
    if !enabled() {
        return;
    }
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let Some(demo) = demo_binary() else {
        eprintln!("x11_e2e: SKIPPED: demo_window example binary not found and could not be built");
        return;
    };

    let dir = TempDir::new("main");
    let title_file = dir.0.join("title.txt");
    let text_file = dir.0.join("text.txt");
    let title = "cargo test - demo";
    std::fs::write(&title_file, title).expect("write title file");
    let line1 = "error[E0308]: mismatched types";
    let line2 = "  --> src/main.rs:12:5";
    std::fs::write(&text_file, format!("{line1}\n{line2}\n")).expect("write text file");

    let child = Command::new(&demo)
        .args(["--title", title, "--class", "demo-term", "--seconds", "60"])
        .arg("--title-file")
        .arg(&title_file)
        .arg("--text-file")
        .arg(&text_file)
        .stdout(Stdio::null())
        .spawn()
        .expect("spawn demo_window");
    let demo_pid = child.id();
    let mut child = KillOnDrop(child);

    let mut p = create_platform();
    assert_eq!(
        p.name(),
        "x11",
        "backend is {} ({:?})",
        p.name(),
        p.capabilities().notes
    );

    // -- foreground ---------------------------------------------------------
    let info: WindowInfo = wait_for("demo window to take focus", Duration::from_secs(15), || {
        p.foreground().filter(|w| w.title == title)
    });
    eprintln!("foreground: {info:?}");
    assert_eq!(info.title, title);
    assert_eq!(info.class, "demo-term");
    assert_eq!(info.pid, demo_pid);
    assert_eq!(info.app, "demo_window");
    assert!(!info.minimized);
    assert_eq!((info.width, info.height), (900, 500));
    assert_eq!(
        (info.x, info.y),
        (40, 40),
        "window position in root coordinates"
    );
    assert_ne!(info.handle, 0);

    // -- capabilities ---------------------------------------------------------
    let caps = p.capabilities();
    eprintln!("capabilities: {caps:?}");
    assert!(caps.foreground && caps.capture);
    assert!(!caps.private_probe);

    // -- idle ---------------------------------------------------------------
    let idle = p.idle_seconds();
    eprintln!("idle: {idle:?}");
    assert_eq!(idle.is_some(), caps.idle);
    if let Some(s) = idle {
        assert!(s.is_finite() && s >= 0.0);
    }

    // -- private browsing / notify -------------------------------------------
    assert_eq!(p.is_private_browsing(&info), None);
    if !caps.notify {
        // Do not pop real notifications; only check the "no binary" contract.
        assert!(matches!(
            p.notify("t", "b"),
            Err(PlatformError::Unsupported(_))
        ));
    }

    // -- capture ------------------------------------------------------------
    // Let the redraw after the focus change settle.
    std::thread::sleep(Duration::from_millis(300));
    let img = p.capture(&info).expect("capture of the demo window");
    eprintln!("captured {}x{}", img.width, img.height);
    assert!((800..=900).contains(&img.width) && (400..=500).contains(&img.height));
    assert_eq!(img.rgba.len(), (img.width * img.height * 4) as usize);
    assert!(
        img.rgba.chunks_exact(4).all(|px| px[3] == 255),
        "alpha must be opaque"
    );
    let (dark, light) = dark_and_light(&img);
    assert!(dark > 200, "no text pixels in the capture ({dark} dark)");
    assert!(
        light > 100_000,
        "no background in the capture ({light} light)"
    );
    // The window background is white, and the grab starts at the window's own corner.
    assert_eq!(
        &img.rgba[(5 * img.width as usize + 5) * 4..][..3],
        &[255, 255, 255]
    );

    // Refusals.
    let minimized = WindowInfo {
        minimized: true,
        ..info.clone()
    };
    assert!(matches!(
        p.capture(&minimized),
        Err(PlatformError::WindowGone(_))
    ));
    let tiny = WindowInfo {
        width: 39,
        ..info.clone()
    };
    assert!(matches!(
        p.capture(&tiny),
        Err(PlatformError::WindowGone(_))
    ));

    // -- OCR ----------------------------------------------------------------
    match create_ocr("") {
        Err(e) => eprintln!("x11_e2e: OCR part SKIPPED: {e}"),
        Ok(mut ocr) => {
            assert!(ocr.name().starts_with("tesseract("), "{}", ocr.name());
            let t = Instant::now();
            let lines = ocr.recognize(&img).expect("recognize");
            let text = joined_text(&lines);
            eprintln!(
                "ocr ({} ms) via {}:\n{text}",
                t.elapsed().as_millis(),
                ocr.name()
            );
            let words: Vec<_> = lines.iter().flat_map(|l| &l.words).collect();
            assert!(!words.is_empty(), "OCR returned nothing");
            for w in &words {
                assert!(w.w > 0 && w.h > 0, "empty box for {w:?}");
                assert!(
                    w.x + w.w <= img.width && w.y + w.h <= img.height,
                    "box of {w:?} is outside the {}x{} image",
                    img.width,
                    img.height
                );
            }
            assert!(
                text.to_lowercase().contains("mismatched"),
                "OCR did not read 'mismatched' from the core X font:\n{text}"
            );
            // Boxes are in ORIGINAL pixels: "mismatched" is the 4th word of the
            // first line, drawn at x = 16 + 14 chars * 10 px = 156, y ~ 16..40.
            let w = words
                .iter()
                .find(|w| w.text.to_lowercase().contains("mismatched"))
                .expect("word box for 'mismatched'");
            assert!(
                (130..=185).contains(&w.x),
                "x of 'mismatched' = {} (expected ~156)",
                w.x
            );
            assert!(
                w.y <= 40 && w.y + w.h <= 60,
                "y of 'mismatched' = {} h = {}",
                w.y,
                w.h
            );
        }
    }

    // -- the text on screen changes -------------------------------------------
    std::fs::write(&text_file, "something else entirely\n").expect("rewrite text file");
    let changed = wait_for(
        "redraw after text-file change",
        Duration::from_secs(10),
        || {
            let now = p.capture(&info).ok()?;
            (now.rgba != img.rgba).then_some(now)
        },
    );
    assert_eq!((changed.width, changed.height), (img.width, img.height));

    // -- the title changes ----------------------------------------------------
    let new_title = "cargo build - changed title";
    std::fs::write(&title_file, new_title).expect("rewrite title file");
    let after = wait_for("title change to be seen", Duration::from_secs(10), || {
        p.foreground().filter(|w| w.title == new_title)
    });
    assert_eq!(after.handle, info.handle);
    assert_eq!(after.pid, demo_pid);

    // -- the window goes away ---------------------------------------------------
    let _ = child.0.kill();
    let _ = child.0.wait();
    wait_for(
        "foreground to move off the dead window",
        Duration::from_secs(10),
        || match p.foreground() {
            None => Some(()),
            Some(w) if w.handle != info.handle => Some(()),
            Some(_) => None,
        },
    );
    let gone = p.capture(&info);
    assert!(
        matches!(gone, Err(PlatformError::WindowGone(_))),
        "capture of a destroyed window must be WindowGone, got {gone:?}"
    );
    // Nothing panics and the connection stays usable.
    assert!(p.foreground().is_none());
    assert_eq!(p.idle_seconds().is_some(), caps.idle);
}

/// The two ways of finding the focused window: EWMH `_NET_ACTIVE_WINDOW`
/// (written here by the test, standing in for a window manager) and the input
/// focus climbed to its top-level client.
#[test]
fn x11_ewmh_and_focus_climb() {
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::{
        AtomEnum, ConnectionExt as _, CreateWindowAux, InputFocus, PropMode, Window, WindowClass,
    };
    use x11rb::wrapper::ConnectionExt as _;
    use x11rb::{COPY_FROM_PARENT, CURRENT_TIME};

    if !enabled() {
        return;
    }
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut p = create_platform();
    assert_eq!(p.name(), "x11");

    let (conn, n) = x11rb::connect(None).expect("connect");
    let root = conn.setup().roots[n].root;
    let atom = |name: &str| {
        conn.intern_atom(false, name.as_bytes())
            .unwrap()
            .reply()
            .unwrap()
            .atom
    };
    let (active, wm_state, net_name, utf8) = (
        atom("_NET_ACTIVE_WINDOW"),
        atom("WM_STATE"),
        atom("_NET_WM_NAME"),
        atom("UTF8_STRING"),
    );
    let mk = |parent: Window, x: i16, y: i16, w: u16, h: u16| -> Window {
        let id = conn.generate_id().unwrap();
        conn.create_window(
            COPY_FROM_PARENT as u8,
            id,
            parent,
            x,
            y,
            w,
            h,
            0,
            WindowClass::INPUT_OUTPUT,
            COPY_FROM_PARENT,
            &CreateWindowAux::new(),
        )
        .unwrap();
        conn.map_window(id).unwrap();
        id
    };
    let set_active = |w: u32| {
        conn.change_property32(PropMode::REPLACE, root, active, AtomEnum::WINDOW, &[w])
            .unwrap();
        conn.flush().unwrap();
    };
    let focus = |w: Window| {
        conn.set_input_focus(InputFocus::PARENT, w, CURRENT_TIME)
            .unwrap()
            .check()
            .unwrap();
    };

    // top-level A > child B > grandchild C; a separate top-level D.
    let a = mk(root, 10, 10, 300, 200);
    let b = mk(a, 5, 5, 200, 100);
    let c = mk(b, 5, 5, 50, 50);
    let d = mk(root, 400, 10, 300, 200);
    // A: Latin-1 WM_NAME only (no EWMH name), plus the ICCCM WM_STATE marker.
    conn.change_property8(
        PropMode::REPLACE,
        a,
        AtomEnum::WM_NAME,
        AtomEnum::STRING,
        b"caf\xe9 term",
    )
    .unwrap();
    conn.change_property8(
        PropMode::REPLACE,
        a,
        AtomEnum::WM_CLASS,
        AtomEnum::STRING,
        b"inst\0Klass\0",
    )
    .unwrap();
    conn.change_property32(PropMode::REPLACE, a, wm_state, wm_state, &[1, 0])
        .unwrap();
    // B: an UTF-8 name of its own, no WM_STATE.
    conn.change_property8(
        PropMode::REPLACE,
        b,
        net_name,
        utf8,
        "inner \u{4f60}\u{597d}".as_bytes(),
    )
    .unwrap();
    // D: UTF-8 name, a pid that is not ours.
    conn.change_property8(PropMode::REPLACE, d, net_name, utf8, b"other window")
        .unwrap();
    conn.sync().unwrap();

    // No _NET_ACTIVE_WINDOW (bare X server): focus on the grandchild climbs to A,
    // the ancestor with WM_STATE, even though B has a name and is closer.
    focus(c);
    let w = p.foreground().expect("foreground via focus climb");
    assert_eq!(w.handle, u64::from(a));
    assert_eq!(w.title, "caf\u{e9} term");
    assert_eq!(
        (w.class.as_str(), w.app.as_str()),
        ("Klass", "inst"),
        "no _NET_WM_PID: instance"
    );
    assert_eq!(w.pid, 0);
    assert_eq!((w.x, w.y, w.width, w.height), (10, 10, 300, 200));

    // Without WM_STATE anywhere the nearest ancestor with a name is used: B.
    conn.delete_property(a, wm_state).unwrap();
    conn.sync().unwrap();
    let w = p.foreground().expect("foreground via named ancestor");
    assert_eq!(w.handle, u64::from(b));
    assert_eq!(w.title, "inner \u{4f60}\u{597d}");
    // B's geometry is reported in ROOT coordinates (A at 10,10 + 5,5).
    assert_eq!((w.x, w.y), (15, 15));
    // With no property on any of them there is no window to report.
    conn.delete_property(b, net_name).unwrap();
    conn.delete_property(a, AtomEnum::WM_NAME.into()).unwrap();
    conn.sync().unwrap();
    assert_eq!(p.foreground(), None);

    // EWMH wins over the input focus.
    set_active(d);
    focus(c);
    let w = p.foreground().expect("foreground via EWMH");
    assert_eq!((w.handle, w.title.as_str()), (u64::from(d), "other window"));

    // A stale or empty _NET_ACTIVE_WINDOW falls back to the focus path.
    conn.change_property8(PropMode::REPLACE, a, net_name, utf8, b"back to focus")
        .unwrap();
    conn.sync().unwrap();
    set_active(0x07ff_fffe);
    assert_eq!(
        p.foreground().map(|w| w.title),
        Some("back to focus".to_string())
    );
    set_active(0);
    assert_eq!(
        p.foreground().map(|w| w.title),
        Some("back to focus".to_string())
    );
    set_active(root);
    assert_eq!(
        p.foreground().map(|w| w.title),
        Some("back to focus".to_string())
    );

    // _NET_WM_STATE_HIDDEN marks a window minimised, and capture refuses it.
    let hidden = atom("_NET_WM_STATE_HIDDEN");
    let net_state = atom("_NET_WM_STATE");
    conn.change_property32(PropMode::REPLACE, d, net_state, AtomEnum::ATOM, &[hidden])
        .unwrap();
    conn.sync().unwrap();
    set_active(d);
    let w = p.foreground().expect("hidden window is still reported");
    assert!(w.minimized);
    assert!(matches!(p.capture(&w), Err(PlatformError::WindowGone(_))));

    // An unmapped window counts as minimised too, and a stale capture fails.
    conn.delete_property(d, net_state).unwrap();
    conn.unmap_window(d).unwrap();
    conn.sync().unwrap();
    let w = p.foreground().expect("unmapped window is still reported");
    assert!(w.minimized);
    conn.destroy_window(d).unwrap();
    conn.sync().unwrap();
    // The EWMH window is gone: falls back to the focus (A), no panic.
    assert_eq!(p.foreground().map(|w| w.handle), Some(u64::from(a)));
    assert!(p.capture(&w).is_err());

    // Focus on the root or on nothing: no window.
    set_active(0);
    conn.set_input_focus(InputFocus::NONE, 0u32, CURRENT_TIME)
        .unwrap();
    conn.sync().unwrap();
    assert_eq!(p.foreground(), None);
    conn.set_input_focus(InputFocus::POINTER_ROOT, 1u32, CURRENT_TIME)
        .unwrap();
    conn.sync().unwrap();
    assert_eq!(p.foreground(), None);
}

/// Without a usable X display the backend degrades to the null backend with
/// a reason; with both Wayland and X set it keeps X and says why windows may
/// be missing.
#[test]
fn x11_environment_fallbacks() {
    if !enabled() {
        return;
    }
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let saved_display = std::env::var_os("DISPLAY");
    let saved_wayland = std::env::var_os("WAYLAND_DISPLAY");
    let restore = || {
        match &saved_display {
            Some(v) => std::env::set_var("DISPLAY", v),
            None => std::env::remove_var("DISPLAY"),
        }
        match &saved_wayland {
            Some(v) => std::env::set_var("WAYLAND_DISPLAY", v),
            None => std::env::remove_var("WAYLAND_DISPLAY"),
        }
    };

    // Wayland only.
    std::env::set_var("WAYLAND_DISPLAY", "wayland-test");
    std::env::remove_var("DISPLAY");
    let p = create_platform();
    assert_eq!(p.name(), "null");
    let notes = p.capabilities().notes;
    assert!(
        notes
            .iter()
            .any(|n| n.contains("Wayland session without XWayland")),
        "{notes:?}"
    );

    // X display that does not exist.
    std::env::remove_var("WAYLAND_DISPLAY");
    std::env::set_var("DISPLAY", ":31337");
    let mut p = create_platform();
    assert_eq!(p.name(), "null");
    let notes = p.capabilities().notes;
    assert!(
        notes
            .iter()
            .any(|n| n.starts_with("cannot connect to X display")),
        "{notes:?}"
    );
    assert!(p.foreground().is_none() && p.idle_seconds().is_none());

    // No display at all.
    std::env::remove_var("DISPLAY");
    let p = create_platform();
    assert_eq!(p.name(), "null");
    assert!(p
        .capabilities()
        .notes
        .iter()
        .any(|n| n.contains("DISPLAY is not set")));

    // Both set: X is used, with a note about XWayland.
    restore();
    std::env::set_var("WAYLAND_DISPLAY", "wayland-test");
    if saved_display.is_some() {
        let p = create_platform();
        assert_eq!(p.name(), "x11");
        let notes = p.capabilities().notes;
        assert!(notes.iter().any(|n| n.contains("XWayland")), "{notes:?}");
    }
    restore();
}

/// Kill the X server under a live platform value, start another on the same
/// display, and check that the backend reconnects (no sooner than 5 s after
/// its previous attempt) instead of staying dead or panicking.
#[test]
fn x11_reconnects_after_the_server_restarts() {
    if !enabled() {
        return;
    }
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let Some(xvfb) = which("Xvfb") else {
        eprintln!("x11_e2e: reconnect test SKIPPED: Xvfb not on PATH");
        return;
    };
    let saved = std::env::var_os("DISPLAY");

    // A display number nobody is using.
    let Some(n) = (200..260u32).find(|n| {
        !Path::new(&format!("/tmp/.X11-unix/X{n}")).exists()
            && !Path::new(&format!("/tmp/.X{n}-lock")).exists()
    }) else {
        eprintln!("x11_e2e: reconnect test SKIPPED: no free display number");
        return;
    };
    let display = format!(":{n}");
    let start_server = || {
        let child = Command::new(&xvfb)
            .args([&display, "-screen", "0", "640x480x24", "-nolisten", "tcp"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn Xvfb");
        let child = KillOnDrop(child);
        wait_for("Xvfb socket", Duration::from_secs(10), || {
            std::os::unix::net::UnixStream::connect(format!("/tmp/.X11-unix/X{n}")).ok()
        });
        child
    };

    std::env::set_var("DISPLAY", &display);
    let server = start_server();
    let mut p = create_platform();
    let created = Instant::now();
    assert_eq!(p.name(), "x11", "{:?}", p.capabilities().notes);
    let has_idle = p.capabilities().idle;
    assert_eq!(p.idle_seconds().is_some(), has_idle);

    // The server dies: calls fail soft.
    drop(server);
    assert_eq!(p.idle_seconds(), None);
    assert_eq!(p.foreground(), None);
    assert!(p
        .capture(&WindowInfo {
            handle: 1,
            width: 100,
            height: 100,
            ..Default::default()
        })
        .is_err());

    // The server comes back; the backend must not hammer it, and must reconnect
    // once five seconds have passed since its last attempt.
    let _server2 = start_server();
    if created.elapsed() < Duration::from_millis(4500) {
        assert_eq!(
            p.idle_seconds(),
            None,
            "reconnected too early (rate limit is 5 s)"
        );
    }
    let remaining = Duration::from_millis(5200).saturating_sub(created.elapsed());
    std::thread::sleep(remaining);
    let got = p.idle_seconds();
    match saved {
        Some(v) => std::env::set_var("DISPLAY", v),
        None => std::env::remove_var("DISPLAY"),
    }
    if has_idle {
        assert!(
            got.is_some(),
            "backend did not reconnect after the server restarted"
        );
    }
}

fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(name))
        .find(|p| p.is_file())
}
