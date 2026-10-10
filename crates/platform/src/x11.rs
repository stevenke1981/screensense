//! Linux backend: the X11 protocol over a pure-Rust connection (`x11rb`'s
//! `RustConnection`, no libxcb).
//!
//! * foreground window: EWMH `_NET_ACTIVE_WINDOW`, falling back to the input
//!   focus climbed up to its top-level client when there is no window manager;
//! * idle time: the MIT-SCREEN-SAVER extension;
//! * capture: a core `GetImage` of the *root* window over the window's
//!   rectangle, so what is grabbed is what the user sees (child windows, GL
//!   content) and unmapped/obscured drawables cannot raise `BadMatch`;
//! * notifications: the `notify-send` binary, if installed.
//!
//! Every request can fail at any time (a window destroyed between two
//! queries is normal), so nothing here panics or unwraps: failures become
//! `None` / `Err`. If the connection itself breaks it is re-established, at
//! most once every five seconds.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use x11rb::connection::{Connection, RequestConnection};
use x11rb::errors::{ConnectionError, ReplyError, ReplyOrIdError};
use x11rb::protocol::screensaver::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ConnectionExt as _, GetPropertyReply, ImageFormat, ImageOrder, MapState, Window,
};
use x11rb::rust_connection::RustConnection;

use crate::{Capabilities, Image, NullPlatform, Platform, PlatformError, Result, WindowInfo};

/// Minimum gap between two attempts to (re)connect to the X server.
const RECONNECT_EVERY: Duration = Duration::from_secs(5);
/// Smallest window edge worth capturing.
const MIN_CAPTURE_EDGE: u32 = 40;
/// Longest property we read, in 32-bit words (titles are truncated beyond 8 KiB).
const MAX_PROP_WORDS: u32 = 2048;

/// The X11 backend, or a [`NullPlatform`] that says why it is not available.
pub fn create() -> Box<dyn Platform> {
    let wayland = env_set("WAYLAND_DISPLAY");
    let display = env_set("DISPLAY");
    if wayland && !display {
        return Box::new(NullPlatform::new(
            "Wayland session without XWayland: window observation is not available",
        ));
    }
    if !display {
        return Box::new(NullPlatform::new(
            "cannot connect to X display: DISPLAY is not set",
        ));
    }
    let mut notes = Vec::new();
    if wayland {
        notes.push(
            "Wayland session: only XWayland windows are visible; native Wayland windows are \
             not observed"
                .to_string(),
        );
    }
    match Conn::connect() {
        Ok(conn) => Box::new(X11Platform {
            idle_ext: conn.has_screensaver,
            conn: Some(conn),
            last_attempt: Some(Instant::now()),
            last_error: None,
            notes,
        }),
        Err(e) => Box::new(NullPlatform::new(format!(
            "cannot connect to X display: {e}"
        ))),
    }
}

fn env_set(name: &str) -> bool {
    std::env::var_os(name).is_some_and(|v| !v.is_empty())
}

/// Find an executable on `PATH` without a shell. Empty entries are skipped:
/// they mean "current directory" and we never want to run things from there.
pub(crate) fn find_in_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .filter(|d| !d.as_os_str().is_empty())
        .map(|d| d.join(name))
        .find(|p| is_executable(p))
}

fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Why an X operation failed.
#[derive(Debug)]
enum XErr {
    /// The connection is unusable; drop it and reconnect later.
    Broken(String),
    /// The server answered with an X protocol error (typically `BadWindow`).
    Request(String),
    /// A precondition on the target window failed.
    Gone(String),
    Unsupported(&'static str),
}

type XResult<T> = std::result::Result<T, XErr>;

impl From<ConnectionError> for XErr {
    fn from(e: ConnectionError) -> Self {
        match e {
            ConnectionError::IoError(_) | ConnectionError::FdPassingFailed => {
                XErr::Broken(e.to_string())
            }
            other => XErr::Request(other.to_string()),
        }
    }
}

impl From<ReplyError> for XErr {
    fn from(e: ReplyError) -> Self {
        match e {
            ReplyError::ConnectionError(c) => c.into(),
            ReplyError::X11Error(x) => XErr::Request(format!("X error {:?}", x.error_kind)),
        }
    }
}

impl From<ReplyOrIdError> for XErr {
    fn from(e: ReplyOrIdError) -> Self {
        match e {
            ReplyOrIdError::ConnectionError(c) => c.into(),
            ReplyOrIdError::X11Error(x) => XErr::Request(format!("X error {:?}", x.error_kind)),
            ReplyOrIdError::IdsExhausted => XErr::Request("X resource ids exhausted".to_string()),
        }
    }
}

// ---------------------------------------------------------------------------
// Connection state
// ---------------------------------------------------------------------------

struct Atoms {
    net_active_window: Atom,
    net_wm_name: Atom,
    utf8_string: Atom,
    net_wm_pid: Atom,
    net_wm_state: Atom,
    net_wm_state_hidden: Atom,
    wm_state: Atom,
}

struct VisualInfo {
    id: u32,
    masks: [u32; 3],
}

/// One live connection plus everything we need to know about the screen.
struct Conn {
    conn: RustConnection,
    root: Window,
    root_visual: u32,
    atoms: Atoms,
    msb_first: bool,
    /// `(depth, bits_per_pixel, scanline_pad)` from the connection setup.
    formats: Vec<(u8, u8, u8)>,
    visuals: Vec<VisualInfo>,
    has_screensaver: bool,
}

/// Properties of one window as raw bytes; `None` when the property is unset.
struct Prop {
    type_: Atom,
    format: u8,
    value: Vec<u8>,
}

impl Prop {
    fn from_reply(r: GetPropertyReply) -> Option<Prop> {
        if r.type_ == u32::from(AtomEnum::NONE) {
            None
        } else {
            Some(Prop {
                type_: r.type_,
                format: r.format,
                value: r.value,
            })
        }
    }

