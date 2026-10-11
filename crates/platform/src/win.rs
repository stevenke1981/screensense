//! Windows backend: foreground window, idle time, window capture, private
//! browsing probe and toast notifications.
//!
//! Threading: a [`WinPlatform`] holds only plain data (window handles are
//! stored as integers, no COM pointers), so it is `Send`. COM/WinRT objects
//! are created per call on the calling thread, after `win_misc::ensure_com`.

use std::collections::HashMap;
use std::ffi::c_void;
use std::mem::size_of;
use std::time::{Duration, Instant};

use windows::core::{BOOL, HSTRING, PWSTR};
use windows::Data::Xml::Dom::XmlDocument;
use windows::Win32::Foundation::{HWND, LPARAM, RECT};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_EXTENDED_FRAME_BOUNDS};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetDIBits, GetWindowDC,
    ReleaseDC, SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HBITMAP, HDC,
    HGDIOBJ,
};
use windows::Win32::Storage::Xps::{PrintWindow, PRINT_WINDOW_FLAGS};
use windows::Win32::System::SystemInformation::GetTickCount;
use windows::Win32::System::Threading::{QueryFullProcessImageNameW, PROCESS_NAME_WIN32};
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumChildWindows, GetClassNameW, GetForegroundWindow, GetWindowRect, GetWindowTextLengthW,
    GetWindowTextW, GetWindowThreadProcessId, IsHungAppWindow, IsIconic, IsWindow,
};
use windows::UI::Notifications::{ToastNotification, ToastNotificationManager};

use crate::win_misc::{ensure_com, open_process_limited};
use crate::{Capabilities, Image, Platform, PlatformError, Result, WindowInfo};

/// `PW_RENDERFULLCONTENT`: ask DWM to render the full window content
/// (needed for Chromium/Electron/UWP windows). The windows crate does not
/// export the constant.
const PW_RENDERFULLCONTENT: PRINT_WINDOW_FLAGS = PRINT_WINDOW_FLAGS(0x0000_0002);

/// Windows below this size (either side) are not worth reading.
const MIN_SIDE: i32 = 40;
/// Refuse absurd windows (a 320 MB RGBA buffer is the ceiling).
const MAX_PIXELS: i64 = 80_000_000;

const PROCESS_NAME_TTL: Duration = Duration::from_secs(30);
const PROCESS_CACHE_SOFT_CAP: usize = 512;
const UNKNOWN_APP: &str = "unknown";
const FRAME_HOST: &str = "ApplicationFrameHost.exe";

const PRIVATE_TRUE_TTL: Duration = Duration::from_secs(30 * 60);
const PRIVATE_FALSE_TTL: Duration = Duration::from_secs(60);
const PRIVATE_NONE_TTL: Duration = Duration::from_secs(10);
const PRIVATE_CACHE_CAP: usize = 256;

/// The toast is attributed to Windows PowerShell: an unpackaged exe has no
/// AppUserModelID of its own (that needs a Start-menu shortcut that carries
/// one), and borrowing PowerShell's well-known AUMID is the common trick for
/// getting a toast out of a plain exe. The user will see "Windows PowerShell"
/// as the sender of the notification.
const TOAST_AUMID: &str =
    r"{1AC14E77-02E7-4E5D-B744-2EB1AE5198B7}\WindowsPowerShell\v1.0\powershell.exe";

pub fn create() -> Box<dyn Platform> {
    // Process-wide, and fails if the awareness was already set (by a manifest
    // or an earlier call): ignoring that is right, whatever is set stays.
    // SAFETY: plain value argument.
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    Box::new(WinPlatform::new())
}

struct WinPlatform {
    /// pid -> (expires, executable file name). Pids are reused, hence the TTL.
    process_names: HashMap<u32, (Instant, String)>,
    private_cache: PrivateCache,
}

impl WinPlatform {
    fn new() -> Self {
        WinPlatform {
            process_names: HashMap::new(),
            private_cache: PrivateCache::default(),
        }
    }

    fn app_name(&mut self, pid: u32) -> String {
        let now = Instant::now();
        if let Some((expires, name)) = self.process_names.get(&pid) {
            if *expires > now {
                return name.clone();
            }
        }
        let name = query_process_name(pid).unwrap_or_else(|| UNKNOWN_APP.to_string());
        if self.process_names.len() >= PROCESS_CACHE_SOFT_CAP {
            self.process_names.retain(|_, (expires, _)| *expires > now);
            if self.process_names.len() >= PROCESS_CACHE_SOFT_CAP {
                self.process_names.clear();
            }
        }
        self.process_names
            .insert(pid, (now + PROCESS_NAME_TTL, name.clone()));
        name
    }
}

