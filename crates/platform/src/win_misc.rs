//! Small Win32 helpers shared by the Windows backend: process liveness,
//! detached spawn, COM initialisation, the sign-in (autostart) registry value.
//!
//! The `pub fn`s with fixed signatures are called from `process.rs` and
//! `autostart.rs`; the `pub(crate)` items are shared with `win.rs`,
//! `win_uia.rs` and `win_ocr.rs`.

use std::cell::Cell;
use std::io;
use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_ACCESS_DENIED, ERROR_FILE_NOT_FOUND, ERROR_MORE_DATA, HANDLE,
    RPC_E_CHANGED_MODE, WIN32_ERROR,
};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegGetValueW, RegOpenKeyExW, RegSetValueExW,
    HKEY, HKEY_CURRENT_USER, KEY_SET_VALUE, REG_OPTION_NON_VOLATILE, REG_SZ, RRF_RT_REG_SZ,
};
use windows::Win32::System::Threading::{
    GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
};

/// `STILL_ACTIVE` as the `u32` that `GetExitCodeProcess` writes (the windows
/// crate types the constant as `NTSTATUS`, which is the wrong type here).
const STILL_ACTIVE_CODE: u32 = 259;

/// `CREATE_NO_WINDOW`. Mutually exclusive with `DETACHED_PROCESS`, so it is
/// the one used: no console is created for a console-subsystem child.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
/// `CREATE_NEW_PROCESS_GROUP`: Ctrl-C in the launching console does not reach the child.
const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;

const RUN_KEY: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
const RUN_VALUE: PCWSTR = w!("ScreenSense");

// ---------------------------------------------------------------------------
// COM / WinRT apartment
// ---------------------------------------------------------------------------

thread_local! {
    static COM_READY: Cell<bool> = const { Cell::new(false) };
}

/// Make sure the calling thread has a COM apartment (MTA) so UI Automation
/// and WinRT calls work. Idempotent per thread, cheap after the first call.
/// `S_FALSE` (already initialised) and `RPC_E_CHANGED_MODE` (the thread
/// already has an STA) both leave COM usable, so both count as ready.
/// `CoUninitialize` is deliberately never called: the apartment lives as long
/// as the thread.
pub(crate) fn ensure_com() {
    COM_READY.with(|ready| {
        if ready.get() {
            return;
        }
        // SAFETY: no pointers are passed; the call only changes the calling
        // thread's apartment state and has no preconditions.
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        if hr.is_ok() || hr == RPC_E_CHANGED_MODE {
            ready.set(true);
        }
    });
}

// ---------------------------------------------------------------------------
// Process handles
// ---------------------------------------------------------------------------

/// A process handle that is closed on drop.
pub(crate) struct OwnedHandle(pub HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        // SAFETY: the handle was returned by a successful OpenProcess and is
        // closed exactly once, here.
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

/// `OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION)`; works for most
/// processes, including elevated ones, but not for some protected ones.
pub(crate) fn open_process_limited(pid: u32) -> windows::core::Result<OwnedHandle> {
    // SAFETY: plain value arguments; the returned handle is owned by the guard.
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid)? };
    Ok(OwnedHandle(handle))
}

/// Whether a process with this pid is running (has not exited).
pub fn pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    match open_process_limited(pid) {
        Ok(handle) => {
            let mut code = 0u32;
            // SAFETY: `handle` is a live process handle with query access and
            // `code` is a valid out pointer for the duration of the call.
            match unsafe { GetExitCodeProcess(handle.0, &mut code) } {
                // A process that exited with code 259 looks alive; that is the
                // documented limitation of this technique.
                Ok(()) => code == STILL_ACTIVE_CODE,
                Err(_) => false,
            }
        }
        // The process exists but we may not look at it.
        Err(e) => e.code() == ERROR_ACCESS_DENIED.to_hresult(),
    }
}

/// Start `exe args…` detached from this process: no console window, own
/// process group, stdin closed, stdout/stderr appended to `log` or discarded.
pub fn spawn_detached(exe: &Path, args: &[&str], log: Option<&Path>) -> io::Result<u32> {
    let (out, err) = match log {
        Some(path) => {
            let file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)?;
            (Stdio::from(file.try_clone()?), Stdio::from(file))
        }
        None => (Stdio::null(), Stdio::null()),
    };
    let child = Command::new(exe)
        .args(args)
        .stdin(Stdio::null())
        .stdout(out)
        .stderr(err)
        .creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP)
        .spawn()?;
    // Dropping `Child` closes our handle to it without killing it.
    Ok(child.id())
}

// ---------------------------------------------------------------------------
// Autostart (HKCU\...\Run)
// ---------------------------------------------------------------------------

fn win32_error(code: WIN32_ERROR) -> io::Error {
    io::Error::from_raw_os_error(code.0 as i32)
}

/// A registry key that is closed on drop.
struct RegKey(HKEY);

impl Drop for RegKey {
    fn drop(&mut self) {
        // SAFETY: the key was opened by RegCreateKeyExW/RegOpenKeyExW and is
        // closed exactly once, here.
        unsafe {
            let _ = RegCloseKey(self.0);
        }
    }
}

