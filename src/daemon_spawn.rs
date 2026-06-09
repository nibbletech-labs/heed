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

/// Best-effort daemon shutdown: SIGTERM the recorded pid if alive.
pub fn stop_if_running(home: &Path) -> Result<(), String> {
    let Some(pid) = running_pid(home) else {
        return Ok(());
    };
    let pid_n = i32::try_from(pid).map_err(|_| format!("pid {pid} out of range"))?;
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(pid_n),
        nix::sys::signal::Signal::SIGTERM,
    )
    .map_err(|e| format!("SIGTERM {pid}: {e}"))?;
    // Best-effort wait — give it up to 3s to write final state and clean its pidfile.
    for _ in 0..30 {
        if running_pid(home).is_none() {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    Err(format!("daemon pid {pid} did not exit within 3s"))
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