impl Platform for WinPlatform {
    fn name(&self) -> &'static str {
        "windows"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            foreground: true,
            capture: true,
            idle: true,
            private_probe: true,
            notify: true,
            notes: Vec::new(),
        }
    }

    fn foreground(&mut self) -> Option<WindowInfo> {
        // SAFETY: no arguments.
        let hwnd = unsafe { GetForegroundWindow() };
        if hwnd.is_invalid() {
            return None;
        }

        let mut pid = 0u32;
        // SAFETY: `pid` is a valid out pointer; a stale hwnd just yields 0.
        unsafe {
            GetWindowThreadProcessId(hwnd, Some(&mut pid));
        }

        let mut app = if pid == 0 {
            UNKNOWN_APP.to_string()
        } else {
            self.app_name(pid)
        };
        if app.eq_ignore_ascii_case(FRAME_HOST) {
            // A UWP window is hosted by ApplicationFrameHost.exe; the real
            // app owns a child window in another process.
            if let Some(real_pid) = uwp_child_pid(hwnd, pid) {
                app = self.app_name(real_pid);
            }
        }

        let rect = frame_rect(hwnd).unwrap_or_default();
        Some(WindowInfo {
            handle: hwnd_to_handle(hwnd),
            pid,
            app,
            title: window_title(hwnd),
            class: window_class(hwnd),
            x: rect.left,
            y: rect.top,
            width: u32::try_from(rect.right.saturating_sub(rect.left)).unwrap_or(0),
            height: u32::try_from(rect.bottom.saturating_sub(rect.top)).unwrap_or(0),
            // SAFETY: IsIconic accepts any handle value.
            minimized: unsafe { IsIconic(hwnd) }.as_bool(),
        })
    }

    fn idle_seconds(&mut self) -> Option<f64> {
        let mut info = LASTINPUTINFO {
            cbSize: size_of::<LASTINPUTINFO>() as u32,
            dwTime: 0,
        };
        // SAFETY: `info` is a valid LASTINPUTINFO with cbSize set.
        if !unsafe { GetLastInputInfo(&mut info) }.as_bool() {
            return None;
        }
        // SAFETY: no arguments.
        let now = unsafe { GetTickCount() };
        Some(f64::from(idle_ms(now, info.dwTime)) / 1000.0)
    }

    fn capture(&mut self, win: &WindowInfo) -> Result<Image> {
        let hwnd = handle_to_hwnd(win.handle);
        // SAFETY: these predicates accept any handle value.
        unsafe {
            if !IsWindow(Some(hwnd)).as_bool() {
                return Err(PlatformError::WindowGone("window no longer exists".into()));
            }
            if IsIconic(hwnd).as_bool() {
                return Err(PlatformError::WindowGone("window is minimised".into()));
            }
            // PrintWindow sends WM_PRINT and would block on a hung window.
            if IsHungAppWindow(hwnd).as_bool() {
                return Err(PlatformError::WindowGone("window is not responding".into()));
            }
        }
        let window = window_rect(hwnd)
            .ok_or_else(|| PlatformError::WindowGone("cannot read window rectangle".into()))?;
        let frame = frame_rect(hwnd).unwrap_or(window);
        let plan =
            plan_capture(window, frame).map_err(|why| PlatformError::WindowGone(why.into()))?;

        // PrintWindow addresses only this HWND, never the desktop DC.
        match render(hwnd, &plan) {
            Ok(bgra) => {
                if let Some(rgba) = bgra_to_rgba_crop(&bgra, &plan) {
                    if !is_uniform(&rgba) {
                        return Ok(image_from(&plan, rgba));
                    }
                }
            }
            Err(RenderError::PrintFailed) => {}
            Err(RenderError::Os(msg)) => return Err(PlatformError::Os(msg)),
        }

        Err(PlatformError::WindowGone(
            "PrintWindow gave no usable frame; desktop-copy fallback is disabled for privacy"
                .into(),
        ))
    }

    fn is_private_browsing(&mut self, win: &WindowInfo) -> Option<bool> {
        let key = (win.handle, win.pid);
        let now = Instant::now();
        if let Some(cached) = self.private_cache.get(key, now) {
            return cached;
        }
        let hwnd = handle_to_hwnd(win.handle);
        // UIA calls into the browser process; do not wait on a hung one.
        // SAFETY: IsHungAppWindow accepts any handle value.
        let answer = if unsafe { IsHungAppWindow(hwnd) }.as_bool() {
            None
        } else {
            ensure_com();
            crate::win_uia::probe_private(hwnd_to_isize(hwnd))
        };
        self.private_cache.put(key, answer, Instant::now());
        answer
    }

    fn notify(&mut self, title: &str, body: &str) -> Result<()> {
        ensure_com();
        let os = |what: &'static str| {
            move |e: windows::core::Error| PlatformError::Os(format!("toast: {what}: {e}"))
        };
        let doc = XmlDocument::new().map_err(os("XmlDocument"))?;
        doc.LoadXml(&HSTRING::from(toast_xml(title, body)))
            .map_err(os("LoadXml"))?;
        let toast = ToastNotification::CreateToastNotification(&doc)
            .map_err(os("CreateToastNotification"))?;
        let notifier =
            ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(TOAST_AUMID))
                .map_err(os("CreateToastNotifierWithId"))?;
        notifier.Show(&toast).map_err(os("Show"))
    }
}

// ---------------------------------------------------------------------------
// Window queries
// ---------------------------------------------------------------------------

fn hwnd_to_isize(hwnd: HWND) -> isize {
    hwnd.0 as isize
}

fn hwnd_to_handle(hwnd: HWND) -> u64 {
    hwnd_to_isize(hwnd) as u64
}

fn handle_to_hwnd(handle: u64) -> HWND {
    HWND(handle as usize as *mut c_void)
}

fn window_title(hwnd: HWND) -> String {
    // SAFETY: accepts any handle value.
    let len = unsafe { GetWindowTextLengthW(hwnd) };
    let Ok(len) = usize::try_from(len) else {
        return String::new();
    };
    if len == 0 {
        return String::new();
    }
    // The title can change between the two calls: the buffer is re-checked
    // against what GetWindowTextW reports it copied.
    let mut buf = vec![0u16; len.saturating_add(1).min(32_768)];
    // SAFETY: `buf` is a valid writable slice; the API writes at most buf.len() units.
    let copied = unsafe { GetWindowTextW(hwnd, &mut buf) };
    let copied = usize::try_from(copied).unwrap_or(0).min(buf.len());
    String::from_utf16_lossy(buf.get(..copied).unwrap_or(&[]))
}

