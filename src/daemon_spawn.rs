//! Spawn / detect / stop the daemon process. The daemon writes a pidfile to
//! `~/.heed/heedd.pid` containing `<pid>\n<lstart>\n` so we can tell whether
//! a recorded PID is the same daemon (vs PID reuse).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::install::heed_dir;
use crate::liveness::{self, LivenessCheck};

#[derive(Clone, Copy, Debug)]
pub enum SpawnResult {
    Spawned(u32),
    AlreadyRunning(u32),
}

pub fn pid_lockfile(home: &Path) -> PathBuf {
    heed_dir(home).join("heedd.pid")
}

/// Returns Some(pid) if a healthy daemon is running (PID alive + lstart match).
pub fn running_pid(home: &Path) -> Option<u32> {
    let path = pid_lockfile(home);
    let raw = fs::read_to_string(&path).ok()?;
    let mut lines = raw.lines();
    let pid: u32 = lines.next()?.trim().parse().ok()?;
    let lstart = lines.next().unwrap_or("").trim();
    match liveness::check(pid, lstart) {
        LivenessCheck::Live => Some(pid),
        LivenessCheck::Gone => None,
    }
}

/// Spawn the daemon as a detached child if one isn't already running.
pub fn spawn_if_not_running(home: &Path) -> Result<SpawnResult, String> {
    if let Some(pid) = running_pid(home) {
        return Ok(SpawnResult::AlreadyRunning(pid));
    }
    let heed = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;

    // Open log files for the detached child. macOS launchctl plist does the
    // same thing for `--service-install` mode.
    let log_dir = heed_dir(home);
    fs::create_dir_all(&log_dir).map_err(|e| format!("create_dir_all {log_dir:?}: {e}"))?;
    let stdout = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_dir.join("heedd.out.log"))
        .map_err(|e| format!("open out log: {e}"))?;
    let stderr = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_dir.join("heedd.err.log"))
        .map_err(|e| format!("open err log: {e}"))?;

    let child = unsafe {
        use std::os::unix::process::CommandExt;
        let mut cmd = Command::new(&heed);
        cmd.arg("daemon")
            .stdin(std::process::Stdio::null())
            .stdout(stdout)
            .stderr(stderr)
            .pre_exec(|| {
                // Detach: new session so SIGHUP doesn't kill the daemon when
                // the spawning shell exits.
                let _ = nix::unistd::setsid();
                Ok(())
            })
            .spawn()
    };
    let child = child.map_err(|e| format!("spawn heed daemon: {e}"))?;
    Ok(SpawnResult::Spawned(child.id()))
}

/// Best-effort daemon shutdown: SIGTERM the recorded pid if alive, then wait
/// up to 3 s for it to write final state and clear its pidfile. `Ok(true)`
/// when a live daemon was found and is now gone, `Ok(false)` when none was
/// running. A pid that disappears between the liveness check and the signal
/// (launchd got there first) counts as stopped.
pub fn stop_if_running(home: &Path) -> Result<bool, String> {
    let Some(pid) = running_pid(home) else {
        return Ok(false);
    };
    let pid_n = i32::try_from(pid).map_err(|_| format!("pid {pid} out of range"))?;
    let sent = nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(pid_n),
        nix::sys::signal::Signal::SIGTERM,
    );
    stop_signal_outcome(sent, pid)?;
    if wait_for_daemon_exit(home) {
        Ok(true)
    } else {
        Err(format!("daemon pid {pid} did not exit within 3s"))
    }
}

/// Classify the `kill(2)` result: ESRCH means the daemon exited on its own
/// after we read the pidfile, which is the outcome we wanted. Pure.
fn stop_signal_outcome(sent: Result<(), nix::Error>, pid: u32) -> Result<(), String> {
    match sent {
        Ok(()) | Err(nix::errno::Errno::ESRCH) => Ok(()),
        Err(e) => Err(format!("SIGTERM {pid}: {e}")),
    }
}

/// Poll the pidfile for up to 3 s until no live daemon is recorded there.
/// `true` once it is gone, `false` on timeout.
pub fn wait_for_daemon_exit(home: &Path) -> bool {
    for _ in 0..30 {
        if running_pid(home).is_none() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn running_pid_returns_none_for_missing_lockfile() {
        let tmp = tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".heed")).unwrap();
        assert!(running_pid(tmp.path()).is_none());
    }

    #[test]
    fn running_pid_validates_lstart_match() {
        let tmp = tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".heed")).unwrap();
        let me = std::process::id();
        let my_lstart = liveness::current_lstart(me).unwrap();
        let path = pid_lockfile(tmp.path());
        std::fs::write(&path, format!("{me}\n{my_lstart}\n")).unwrap();
        assert_eq!(running_pid(tmp.path()), Some(me));
    }

    #[test]
    fn running_pid_returns_none_on_lstart_mismatch() {
        let tmp = tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".heed")).unwrap();
        let me = std::process::id();
        let path = pid_lockfile(tmp.path());
        std::fs::write(&path, format!("{me}\nBogus lstart that won't match\n")).unwrap();
        assert!(running_pid(tmp.path()).is_none());
    }

    /// HD-11 (1): `install --uninstall` unregisters the launchd agent and
    /// then SIGTERMs the pidfile daemon; launchd usually wins the race, so
    /// the pid recorded as live a moment ago is gone by the time we signal
    /// it. ESRCH means "already stopped", not a failure. Any other errno is.
    #[test]
    fn stop_signal_outcome_treats_esrch_as_stopped() {
        assert_eq!(stop_signal_outcome(Ok(()), 4242), Ok(()));
        assert_eq!(
            stop_signal_outcome(Err(nix::errno::Errno::ESRCH), 4242),
            Ok(())
        );
        let err = stop_signal_outcome(Err(nix::errno::Errno::EPERM), 4242).unwrap_err();
        assert!(err.contains("SIGTERM 4242"), "{err}");
        assert!(err.contains("EPERM"), "{err}");
    }

    #[test]
    fn running_pid_returns_none_for_dead_pid() {
        let tmp = tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".heed")).unwrap();
        let path = pid_lockfile(tmp.path());
        // PID far above any plausible alive process.
        std::fs::write(&path, "4000000\nfake lstart\n").unwrap();
        assert!(running_pid(tmp.path()).is_none());
    }
}