    fn first_u32(&self) -> Option<u32> {
        if self.format != 32 {
            return None;
        }
        self.value
            .get(..4)
            .map(|b| u32::from_ne_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn u32s(&self) -> Vec<u32> {
        if self.format != 32 {
            return Vec::new();
        }
        self.value
            .chunks_exact(4)
            .map(|b| u32::from_ne_bytes([b[0], b[1], b[2], b[3]]))
            .collect()
    }
}

impl Conn {
    fn connect() -> std::result::Result<Conn, String> {
        let (conn, screen_num) = x11rb::connect(None).map_err(|e| e.to_string())?;
        let (root, root_visual, formats, visuals, msb_first) = {
            let setup = conn.setup();
            let screen = setup
                .roots
                .get(screen_num)
                .ok_or_else(|| "X server has no screen".to_string())?;
            let formats = setup
                .pixmap_formats
                .iter()
                .map(|f| (f.depth, f.bits_per_pixel, f.scanline_pad))
                .collect::<Vec<_>>();
            let visuals = screen
                .allowed_depths
                .iter()
                .flat_map(|d| d.visuals.iter())
                .map(|v| VisualInfo {
                    id: v.visual_id,
                    masks: [v.red_mask, v.green_mask, v.blue_mask],
                })
                .collect::<Vec<_>>();
            (
                screen.root,
                screen.root_visual,
                formats,
                visuals,
                setup.image_byte_order == ImageOrder::MSB_FIRST,
            )
        };

        let atoms = intern_atoms(&conn).map_err(|e| format!("{e:?}"))?;

        // The extension is optional: no extension, no idle time.
        let has_screensaver = match conn.extension_information(screensaver::X11_EXTENSION_NAME) {
            Ok(Some(_)) => conn
                .screensaver_query_version(1, 1)
                .ok()
                .and_then(|c| c.reply().ok())
                .is_some(),
            _ => false,
        };

        Ok(Conn {
            conn,
            root,
            root_visual,
            atoms,
            msb_first,
            formats,
            visuals,
            has_screensaver,
        })
    }

    // -- properties ---------------------------------------------------------

    fn prop(
        &self,
        win: Window,
        prop: impl Into<Atom>,
        ty: impl Into<Atom>,
    ) -> XResult<Option<Prop>> {
        let r = self
            .conn
            .get_property(false, win, prop, ty, 0, MAX_PROP_WORDS)?
            .reply()?;
        Ok(Prop::from_reply(r))
    }

    /// Does the property exist at all (any type)? Cheap: asks for zero bytes.
    fn has_prop(&self, win: Window, prop: impl Into<Atom>) -> XResult<bool> {
        let r = self
            .conn
            .get_property(false, win, prop, AtomEnum::NONE, 0, 0)?
            .reply()?;
        Ok(r.type_ != u32::from(AtomEnum::NONE))
    }

    fn has_name(&self, win: Window) -> XResult<bool> {
        Ok(self.has_prop(win, self.atoms.net_wm_name)? || self.has_prop(win, AtomEnum::WM_NAME)?)
    }

    // -- foreground ---------------------------------------------------------

    fn active_window(&self) -> XResult<Option<Window>> {
        let p = self.prop(self.root, self.atoms.net_active_window, AtomEnum::WINDOW)?;
        Ok(p.and_then(|p| p.first_u32())
            .filter(|&w| w != 0 && w != self.root))
    }

    /// The top-level client that owns the input focus, for sessions without
    /// an EWMH window manager.
    fn focus_toplevel(&self) -> XResult<Option<Window>> {
        let focus = self.conn.get_input_focus()?.reply()?.focus;
        // 0 = None, 1 = PointerRoot.
        if focus == 0 || focus == 1 || focus == self.root {
            return Ok(None);
        }
        // The focus window and its ancestors, nearest first, excluding the root.
        let mut chain = vec![focus];
        let mut cur = focus;
        for _ in 0..32 {
            let parent = self.conn.query_tree(cur)?.reply()?.parent;
            if parent == 0 || parent == self.root {
                break;
            }
            chain.push(parent);
            cur = parent;
        }
        for &w in &chain {
            if self.has_prop(w, self.atoms.wm_state)? {
                return Ok(Some(w));
            }
        }
        for &w in &chain {
            if self.has_name(w)? {
                return Ok(Some(w));
            }
        }
        Ok(None)
    }

    fn foreground(&self) -> XResult<Option<WindowInfo>> {
        // Errors on the EWMH candidate other than a broken connection are
        // treated as "stale property": try the input focus instead.
        if let Some(w) = self.active_window()? {
            match self.window_info(w) {
                Ok(info) => return Ok(Some(info)),
                Err(e @ XErr::Broken(_)) => return Err(e),
                Err(_) => {}
            }
        }
        match self.focus_toplevel()? {
            Some(w) => self.window_info(w).map(Some),
            None => Ok(None),
        }
    }

    fn window_info(&self, win: Window) -> XResult<WindowInfo> {
        let c = &self.conn;
        let a = &self.atoms;
        // Pipeline every request, then collect the replies.
        let net_name =
            c.get_property(false, win, a.net_wm_name, a.utf8_string, 0, MAX_PROP_WORDS)?;
        let wm_name = c.get_property(
            false,
            win,
            AtomEnum::WM_NAME,
            AtomEnum::NONE,
            0,
            MAX_PROP_WORDS,
        )?;
        let wm_class = c.get_property(false, win, AtomEnum::WM_CLASS, AtomEnum::STRING, 0, 256)?;
        let pid = c.get_property(false, win, a.net_wm_pid, AtomEnum::CARDINAL, 0, 1)?;
        let state = c.get_property(false, win, a.net_wm_state, AtomEnum::ATOM, 0, 64)?;
        let geo = c.get_geometry(win)?;
        let attrs = c.get_window_attributes(win)?;
        let origin = c.translate_coordinates(win, self.root, 0, 0)?;

        let geo = geo.reply()?;
        let attrs = attrs.reply()?;
        let origin = origin.reply()?;
        let net_name = Prop::from_reply(net_name.reply()?);
        let wm_name = Prop::from_reply(wm_name.reply()?);
        let wm_class = Prop::from_reply(wm_class.reply()?);
        let pid = Prop::from_reply(pid.reply()?)
            .and_then(|p| p.first_u32())
            .unwrap_or(0);
        let state = Prop::from_reply(state.reply()?)
            .map(|p| p.u32s())
            .unwrap_or_default();

        let title = net_name
            .filter(|p| !p.value.is_empty())
            .map(|p| decode_text(p.type_, &p.value, a.utf8_string))
            .or_else(|| wm_name.map(|p| decode_text(p.type_, &p.value, a.utf8_string)))
            .unwrap_or_default();
        let (instance, class) = wm_class
            .map(|p| parse_wm_class(&p.value))
            .unwrap_or_default();
        let app = app_name(pid, &instance, &class);
        let hidden = state.contains(&a.net_wm_state_hidden);

        Ok(WindowInfo {
            handle: u64::from(win),
            pid,
            app,
            title,
            class,
            x: i32::from(origin.dst_x),
            y: i32::from(origin.dst_y),
            width: u32::from(geo.width),
            height: u32::from(geo.height),
            minimized: hidden || attrs.map_state != MapState::VIEWABLE,
        })
    }

    // -- idle ---------------------------------------------------------------

    fn idle_seconds(&self) -> XResult<Option<f64>> {
        if !self.has_screensaver {
            return Ok(None);
        }
        let r = self.conn.screensaver_query_info(self.root)?.reply()?;
        Ok(Some(f64::from(r.ms_since_user_input) / 1000.0))
    }

    // -- capture ------------------------------------------------------------

    fn capture(&self, win: &WindowInfo) -> XResult<Image> {
        if win.minimized || win.width < MIN_CAPTURE_EDGE || win.height < MIN_CAPTURE_EDGE {
            return Err(XErr::Gone("window is minimised or too small".to_string()));
        }
        let handle = u32::try_from(win.handle)
            .map_err(|_| XErr::Gone("not an X11 window id".to_string()))?;

        // The window may have been closed, unmapped, moved or resized since
        // `foreground()` saw it: ask again, since a root grab of a stale
        // rectangle would happily return whatever is there now.
        let attrs = self.conn.get_window_attributes(handle)?;
        let geo = self.conn.get_geometry(handle)?;
        let origin = self.conn.translate_coordinates(handle, self.root, 0, 0)?;
        let root_geo = self.conn.get_geometry(self.root)?;
        let attrs = attrs.reply()?;
        let geo = geo.reply()?;
        let origin = origin.reply()?;
        let root_geo = root_geo.reply()?;
        if attrs.map_state != MapState::VIEWABLE {
            return Err(XErr::Gone("window is not viewable".to_string()));
        }

        let x0 = i32::from(origin.dst_x).max(0);
        let y0 = i32::from(origin.dst_y).max(0);
        let x1 = (i32::from(origin.dst_x) + i32::from(geo.width)).min(i32::from(root_geo.width));
        let y1 = (i32::from(origin.dst_y) + i32::from(geo.height)).min(i32::from(root_geo.height));
        let (w, h) = (x1 - x0, y1 - y0);
        if w < MIN_CAPTURE_EDGE as i32 || h < MIN_CAPTURE_EDGE as i32 {
            return Err(XErr::Gone("window is off screen".to_string()));
        }
        let (w, h) = (w as u16, h as u16);

        let reply = self
            .conn
            .get_image(
                ImageFormat::Z_PIXMAP,
                self.root,
                x0 as i16,
                y0 as i16,
                w,
                h,
                !0,
            )?
            .reply()?;
        if reply.depth < 24 {
            return Err(XErr::Unsupported(
                "capture needs a screen depth of at least 24 bits",
            ));
        }
        let &(_, bpp, pad) = self
            .formats
            .iter()
            .find(|f| f.0 == reply.depth)
            .ok_or(XErr::Unsupported("no pixmap format for the screen depth"))?;
        let masks = self
            .visuals
            .iter()
            .find(|v| v.id == reply.visual)
            .or_else(|| self.visuals.iter().find(|v| v.id == self.root_visual))
            .map(|v| v.masks)
            .ok_or(XErr::Unsupported("screen visual is unknown"))?;
        let rgba = zpixmap_to_rgba(
            &reply.data,
            usize::from(w),
            usize::from(h),
            PixelLayout {
                bpp,
                pad,
                msb_first: self.msb_first,
                masks,
            },
        )
        .map_err(|_| XErr::Unsupported("screen pixel format is not a 24/32-bit true colour"))?;
        Ok(Image {
            width: u32::from(w),
            height: u32::from(h),
            rgba,
        })
    }
}

fn intern_atoms(conn: &RustConnection) -> XResult<Atoms> {
    const NAMES: [&str; 6] = [
        "_NET_ACTIVE_WINDOW",
        "_NET_WM_NAME",
        "UTF8_STRING",
        "_NET_WM_PID",
        "_NET_WM_STATE",
        "_NET_WM_STATE_HIDDEN",
    ];
    let mut cookies = Vec::with_capacity(NAMES.len() + 1);
    for n in NAMES.iter().chain(std::iter::once(&"WM_STATE")) {
        cookies.push(conn.intern_atom(false, n.as_bytes())?);
    }
    let mut ids = Vec::with_capacity(cookies.len());
    for c in cookies {
        ids.push(c.reply()?.atom);
    }
    match ids.as_slice() {
        &[active, name, utf8, pid, state, hidden, wm_state] => Ok(Atoms {
            net_active_window: active,
            net_wm_name: name,
            utf8_string: utf8,
            net_wm_pid: pid,
            net_wm_state: state,
            net_wm_state_hidden: hidden,
            wm_state,
        }),
        _ => Err(XErr::Request("unexpected number of atoms".to_string())),
    }
}

// ---------------------------------------------------------------------------
// Pure helpers (unit tested)
// ---------------------------------------------------------------------------

/// Decode a window-name property. UTF8_STRING is UTF-8; STRING is Latin-1;
/// anything else (COMPOUND_TEXT, ...) is tried as UTF-8 and falls back to
/// Latin-1. Lossy by design.
fn decode_text(ty: Atom, bytes: &[u8], utf8_atom: Atom) -> String {
    let bytes = {
        let mut end = bytes.len();
        while end > 0 && bytes[end - 1] == 0 {
            end -= 1;
        }
        &bytes[..end]
    };
    if ty == utf8_atom {
        return String::from_utf8_lossy(bytes).into_owned();
    }
    if ty != u32::from(AtomEnum::STRING) {
        if let Ok(s) = std::str::from_utf8(bytes) {
            return s.to_string();
        }
    }
    bytes.iter().map(|&b| char::from(b)).collect()
}

/// `WM_CLASS` is `instance\0class\0`. Returns `(instance, class)`.
fn parse_wm_class(bytes: &[u8]) -> (String, String) {
    let mut parts = bytes.split(|&b| b == 0);
    let instance = parts
        .next()
        .map(|p| String::from_utf8_lossy(p).into_owned());
    let class = parts
        .next()
        .map(|p| String::from_utf8_lossy(p).into_owned());
    (instance.unwrap_or_default(), class.unwrap_or_default())
}

/// Process name for a window: `/proc/<pid>/comm`, else the `WM_CLASS`
/// instance, else its class, else `unknown`.
fn app_name(pid: u32, instance: &str, class: &str) -> String {
    if pid != 0 {
        if let Ok(s) = std::fs::read_to_string(format!("/proc/{pid}/comm")) {
            let name = s.trim();
            if !name.is_empty() {
                return name.to_string();
            }
        }
    }
    if !instance.is_empty() {
        instance.to_string()
    } else if !class.is_empty() {
        class.to_string()
    } else {
        "unknown".to_string()
    }
}

/// How the server lays out a ZPixmap.
#[derive(Clone, Copy, Debug)]
struct PixelLayout {
    bpp: u8,
    pad: u8,
    msb_first: bool,
    /// Red, green and blue masks of the visual.
    masks: [u32; 3],
}

/// `(shift, bits)` of a contiguous channel mask.
fn channel(mask: u32) -> Option<(u32, u32)> {
    if mask == 0 {
        return None;
    }
    let shift = mask.trailing_zeros();
    let bits = (mask >> shift).trailing_ones();
    let rebuilt = (((1u64 << bits) - 1) << shift) as u32;
    (rebuilt == mask && bits <= 16).then_some((shift, bits))
}

/// Scale a `bits`-wide channel value to 8 bits.
fn to_u8(v: u32, bits: u32) -> u8 {
    let v = match bits {
        8 => v,
        b if b > 8 => v >> (b - 8),
        b => v * 255 / ((1 << b) - 1),
    };
    v.min(255) as u8
}

/// Convert server ZPixmap bytes to RGBA8 (alpha 255), honouring the visual's
/// masks, the pixel size, the row padding and the image byte order.
fn zpixmap_to_rgba(
    data: &[u8],
    w: usize,
    h: usize,
    fmt: PixelLayout,
) -> std::result::Result<Vec<u8>, String> {
    let bytes_pp = match fmt.bpp {
        24 => 3,
        32 => 4,
        other => return Err(format!("unsupported bits per pixel: {other}")),
    };
    let pad = usize::from(fmt.pad).max(8);
    let stride = (w * usize::from(fmt.bpp)).div_ceil(pad) * pad / 8;
    if data.len() < stride * h {
        return Err(format!("short image: {} bytes for {w}x{h}", data.len()));
    }
    let [r, g, b] = fmt.masks;
    let (Some(r), Some(g), Some(b)) = (channel(r), channel(g), channel(b)) else {
        return Err("visual is not true colour".to_string());
    };
    let mut out = Vec::with_capacity(w * h * 4);
    for row in 0..h {
        let line = &data[row * stride..row * stride + w * bytes_pp];
        for px in line.chunks_exact(bytes_pp) {
            let mut v = 0u32;
            if fmt.msb_first {
                for &byte in px {
                    v = (v << 8) | u32::from(byte);
                }
            } else {
                for &byte in px.iter().rev() {
                    v = (v << 8) | u32::from(byte);
                }
            }
            let mask_of = |(shift, bits): (u32, u32)| (v >> shift) & ((1 << bits) - 1);
            out.push(to_u8(mask_of(r), r.1));
            out.push(to_u8(mask_of(g), g.1));
            out.push(to_u8(mask_of(b), b.1));
            out.push(255);
        }
    }
    Ok(out)
}

/// Run `notify-send -- <title> <body>` and reap it in the background.
fn spawn_notify(exe: &Path, title: &str, body: &str) -> Result<()> {
    let mut child = Command::new(exe)
        .arg("--")
        .arg(title)
        .arg(body)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| PlatformError::Os(format!("cannot run notify-send: {e}")))?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

// ---------------------------------------------------------------------------
// Platform
// ---------------------------------------------------------------------------

struct X11Platform {
    conn: Option<Conn>,
    last_attempt: Option<Instant>,
    last_error: Option<String>,
    idle_ext: bool,
    notes: Vec<String>,
}

impl X11Platform {
    /// Make sure a connection exists, reconnecting at most every 5 seconds.
    fn ensure(&mut self) -> XResult<()> {
        if self.conn.is_some() {
            return Ok(());
        }
        if self
            .last_attempt
            .is_some_and(|t| t.elapsed() < RECONNECT_EVERY)
        {
            return Err(XErr::Broken("not connected to the X server".to_string()));
        }
        self.last_attempt = Some(Instant::now());
        match Conn::connect() {
            Ok(c) => {
                self.idle_ext = c.has_screensaver;
                self.last_error = None;
                self.conn = Some(c);
                Ok(())
            }
            Err(e) => {
                self.last_error = Some(e.clone());
                Err(XErr::Broken(e))
            }
        }
    }

    /// Run `f` against the connection; a broken connection is dropped.
    fn run<T>(&mut self, f: impl FnOnce(&Conn) -> XResult<T>) -> XResult<T> {
        self.ensure()?;
        let r = match self.conn.as_ref() {
            Some(c) => {
                // Nothing selects events, but never let stray ones pile up.
                while let Ok(Some(_)) = c.conn.poll_for_event() {}
                f(c)
            }
            None => Err(XErr::Broken("not connected to the X server".to_string())),
        };
        if let Err(XErr::Broken(msg)) = &r {
            self.last_error = Some(msg.clone());
            self.conn = None;
        }
        r
    }
}

impl Platform for X11Platform {
    fn name(&self) -> &'static str {
        "x11"
    }

    fn capabilities(&self) -> Capabilities {
        let notify = find_in_path("notify-send").is_some();
        let mut notes = self.notes.clone();
        if let (None, Some(e)) = (&self.conn, &self.last_error) {
            notes.push(format!("X connection lost: {e}"));
        }
        if !self.idle_ext {
            notes.push("MIT-SCREEN-SAVER extension missing: idle time is unavailable".to_string());
        }
        if !notify {
            notes.push("notify-send not found: desktop notifications are unavailable".to_string());
        }
        notes.push("X11 cannot tell whether a browser window is private".to_string());
        Capabilities {
            foreground: true,
            capture: true,
            idle: self.idle_ext,
            private_probe: false,
            notify,
            notes,
        }
    }

    fn foreground(&mut self) -> Option<WindowInfo> {
        self.run(|c| c.foreground()).ok().flatten()
    }

    fn idle_seconds(&mut self) -> Option<f64> {
        self.run(|c| c.idle_seconds()).ok().flatten()
    }

    fn capture(&mut self, win: &WindowInfo) -> Result<Image> {
        self.run(|c| c.capture(win)).map_err(|e| match e {
            XErr::Broken(m) => PlatformError::Os(m),
            XErr::Request(m) | XErr::Gone(m) => PlatformError::WindowGone(m),
            XErr::Unsupported(m) => PlatformError::Unsupported(m),
        })
    }

    fn is_private_browsing(&mut self, _win: &WindowInfo) -> Option<bool> {
        None
    }

    fn notify(&mut self, title: &str, body: &str) -> Result<()> {
        match find_in_path("notify-send") {
            Some(exe) => spawn_notify(&exe, title, body),
            None => Err(PlatformError::Unsupported("notify-send not found")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout(bpp: u8, msb_first: bool, masks: [u32; 3]) -> PixelLayout {
        PixelLayout {
            bpp,
            pad: 32,
            msb_first,
            masks,
        }
    }

    const XRGB: [u32; 3] = [0xff0000, 0x00ff00, 0x0000ff];

    #[test]
    fn little_endian_xrgb_is_bgrx_in_memory() {
        // Pixel 0xXXRRGGBB, little endian: bytes B G R X.
        let data = [0x30, 0x20, 0x10, 0x00, 0xff, 0xff, 0xff, 0x00];
        let out = zpixmap_to_rgba(&data, 2, 1, layout(32, false, XRGB)).unwrap();
        assert_eq!(out, vec![0x10, 0x20, 0x30, 255, 255, 255, 255, 255]);
    }

    #[test]
    fn big_endian_xrgb_is_xrgb_in_memory() {
        let data = [0x00, 0x10, 0x20, 0x30];
        let out = zpixmap_to_rgba(&data, 1, 1, layout(32, true, XRGB)).unwrap();
        assert_eq!(out, vec![0x10, 0x20, 0x30, 255]);
    }

    #[test]
    fn bgr_masks_swap_channels() {
        let data = [0x30, 0x20, 0x10, 0x00];
        let out = zpixmap_to_rgba(
            &data,
            1,
            1,
            layout(32, false, [0x0000ff, 0x00ff00, 0xff0000]),
        )
        .unwrap();
        assert_eq!(out, vec![0x30, 0x20, 0x10, 255]);
    }

    #[test]
    fn packed_24_bit_pixels_with_row_padding() {
        // 3x2 image, 24 bpp, pad 32 => stride 12 bytes (9 used).
        let mut data = Vec::new();
        for row in 0..2u8 {
            for col in 0..3u8 {
                // little endian: B G R
                data.extend_from_slice(&[col, 10 + row, 20]);
            }
            data.extend_from_slice(&[0xee, 0xee, 0xee]); // padding
        }
        let out = zpixmap_to_rgba(&data, 3, 2, layout(24, false, XRGB)).unwrap();
        assert_eq!(out.len(), 3 * 2 * 4);
        // Pixel (col 2, row 1): R = 20, G = 11, B = 2.
        assert_eq!(&out[(3 + 2) * 4..(3 + 2) * 4 + 4], &[20, 11, 2, 255]);
    }

    #[test]
    fn ten_bit_channels_scale_down() {
        // depth 30: xRGB 2:10:10:10.
        let masks = [0x3ff0_0000, 0x000f_fc00, 0x0000_03ff];
        let px: u32 = (0x3ff << 20) | (0x200 << 10);
        let out = zpixmap_to_rgba(&px.to_le_bytes(), 1, 1, layout(32, false, masks)).unwrap();
        assert_eq!(out, vec![255, 128, 0, 255]);
    }

    #[test]
    fn rgb565_masks_scale_up() {
        // Not reachable through capture() (depth 16 is rejected) but the maths
        // for narrow channels must still be right.
        assert_eq!(to_u8(31, 5), 255);
        assert_eq!(to_u8(0, 5), 0);
        assert_eq!(to_u8(63, 6), 255);
    }

    #[test]
    fn short_or_unsupported_data_is_an_error_not_a_panic() {
        assert!(zpixmap_to_rgba(&[0; 3], 2, 1, layout(32, false, XRGB)).is_err());
        assert!(zpixmap_to_rgba(&[0; 8], 2, 1, layout(16, false, XRGB)).is_err());
        assert!(zpixmap_to_rgba(&[0; 8], 2, 1, layout(32, false, [0, 0, 0])).is_err());
        assert!(zpixmap_to_rgba(&[0; 8], 2, 1, layout(32, false, [0xf0f0, 0xff00, 0xff])).is_err());
    }

    #[test]
    fn wm_class_splits_instance_and_class() {
        assert_eq!(
            parse_wm_class(b"gnome-terminal-server\0Gnome-terminal\0"),
            (
                "gnome-terminal-server".to_string(),
                "Gnome-terminal".to_string()
            )
        );
        assert_eq!(parse_wm_class(b""), (String::new(), String::new()));
        assert_eq!(parse_wm_class(b"only"), ("only".to_string(), String::new()));
    }

    #[test]
    fn titles_decode_by_type() {
        let utf8 = 500;
        let string = u32::from(AtomEnum::STRING);
        assert_eq!(
            decode_text(utf8, "héllo 你好".as_bytes(), utf8),
            "héllo 你好"
        );
        assert_eq!(decode_text(string, &[0x68, 0xe9, 0x00], utf8), "hé");
        // Unknown type (COMPOUND_TEXT): valid UTF-8 is kept, otherwise Latin-1.
        assert_eq!(decode_text(77, "é".as_bytes(), utf8), "é");
        assert_eq!(decode_text(77, &[0xe9], utf8), "é");
        // Invalid UTF-8 in a UTF8_STRING is replaced, not an error.
        assert_eq!(decode_text(utf8, &[b'a', 0xff, b'b'], utf8), "a\u{fffd}b");
    }

    #[test]
    fn app_name_prefers_proc_then_class() {
        let me = std::process::id();
        let comm = std::fs::read_to_string(format!("/proc/{me}/comm")).unwrap();
        assert_eq!(app_name(me, "inst", "Cls"), comm.trim());
        assert_eq!(app_name(0, "inst", "Cls"), "inst");
        assert_eq!(app_name(0, "", "Cls"), "Cls");
        assert_eq!(app_name(0, "", ""), "unknown");
        // A pid that cannot exist.
        assert_eq!(app_name(u32::MAX, "", "Cls"), "Cls");
    }

    #[test]
    fn find_in_path_finds_sh_and_misses_nonsense() {
        assert!(find_in_path("sh").is_some());
        assert!(find_in_path("definitely-not-a-real-binary-xyz").is_none());
    }

    #[test]
    fn notify_passes_arguments_without_a_shell() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("ss-notify-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("args.txt");
        let script = dir.join("notify-send");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nfor a in \"$@\"; do printf '%s\\n' \"$a\"; done > '{}'\n",
                out.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let title = "-title; touch /tmp/pwned";
        let body = "$(id) `id` \"quoted\"";
        spawn_notify(&script, title, body).unwrap();
        let mut got = String::new();
        for _ in 0..100 {
            if let Ok(s) = std::fs::read_to_string(&out) {
                if s.lines().count() >= 3 {
                    got = s;
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(got, format!("--\n{title}\n{body}\n"));
    }

    #[test]
    fn unrunnable_notify_binary_is_an_os_error() {
        let r = spawn_notify(Path::new("/nonexistent/notify-send"), "t", "b");
        assert!(matches!(r, Err(PlatformError::Os(_))));
    }
}