fn window_class(hwnd: HWND) -> String {
    let mut buf = [0u16; 256];
    // SAFETY: `buf` is a valid writable slice.
    let copied = unsafe { GetClassNameW(hwnd, &mut buf) };
    let copied = usize::try_from(copied).unwrap_or(0).min(buf.len());
    String::from_utf16_lossy(buf.get(..copied).unwrap_or(&[]))
}

fn window_rect(hwnd: HWND) -> Option<RECT> {
    let mut rect = RECT::default();
    // SAFETY: `rect` is a valid out pointer.
    unsafe { GetWindowRect(hwnd, &mut rect) }.ok()?;
    Some(rect)
}

/// The visible bounds (without the invisible resize border and shadow that
/// `GetWindowRect` includes on Windows 10/11), falling back to `GetWindowRect`.
fn frame_rect(hwnd: HWND) -> Option<RECT> {
    let mut rect = RECT::default();
    // SAFETY: `rect` is a valid out pointer of exactly the size passed.
    let dwm = unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            (&mut rect as *mut RECT).cast::<c_void>(),
            size_of::<RECT>() as u32,
        )
    };
    if dwm.is_ok() && rect.right > rect.left && rect.bottom > rect.top {
        return Some(rect);
    }
    window_rect(hwnd)
}

/// File name of the process image, `None` if the process cannot be opened
/// (typically an elevated process while we are not elevated).
fn query_process_name(pid: u32) -> Option<String> {
    if pid == 0 {
        return None;
    }
    let handle = open_process_limited(pid).ok()?;
    let mut buf = vec![0u16; 32_768];
    let mut size = buf.len() as u32;
    // SAFETY: `buf` is valid for `size` units; `size` is in/out.
    unsafe {
        QueryFullProcessImageNameW(
            handle.0,
            PROCESS_NAME_WIN32,
            PWSTR(buf.as_mut_ptr()),
            &mut size,
        )
    }
    .ok()?;
    let path = String::from_utf16_lossy(buf.get(..(size as usize).min(buf.len()))?);
    file_name_of(&path).map(str::to_string)
}

struct ChildSearch {
    frame_pid: u32,
    found: u32,
}

unsafe extern "system" fn child_window_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    // SAFETY: `lparam` is the address of the `ChildSearch` that `uwp_child_pid`
    // keeps alive for the whole EnumChildWindows call; nothing else aliases it.
    let search = unsafe { &mut *(lparam.0 as *mut ChildSearch) };
    let mut pid = 0u32;
    // SAFETY: `pid` is a valid out pointer.
    unsafe {
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
    }
    if pid != 0 && pid != search.frame_pid {
        search.found = pid;
        return BOOL::from(false); // stop enumerating
    }
    BOOL::from(true)
}

/// The pid of the first child window owned by a process other than the frame host.
fn uwp_child_pid(frame: HWND, frame_pid: u32) -> Option<u32> {
    let mut search = ChildSearch {
        frame_pid,
        found: 0,
    };
    // SAFETY: the callback only dereferences the pointer passed here, and the
    // enumeration is synchronous so `search` outlives it.
    unsafe {
        let _ = EnumChildWindows(
            Some(frame),
            Some(child_window_proc),
            LPARAM(&mut search as *mut ChildSearch as isize),
        );
    }
    (search.found != 0).then_some(search.found)
}

// ---------------------------------------------------------------------------
// GDI guards
// ---------------------------------------------------------------------------

/// `GetWindowDC` / `ReleaseDC`.
struct WindowDc {
    hwnd: HWND,
    hdc: HDC,
}

impl WindowDc {
    fn acquire(hwnd: HWND) -> Option<Self> {
        // SAFETY: a failure returns a null HDC, checked below.
        let hdc = unsafe { GetWindowDC(Some(hwnd)) };
        (!hdc.is_invalid()).then_some(WindowDc { hwnd, hdc })
    }
}

impl Drop for WindowDc {
    fn drop(&mut self) {
        // SAFETY: `hdc` came from GetWindowDC(hwnd) and is released once.
        unsafe {
            ReleaseDC(Some(self.hwnd), self.hdc);
        }
    }
}

/// `CreateCompatibleDC` / `DeleteDC`.
struct MemDc(HDC);

impl MemDc {
    fn create(like: HDC) -> Option<Self> {
        // SAFETY: a failure returns a null HDC, checked below.
        let hdc = unsafe { CreateCompatibleDC(Some(like)) };
        (!hdc.is_invalid()).then_some(MemDc(hdc))
    }
}

impl Drop for MemDc {
    fn drop(&mut self) {
        // SAFETY: created by CreateCompatibleDC, deleted once.
        unsafe {
            let _ = DeleteDC(self.0);
        }
    }
}

/// `CreateCompatibleBitmap` / `DeleteObject`.
struct Bitmap(HBITMAP);

impl Bitmap {
    fn create(like: HDC, width: i32, height: i32) -> Option<Self> {
        // SAFETY: a failure returns a null HBITMAP, checked below.
        let bmp = unsafe { CreateCompatibleBitmap(like, width, height) };
        (!bmp.is_invalid()).then_some(Bitmap(bmp))
    }
}

impl Drop for Bitmap {
    fn drop(&mut self) {
        // SAFETY: created by CreateCompatibleBitmap; by the time this runs the
        // bitmap has been deselected from its DC (see `Selected`).
        unsafe {
            let _ = DeleteObject(HGDIOBJ::from(self.0));
        }
    }
}