/// Open (creating if needed) the Run key for writing.
fn create_run_key() -> io::Result<RegKey> {
    let mut key = HKEY::default();
    // SAFETY: `RUN_KEY` is a NUL-terminated literal, `key` a valid out pointer.
    let status = unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            RUN_KEY,
            None,
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE,
            None,
            &mut key,
            None,
        )
    };
    if status.is_err() {
        return Err(win32_error(status));
    }
    Ok(RegKey(key))
}

/// The command line stored in the Run key: `"<exe>" daemon start`.
///
/// A `\\?\` verbatim prefix (what `canonicalize` produces) is stripped: the
/// shell's Run-key launcher does not reliably understand it. Windows paths
/// cannot contain a double quote, so quoting the path is sufficient.
fn autostart_command(exe: &Path) -> String {
    let raw = exe.to_string_lossy();
    let path: String = if let Some(rest) = raw.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = raw.strip_prefix(r"\\?\") {
        rest.to_string()
    } else {
        raw.into_owned()
    };
    format!("\"{path}\" daemon start")
}

/// Register `"<exe>" daemon start` to start at sign-in.
pub fn autostart_enable(exe: &Path) -> io::Result<()> {
    let command = autostart_command(exe);
    if command.contains('\0') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "path contains a NUL character",
        ));
    }
    // REG_SZ data: UTF-16LE including the terminating NUL.
    let mut data: Vec<u8> = Vec::with_capacity((command.len() + 1) * 2);
    for unit in command.encode_utf16().chain(std::iter::once(0)) {
        data.extend_from_slice(&unit.to_le_bytes());
    }
    let key = create_run_key()?;
    // SAFETY: `key` is open with KEY_SET_VALUE; `data` outlives the call.
    let status = unsafe { RegSetValueExW(key.0, RUN_VALUE, None, REG_SZ, Some(&data)) };
    if status.is_err() {
        return Err(win32_error(status));
    }
    Ok(())
}

/// Remove the sign-in entry. A missing key or value is not an error.
pub fn autostart_disable() -> io::Result<()> {
    let mut key = HKEY::default();
    // SAFETY: `RUN_KEY` is a NUL-terminated literal, `key` a valid out pointer.
    let status =
        unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, RUN_KEY, None, KEY_SET_VALUE, &mut key) };
    if status == ERROR_FILE_NOT_FOUND {
        return Ok(());
    }
    if status.is_err() {
        return Err(win32_error(status));
    }
    let key = RegKey(key);
    // SAFETY: `key` is open with KEY_SET_VALUE.
    let status = unsafe { RegDeleteValueW(key.0, RUN_VALUE) };
    if status == ERROR_FILE_NOT_FOUND || status.is_ok() {
        return Ok(());
    }
    Err(win32_error(status))
}

/// The registered command line, if the sign-in entry exists.
pub fn autostart_status() -> Option<String> {
    // The value can change between the size query and the read; retry a few times.
    for _ in 0..4 {
        let mut bytes = 0u32;
        // SAFETY: size query only (no data pointer); `bytes` is a valid out pointer.
        let status = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                RUN_KEY,
                RUN_VALUE,
                RRF_RT_REG_SZ,
                None,
                None,
                Some(&mut bytes),
            )
        };
        if status.is_err() && status != ERROR_MORE_DATA {
            return None;
        }
        // +2 units of slack: RegGetValueW NUL-terminates but may report the
        // size without it for malformed data.
        let mut buf = vec![0u16; (bytes as usize) / 2 + 2];
        let mut capacity = u32::try_from(buf.len() * 2).ok()?;
        // SAFETY: `buf` is valid for `capacity` bytes and outlives the call.
        let status = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                RUN_KEY,
                RUN_VALUE,
                RRF_RT_REG_SZ,
                None,
                Some(buf.as_mut_ptr().cast()),
                Some(&mut capacity),
            )
        };
        if status == ERROR_MORE_DATA {
            continue;
        }
        if status.is_err() {
            return None;
        }
        let end = buf.iter().position(|&u| u == 0).unwrap_or(buf.len());
        return Some(String::from_utf16_lossy(buf.get(..end)?));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_quotes_a_path_with_spaces() {
        let cmd = autostart_command(Path::new(r"C:\Program Files\ScreenSense\screensense.exe"));
        assert_eq!(
            cmd,
            r#""C:\Program Files\ScreenSense\screensense.exe" daemon start"#
        );
    }

    #[test]
    fn command_strips_the_verbatim_prefix() {
        assert_eq!(
            autostart_command(Path::new(r"\\?\C:\tools\screensense.exe")),
            r#""C:\tools\screensense.exe" daemon start"#
        );
        assert_eq!(
            autostart_command(Path::new(r"\\?\UNC\srv\share\screensense.exe")),
            r#""\\srv\share\screensense.exe" daemon start"#
        );
    }

    #[test]
    fn own_pid_is_alive_and_zero_is_not() {
        assert!(pid_alive(std::process::id()));
        assert!(!pid_alive(0));
    }
}
