//! One desktop window per user session: a named mutex, and a way for a second
//! start to wake the first one through a message-only window.

use std::os::windows::ffi::OsStrExt;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE, HWND, LPARAM, WPARAM,
};
use windows::Win32::System::Threading::{CreateMutexW, GetCurrentProcessId};
use windows::Win32::UI::WindowsAndMessaging::{
    FindWindowExW, GetWindowThreadProcessId, PostMessageW, HWND_MESSAGE,
};

fn wide(s: &str) -> Vec<u16> {
    std::ffi::OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// The open mutex handle; closing it releases the name.
pub struct Mutex(HANDLE);

impl Drop for Mutex {
    fn drop(&mut self) {
        // SAFETY: the handle came from CreateMutexW and is closed exactly once.
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

/// `Ok(Some(_))`: this process now holds the name. `Ok(None)`: another process
/// already does.
pub fn acquire(name: &str) -> std::io::Result<Option<Mutex>> {
    let name = wide(name);
    // SAFETY: `name` is NUL-terminated and outlives the call; no security
    // attributes; the returned handle is owned by `Mutex`.
    let handle = unsafe { CreateMutexW(None, false, PCWSTR(name.as_ptr())) }
        .map_err(|e| std::io::Error::from_raw_os_error(e.code().0))?;
    // SAFETY: reads the calling thread's last-error value set by CreateMutexW.
    let existed = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
    let guard = Mutex(handle);
    Ok((!existed).then_some(guard))
}

/// Post `message` (with `lparam`) to the first message-only window of class
/// `class` that belongs to another process. Returns whether one was found.
pub fn post_to_other(class: &str, message: u32, lparam: isize) -> bool {
    let class = wide(class);
    // SAFETY: plain query of the calling process id.
    let me = unsafe { GetCurrentProcessId() };
    let mut after: Option<HWND> = None;
    loop {
        // SAFETY: `class` is NUL-terminated and outlives the call; HWND_MESSAGE
        // enumerates message-only windows; `after` is a window returned by the
        // previous call or None.
        let found = unsafe {
            FindWindowExW(
                Some(HWND_MESSAGE),
                after,
                PCWSTR(class.as_ptr()),
                PCWSTR::null(),
            )
        };
        let Ok(hwnd) = found else { return false };
        if hwnd.is_invalid() {
            return false;
        }
        let mut pid = 0u32;
        // SAFETY: `hwnd` is a window handle just returned by FindWindowExW;
        // `pid` is a valid out pointer.
        unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
        if pid != me {
            // SAFETY: posting a message to an existing window has no memory effects here.
            return unsafe { PostMessageW(Some(hwnd), message, WPARAM(0), LPARAM(lparam)) }.is_ok();
        }
        after = Some(hwnd);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_claim_fails_until_the_first_is_dropped() {
        let name = format!("ScreenSenseTest-{}", std::process::id());
        let first = acquire(&name).unwrap();
        assert!(first.is_some());
        assert!(acquire(&name).unwrap().is_none());
        drop(first);
        assert!(acquire(&name).unwrap().is_some());
    }

    #[test]
    fn waking_a_missing_window_reports_false() {
        assert!(!post_to_other("ScreenSenseNoSuchClass", 0x8001, 0));
    }
}