/// A bitmap selected into a DC; the previous object is selected back on
/// `restore` or drop (whichever comes first), as GDI requires before the
/// bitmap is read with `GetDIBits` or deleted.
struct Selected {
    dc: HDC,
    old: HGDIOBJ,
    active: bool,
}

impl Selected {
    fn select(dc: HDC, bitmap: &Bitmap) -> Option<Self> {
        // SAFETY: both handles are live; failure returns a null HGDIOBJ.
        let old = unsafe { SelectObject(dc, HGDIOBJ::from(bitmap.0)) };
        (!old.is_invalid()).then_some(Selected {
            dc,
            old,
            active: true,
        })
    }

    fn restore(&mut self) {
        if self.active {
            // SAFETY: `old` is the object SelectObject returned for this DC.
            unsafe {
                SelectObject(self.dc, self.old);
            }
            self.active = false;
        }
    }
}

impl Drop for Selected {
    fn drop(&mut self) {
        self.restore();
    }
}

// ---------------------------------------------------------------------------
// Capture
// ---------------------------------------------------------------------------

enum RenderError {
    /// `PrintWindow` returned FALSE.
    PrintFailed,
    Os(String),
}

/// Draw the window into a window-sized bitmap and return it as top-down
/// 32-bpp BGRA (`plan.win_w * plan.win_h * 4` bytes). Every GDI object is
/// released by the guards on every path; drop order is the reverse of
/// declaration (selection undone, bitmap, memory DC, window DC).
fn render(hwnd: HWND, plan: &CapturePlan) -> std::result::Result<Vec<u8>, RenderError> {
    let os = |m: &str| RenderError::Os(m.to_string());
    let window_dc = WindowDc::acquire(hwnd).ok_or_else(|| os("GetWindowDC failed"))?;
    let mem_dc = MemDc::create(window_dc.hdc).ok_or_else(|| os("CreateCompatibleDC failed"))?;
    let bitmap = Bitmap::create(window_dc.hdc, plan.win_w, plan.win_h)
        .ok_or_else(|| os("CreateCompatibleBitmap failed"))?;
    let mut selected =
        Selected::select(mem_dc.0, &bitmap).ok_or_else(|| os("SelectObject failed"))?;

    // SAFETY: live window handle and memory DC. Never read the desktop DC.
    if !unsafe { PrintWindow(hwnd, mem_dc.0, PW_RENDERFULLCONTENT) }.as_bool() {
        return Err(RenderError::PrintFailed);
    }

    // GetDIBits requires that the bitmap is not selected into a DC.
    selected.restore();

    let pixels = (plan.win_w as usize)
        .checked_mul(plan.win_h as usize)
        .and_then(|p| p.checked_mul(4))
        .ok_or_else(|| os("bitmap size overflow"))?;
    let mut bgra = vec![0u8; pixels];
    let mut info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: plan.win_w,
            biHeight: -plan.win_h, // negative: top-down rows
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..BITMAPINFOHEADER::default()
        },
        ..BITMAPINFO::default()
    };
    // SAFETY: `bgra` has room for win_w * win_h 32-bit pixels, `info` describes
    // exactly that layout, and the bitmap is not selected into any DC.
    let lines = unsafe {
        GetDIBits(
            mem_dc.0,
            bitmap.0,
            0,
            plan.win_h as u32,
            Some(bgra.as_mut_ptr().cast::<c_void>()),
            &mut info,
            DIB_RGB_COLORS,
        )
    };
    if lines != plan.win_h {
        return Err(os("GetDIBits failed"));
    }
    Ok(bgra)
}

fn image_from(plan: &CapturePlan, rgba: Vec<u8>) -> Image {
    Image {
        width: plan.crop_w as u32,
        height: plan.crop_h as u32,
        rgba,
    }
}

/// Geometry of one capture, in window-local pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CapturePlan {
    /// Size of the full window rectangle (what PrintWindow renders).
    win_w: i32,
    win_h: i32,
    /// The part to keep: the visible frame, without the invisible border.
    crop_x: i32,
    crop_y: i32,
    crop_w: i32,
    crop_h: i32,
}

/// Work out the bitmap size and the crop from `GetWindowRect` (`window`) and
/// the DWM extended frame bounds (`frame`). Refuses windows that are too
/// small or too large. A frame that does not overlap the window is ignored
/// (the whole window is kept).
fn plan_capture(window: RECT, frame: RECT) -> std::result::Result<CapturePlan, &'static str> {
    let win_w = i64::from(window.right) - i64::from(window.left);
    let win_h = i64::from(window.bottom) - i64::from(window.top);
    if win_w <= 0 || win_h <= 0 {
        return Err("window has no area");
    }
    if win_w * win_h > MAX_PIXELS {
        return Err("window is too large to capture");
    }

    let x0 = (i64::from(frame.left) - i64::from(window.left)).clamp(0, win_w);
    let y0 = (i64::from(frame.top) - i64::from(window.top)).clamp(0, win_h);
    let x1 = (i64::from(frame.right) - i64::from(window.left)).clamp(0, win_w);
    let y1 = (i64::from(frame.bottom) - i64::from(window.top)).clamp(0, win_h);
    let (cx, cy, cw, ch) = if x1 > x0 && y1 > y0 {
        (x0, y0, x1 - x0, y1 - y0)
    } else {
        (0, 0, win_w, win_h)
    };

    if cw < i64::from(MIN_SIDE) || ch < i64::from(MIN_SIDE) {
        return Err("window is too small");
    }
    // Every value is bounded by win_w/win_h, which passed the pixel limit
    // (so each side is < 80 million and fits an i32).
    let to_i32 = |v: i64| i32::try_from(v).map_err(|_| "window is too large to capture");
    Ok(CapturePlan {
        win_w: to_i32(win_w)?,
        win_h: to_i32(win_h)?,
        crop_x: to_i32(cx)?,
        crop_y: to_i32(cy)?,
        crop_w: to_i32(cw)?,
        crop_h: to_i32(ch)?,
    })
}

