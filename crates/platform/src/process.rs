//! Process helpers: is a pid alive, start a detached background process.

use std::path::Path;

/// Whether a process with this pid exists.
pub fn pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    #[cfg(windows)]
    {
        return crate::win_misc::pid_alive(pid);
    }
    #[cfg(target_os = "linux")]
    {
        return Path::new(&format!("/proc/{pid}")).exists();
    }
    #[allow(unreachable_code)]
    {
        // No cheap dependency-free check elsewhere: ask `kill -0`.
        std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
}

/// Start `exe args…` so that it outlives this process and has no console
/// window or controlling terminal. stdout/stderr are appended to `log` when
/// given, otherwise discarded. Returns the child's pid.
pub fn spawn_detached(exe: &Path, args: &[&str], log: Option<&Path>) -> std::io::Result<u32> {
    #[cfg(windows)]
    {
        return crate::win_misc::spawn_detached(exe, args, log);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        use std::process::{Command, Stdio};
        let (out, err) = match log {
            Some(path) => {
                let f = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
                (Stdio::from(f.try_clone()?), Stdio::from(f))
            }
            None => (Stdio::null(), Stdio::null()),
        };
        let child = Command::new(exe)
            .args(args)
            .stdin(Stdio::null())
            .stdout(out)
            .stderr(err)
            // Own process group: a Ctrl-C or hang-up in the launching terminal does not reach it.
            .process_group(0)
            .spawn()?;
        return Ok(child.id());
    }
    #[allow(unreachable_code)]
    {
        let _ = (exe, args, log);
        Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "spawn_detached"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn own_pid_is_alive_and_zero_is_not() {
        assert!(pid_alive(std::process::id()));
        assert!(!pid_alive(0));
    }

    #[cfg(unix)]
    #[test]
    fn detached_child_runs_and_logs() {
        let dir = std::env::temp_dir().join(format!("ss-spawn-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("out.log");
        let pid = spawn_detached(Path::new("/bin/sh"), &["-c", "echo hello-detached"], Some(&log)).unwrap();
        assert!(pid > 0);
        for _ in 0..50 {
            if std::fs::read_to_string(&log).map(|s| s.contains("hello-detached")).unwrap_or(false) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(40));
        }
        assert!(std::fs::read_to_string(&log).unwrap().contains("hello-detached"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