/// Take the crop rectangle out of a window-sized top-down BGRA buffer and
/// convert it to RGBA with alpha forced to 255. `None` if the buffer is too
/// short for the plan.
fn bgra_to_rgba_crop(bgra: &[u8], plan: &CapturePlan) -> Option<Vec<u8>> {
    let win_w = usize::try_from(plan.win_w).ok()?;
    let win_h = usize::try_from(plan.win_h).ok()?;
    let (cx, cy) = (
        usize::try_from(plan.crop_x).ok()?,
        usize::try_from(plan.crop_y).ok()?,
    );
    let (cw, ch) = (
        usize::try_from(plan.crop_w).ok()?,
        usize::try_from(plan.crop_h).ok()?,
    );
    if cx.checked_add(cw)? > win_w || cy.checked_add(ch)? > win_h {
        return None;
    }
    let mut out = Vec::with_capacity(cw.checked_mul(ch)?.checked_mul(4)?);
    for row in cy..cy + ch {
        let start = row.checked_mul(win_w)?.checked_add(cx)?.checked_mul(4)?;
        let end = start.checked_add(cw.checked_mul(4)?)?;
        for px in bgra.get(start..end)?.chunks_exact(4) {
            if let [b, g, r, _] = *px {
                out.extend_from_slice(&[r, g, b, 255]);
            }
        }
    }
    Some(out)
}

/// True when every pixel has the same colour (a failed PrintWindow typically
/// yields solid black). Such a frame carries no text, so it is "blank".
fn is_uniform(rgba: &[u8]) -> bool {
    let mut pixels = rgba.chunks_exact(4);
    let Some(first) = pixels.next() else {
        return true;
    };
    let first = first.get(..3);
    pixels.all(|px| px.get(..3) == first)
}

// ---------------------------------------------------------------------------
// Small pure helpers
// ---------------------------------------------------------------------------

/// Last path component of a Windows path.
fn file_name_of(path: &str) -> Option<&str> {
    path.rsplit(['\\', '/'])
        .next()
        .filter(|name| !name.is_empty())
}

/// Milliseconds since the last input; `GetTickCount` wraps every 49.7 days.
fn idle_ms(now_tick: u32, last_input_tick: u32) -> u32 {
    now_tick.wrapping_sub(last_input_tick)
}

/// Escape text for an XML element body and drop characters XML 1.0 forbids
/// (they would make `LoadXml` fail).
fn xml_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            '\t' | '\n' | '\r' => out.push(ch),
            c if (c as u32) < 0x20 || c == '\u{FFFE}' || c == '\u{FFFF}' => {}
            c => out.push(c),
        }
    }
    out
}

fn toast_xml(title: &str, body: &str) -> String {
    format!(
        "<toast><visual><binding template=\"ToastGeneric\"><text>{}</text><text>{}</text></binding></visual></toast>",
        xml_escape(title),
        xml_escape(body)
    )
}

/// How long an answer of the private-browsing probe stays valid. A "yes"
/// sticks (a window never turns normal). A "no" is short-lived because
/// Chromium builds its accessibility tree lazily, so an early negative can be
/// wrong; "unknown" is retried soon.
fn private_ttl(answer: Option<bool>) -> Duration {
    match answer {
        Some(true) => PRIVATE_TRUE_TTL,
        Some(false) => PRIVATE_FALSE_TTL,
        None => PRIVATE_NONE_TTL,
    }
}

/// Cache of private-browsing answers keyed by (window handle, pid) so that a
/// reused handle in a different process is not served a stale answer.
#[derive(Default)]
struct PrivateCache {
    entries: HashMap<(u64, u32), (Instant, Option<bool>)>,
}

impl PrivateCache {
    /// `Some(answer)` on a live hit (the answer itself may be `None`).
    fn get(&self, key: (u64, u32), now: Instant) -> Option<Option<bool>> {
        self.entries
            .get(&key)
            .filter(|(expires, _)| *expires > now)
            .map(|(_, answer)| *answer)
    }

    fn put(&mut self, key: (u64, u32), answer: Option<bool>, now: Instant) {
        if self.entries.len() >= PRIVATE_CACHE_CAP && !self.entries.contains_key(&key) {
            self.entries.retain(|_, (expires, _)| *expires > now);
        }
        if self.entries.len() >= PRIVATE_CACHE_CAP && !self.entries.contains_key(&key) {
            // Still full of live entries: evict the one expiring first.
            let oldest = self
                .entries
                .iter()
                .min_by_key(|(_, (expires, _))| *expires)
                .map(|(k, _)| *k);
            if let Some(oldest) = oldest {
                self.entries.remove(&oldest);
            }
        }
        self.entries
            .insert(key, (now + private_ttl(answer), answer));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Explicit native smoke test: only this process's nonactivating fixture is rendered.
    /// It never observes the foreground window and capture has no desktop-copy fallback.
    #[test]
    #[ignore = "opt-in native fixture; requires Windows desktop APIs and an OCR language"]
    fn windows_fixture_capture_and_ocr() {
        use windows::core::w;
        use windows::Win32::Foundation::{COLORREF, LRESULT, WPARAM};
        use windows::Win32::Graphics::Gdi::{
            BeginPaint, CreateFontW, DrawTextW, EndPaint, FillRect, GetStockObject, SetTextColor,
            UpdateWindow, CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET, DEFAULT_QUALITY, DT_LEFT,
            DT_NOPREFIX, DT_TOP, HBRUSH, OUT_DEFAULT_PRECIS, PAINTSTRUCT, WHITE_BRUSH,
        };
        use windows::Win32::UI::WindowsAndMessaging::{
            CreateWindowExW, DefWindowProcW, DestroyWindow, RegisterClassW, UnregisterClassW,
            WM_PAINT, WM_PRINT, WM_PRINTCLIENT, WNDCLASSW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
            WS_POPUP, WS_VISIBLE,
        };
        // A hidden STATIC control is allowed to ignore WM_PRINT. This class explicitly
        // paints only constant fixture text into its own paint DC or PrintWindow DC.
        unsafe extern "system" fn paint_fixture(
            hwnd: HWND,
            msg: u32,
            wp: WPARAM,
            lp: LPARAM,
        ) -> LRESULT {
            if msg == WM_PRINT || msg == WM_PRINTCLIENT || msg == WM_PAINT {
                // SAFETY: Windows supplied this DC for this self-owned window's PrintWindow message.
                unsafe {
                    let mut paint = PAINTSTRUCT::default();
                    let dc = if msg == WM_PAINT {
                        BeginPaint(hwnd, &mut paint)
                    } else {
                        HDC(wp.0 as *mut std::ffi::c_void)
                    };
                    let mut bounds = RECT {
                        left: 0,
                        top: 0,
                        right: 900,
                        bottom: 220,
                    };
                    FillRect(dc, &bounds, HBRUSH(GetStockObject(WHITE_BRUSH).0));
                    SetTextColor(dc, COLORREF(0));
                    let font = CreateFontW(
                        36,
                        0,
                        0,
                        0,
                        400,
                        0,
                        0,
                        0,
                        DEFAULT_CHARSET,
                        OUT_DEFAULT_PRECIS,
                        CLIP_DEFAULT_PRECIS,
                        DEFAULT_QUALITY,
                        0,
                        w!("Arial"),
                    );
                    let old = SelectObject(dc, HGDIOBJ(font.0));
                    let mut text: Vec<u16> =
                        "HELLO SCREENSENSE FIXTURE\r\nBUILD COMPLETE NO ERRORS"
                            .encode_utf16()
                            .collect();
                    DrawTextW(dc, &mut text, &mut bounds, DT_LEFT | DT_TOP | DT_NOPREFIX);
                    SelectObject(dc, old);
                    let _ = DeleteObject(HGDIOBJ(font.0));
                    if msg == WM_PAINT {
                        let _ = EndPaint(hwnd, &paint);
                    }
                }
                return LRESULT(0);
            }
            // SAFETY: forwarding messages for the self-owned registered class.
            unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
        }
        struct Fixture {
            hwnd: HWND,
        }
        impl Drop for Fixture {
            fn drop(&mut self) {
                // SAFETY: only this fixture owns the window and registered class.
                unsafe {
                    let _ = DestroyWindow(self.hwnd);
                    let _ = UnregisterClassW(w!("ScreenSenseIsolatedFixture"), None);
                }
            }
        }
        // SAFETY: constant class name and callback; the window contains only synthetic text and cannot activate.
        let hwnd = unsafe {
            assert_ne!(
                RegisterClassW(&WNDCLASSW {
                    lpfnWndProc: Some(paint_fixture),
                    lpszClassName: w!("ScreenSenseIsolatedFixture"),
                    ..Default::default()
                }),
                0
            );
            CreateWindowExW(
                WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
                w!("ScreenSenseIsolatedFixture"),
                w!("self-owned synthetic fixture"),
                WS_POPUP | WS_VISIBLE,
                30,
                30,
                900,
                220,
                None,
                None,
                None,
                None,
            )
        }
        .expect("create self-owned nonactivating fixture");
        let fixture = Fixture { hwnd };
        // SAFETY: paint only the new fixture; WS_EX_NOACTIVATE prevents stealing focus.
        unsafe {
            let _ = UpdateWindow(hwnd);
        }
        let mut platform = WinPlatform::new();
        let image = platform
            .capture(&WindowInfo {
                handle: hwnd_to_handle(fixture.hwnd),
                pid: std::process::id(),
                app: "screensense-fixture".into(),
                title: "self-owned fixture".into(),
                width: 900,
                height: 220,
                ..Default::default()
            })
            .expect("PrintWindow fixture capture (no desktop fallback)");
        let mut ocr = crate::win_ocr::create("en-US").expect("Windows OCR en-US language");
        let lines = ocr.recognize(&image).expect("OCR synthetic fixture pixels");
        let text = lines
            .iter()
            .flat_map(|l| &l.words)
            .map(|w| w.text.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            text.to_uppercase().contains("SCREENSENSE"),
            "synthetic OCR: {text}"
        );
        assert!(
            text.to_uppercase().contains("BUILD COMPLETE"),
            "synthetic OCR: {text}"
        );
        assert!(lines
            .iter()
            .flat_map(|l| &l.words)
            .all(|w| w.x + w.w <= image.width && w.y + w.h <= image.height));
        println!("native fixture only: PrintWindow {}x{}, {}, {} words; fixture handles cleaned at test exit", image.width, image.height, ocr.name(), lines.iter().map(|l| l.words.len()).sum::<usize>());
    }

    /// Native OCR, with a GDI-generated in-memory bitmap and no screen/DC read.
    #[test]
    #[ignore = "opt-in native OCR; requires an installed Windows OCR language"]
    fn windows_synthetic_ocr() {
        use windows::core::w;
        use windows::Win32::Foundation::COLORREF;
        use windows::Win32::Graphics::Gdi::{
            CreateDIBSection, CreateFontW, DrawTextW, FillRect, GdiFlush, GetStockObject,
            SetTextColor, CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET, DEFAULT_QUALITY, DT_LEFT,
            DT_NOPREFIX, DT_TOP, HBRUSH, OUT_DEFAULT_PRECIS, WHITE_BRUSH,
        };
        let plan = plan_capture(rect(0, 0, 900, 220), rect(0, 0, 900, 220)).unwrap();
        let mem = MemDc::create(HDC::default()).expect("memory DC, no screen DC");
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: 900,
                biHeight: -220,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits = std::ptr::null_mut();
        // SAFETY: the info describes a bounded bitmap; bits is a valid out pointer.
        let bitmap = Bitmap(
            unsafe { CreateDIBSection(Some(mem.0), &info, DIB_RGB_COLORS, &mut bits, None, 0) }
                .expect("in-memory bitmap"),
        );
        let _selected = Selected::select(mem.0, &bitmap).unwrap();
        // SAFETY: live owned memory DC and bitmap; only constant synthetic text is painted.
        let bgra = unsafe {
            let mut bounds = rect(0, 0, 900, 220);
            FillRect(mem.0, &bounds, HBRUSH(GetStockObject(WHITE_BRUSH).0));
            SetTextColor(mem.0, COLORREF(0));
            let font = CreateFontW(
                36,
                0,
                0,
                0,
                400,
                0,
                0,
                0,
                DEFAULT_CHARSET,
                OUT_DEFAULT_PRECIS,
                CLIP_DEFAULT_PRECIS,
                DEFAULT_QUALITY,
                0,
                w!("Arial"),
            );
            let old = SelectObject(mem.0, HGDIOBJ(font.0));
            let mut text: Vec<u16> = "HELLO SCREENSENSE FIXTURE\r\nBUILD COMPLETE NO ERRORS"
                .encode_utf16()
                .collect();
            DrawTextW(
                mem.0,
                &mut text,
                &mut bounds,
                DT_LEFT | DT_TOP | DT_NOPREFIX,
            );
            SelectObject(mem.0, old);
            let _ = DeleteObject(HGDIOBJ(font.0));
            assert!(GdiFlush().as_bool(), "flush synthetic GDI writes");
            assert!(!bits.is_null());
            // CreateDIBSection owns this allocation until bitmap is dropped, below.
            std::slice::from_raw_parts(bits.cast::<u8>(), 900 * 220 * 4).to_vec()
        };
        let image = image_from(&plan, bgra_to_rgba_crop(&bgra, &plan).unwrap());
        assert!(!is_uniform(&image.rgba), "synthetic bitmap has text");
        let mut ocr = crate::win_ocr::create("en-US").expect("Windows OCR language");
        let lines = ocr.recognize(&image).expect("recognize in-memory fixture");
        let text = lines
            .iter()
            .flat_map(|l| &l.words)
            .map(|w| w.text.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            text.to_uppercase().contains("SCREENSENSE"),
            "synthetic OCR: {text}"
        );
        assert!(
            text.to_uppercase().contains("BUILD COMPLETE"),
            "synthetic OCR: {text}"
        );
        println!(
            "native OCR only: {}x{}, {}, {} words; no screen read",
            image.width,
            image.height,
            ocr.name(),
            lines.iter().map(|l| l.words.len()).sum::<usize>()
        );
    }

    fn rect(left: i32, top: i32, right: i32, bottom: i32) -> RECT {
        RECT {
            left,
            top,
            right,
            bottom,
        }
    }

    #[test]
    fn file_name_is_the_last_component() {
        assert_eq!(
            file_name_of(r"C:\Program Files\Microsoft VS Code\Code.exe"),
            Some("Code.exe")
        );
        assert_eq!(file_name_of("notepad.exe"), Some("notepad.exe"));
        assert_eq!(file_name_of(r"C:\dir\"), None);
        assert_eq!(file_name_of(""), None);
    }

    #[test]
    fn idle_survives_tick_wraparound() {
        assert_eq!(idle_ms(5_000, 2_000), 3_000);
        assert_eq!(idle_ms(1_000, u32::MAX - 999), 2_000);
        assert_eq!(idle_ms(7, 7), 0);
    }

    #[test]
    fn plan_crops_the_invisible_border() {
        // Typical Windows 10/11 window: 7 px invisible border left/right/bottom.
        let window = rect(100, 100, 1114, 807);
        let frame = rect(107, 100, 1107, 800);
        let plan = plan_capture(window, frame).unwrap();
        assert_eq!((plan.win_w, plan.win_h), (1014, 707));
        assert_eq!(
            (plan.crop_x, plan.crop_y, plan.crop_w, plan.crop_h),
            (7, 0, 1000, 700)
        );
    }

    #[test]
    fn plan_keeps_the_whole_window_when_the_frame_is_unusable() {
        let window = rect(0, 0, 800, 600);
        let plan = plan_capture(window, rect(5000, 5000, 5100, 5100)).unwrap();
        assert_eq!(
            (plan.crop_x, plan.crop_y, plan.crop_w, plan.crop_h),
            (0, 0, 800, 600)
        );
        let plan = plan_capture(window, window).unwrap();
        assert_eq!((plan.crop_w, plan.crop_h), (800, 600));
    }

    #[test]
    fn plan_clamps_a_frame_larger_than_the_window() {
        let plan = plan_capture(rect(0, 0, 800, 600), rect(-10, -10, 900, 700)).unwrap();
        assert_eq!(
            (plan.crop_x, plan.crop_y, plan.crop_w, plan.crop_h),
            (0, 0, 800, 600)
        );
    }

    #[test]
    fn plan_refuses_tiny_huge_and_empty_windows() {
        assert!(plan_capture(rect(0, 0, 39, 500), rect(0, 0, 39, 500)).is_err());
        assert!(plan_capture(rect(0, 0, 500, 39), rect(0, 0, 500, 39)).is_err());
        assert!(plan_capture(rect(0, 0, 40, 40), rect(0, 0, 40, 40)).is_ok());
        assert!(plan_capture(rect(0, 0, 10_000, 8_001), rect(0, 0, 10_000, 8_001)).is_err());
        assert!(plan_capture(rect(0, 0, 10_000, 8_000), rect(0, 0, 10_000, 8_000)).is_ok());
        assert!(plan_capture(rect(10, 10, 10, 10), rect(10, 10, 10, 10)).is_err());
        assert!(plan_capture(rect(i32::MIN, 0, i32::MAX, 100), rect(0, 0, 1, 1)).is_err());
    }

    #[test]
    fn crop_converts_bgra_to_opaque_rgba() {
        // 3x2 window, crop the right 2 columns of the bottom row.
        let mut bgra = Vec::new();
        for i in 0..6u8 {
            bgra.extend_from_slice(&[i, i + 10, i + 20, 0]); // B, G, R, A=0
        }
        let plan = CapturePlan {
            win_w: 3,
            win_h: 2,
            crop_x: 1,
            crop_y: 1,
            crop_w: 2,
            crop_h: 1,
        };
        let rgba = bgra_to_rgba_crop(&bgra, &plan).unwrap();
        assert_eq!(rgba, vec![24, 14, 4, 255, 25, 15, 5, 255]);
    }

    #[test]
    fn crop_rejects_short_buffers_and_bad_plans() {
        let plan = CapturePlan {
            win_w: 4,
            win_h: 4,
            crop_x: 0,
            crop_y: 0,
            crop_w: 4,
            crop_h: 4,
        };
        assert!(bgra_to_rgba_crop(&[0u8; 63], &plan).is_none());
        assert!(bgra_to_rgba_crop(&[0u8; 64], &plan).is_some());
        let outside = CapturePlan { crop_x: 2, ..plan };
        assert!(bgra_to_rgba_crop(&[0u8; 64], &outside).is_none());
    }

    #[test]
    fn uniform_frames_are_blank() {
        assert!(is_uniform(&[]));
        assert!(is_uniform(&[0, 0, 0, 255, 0, 0, 0, 255]));
        assert!(is_uniform(&[9, 9, 9, 255, 9, 9, 9, 7]));
        assert!(!is_uniform(&[0, 0, 0, 255, 0, 0, 1, 255]));
    }

    #[test]
    fn xml_is_escaped() {
        assert_eq!(
            xml_escape("a & b < c > d \" e ' f"),
            "a &amp; b &lt; c &gt; d &quot; e &apos; f"
        );
        assert_eq!(xml_escape("ok\tline\nnext"), "ok\tline\nnext");
        assert_eq!(xml_escape("bell\u{7}nul\u{0}"), "bellnul");
        assert_eq!(xml_escape("中文 ✓"), "中文 ✓");
    }

    #[test]
    fn toast_xml_has_two_escaped_text_nodes() {
        let xml = toast_xml("<b>Build & test</b>", "it's \"done\"");
        assert!(xml.starts_with("<toast><visual><binding template=\"ToastGeneric\">"));
        assert!(xml.contains("<text>&lt;b&gt;Build &amp; test&lt;/b&gt;</text>"));
        assert!(xml.contains("<text>it&apos;s &quot;done&quot;</text>"));
        assert_eq!(xml.matches("<text>").count(), 2);
    }

    #[test]
    fn private_answers_have_the_specified_lifetimes() {
        assert_eq!(private_ttl(Some(true)), Duration::from_secs(1800));
        assert_eq!(private_ttl(Some(false)), Duration::from_secs(60));
        assert_eq!(private_ttl(None), Duration::from_secs(10));
    }

    #[test]
    fn private_cache_expires_and_distinguishes_miss_from_unknown() {
        let mut cache = PrivateCache::default();
        let t0 = Instant::now();
        assert_eq!(cache.get((1, 1), t0), None);
        cache.put((1, 1), None, t0);
        cache.put((2, 1), Some(false), t0);
        cache.put((3, 1), Some(true), t0);
        assert_eq!(cache.get((1, 1), t0), Some(None));
        assert_eq!(cache.get((1, 1), t0 + Duration::from_secs(11)), None);
        assert_eq!(
            cache.get((2, 1), t0 + Duration::from_secs(59)),
            Some(Some(false))
        );
        assert_eq!(cache.get((2, 1), t0 + Duration::from_secs(61)), None);
        assert_eq!(
            cache.get((3, 1), t0 + Duration::from_secs(1799)),
            Some(Some(true))
        );
        // Same handle, different process: miss.
        assert_eq!(cache.get((3, 2), t0), None);
    }

    #[test]
    fn private_cache_is_capped() {
        let mut cache = PrivateCache::default();
        let t0 = Instant::now();
        for i in 0..(PRIVATE_CACHE_CAP as u64 * 2) {
            cache.put((i, 0), Some(true), t0 + Duration::from_millis(i));
        }
        assert!(cache.entries.len() <= PRIVATE_CACHE_CAP);
        // The newest entry survives; the oldest was evicted.
        let last = PRIVATE_CACHE_CAP as u64 * 2 - 1;
        assert!(cache.entries.contains_key(&(last, 0)));
        assert!(!cache.entries.contains_key(&(0, 0)));
    }
}
