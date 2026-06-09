//! PPID + `lstart` liveness check.
//!
//! SPEC §5: a thread is `Live` if a hook event arrived within the last 120s OR
//! `kill -0 $pid` succeeds and `ps -o lstart= -p $pid` matches the recorded
//! `pid_start`. Mismatch on lstart means the PID was reused after the original
//! process died.

use nix::sys::signal;
use nix::unistd::Pid;
use std::process::Command;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LivenessCheck {
    /// Process is still alive and its start-time matches.
    Live,
    /// `kill -0` failed (no such process), or `lstart` mismatched (PID reuse),
    /// or `ps` couldn't be parsed.
    Gone,
}

/// Check whether the process with `pid` is the same instance whose start time
/// equals `expected_lstart`. Empty `expected_lstart` is treated as "no
/// recorded start" — fall back to `kill -0` only.
pub fn check(pid: u32, expected_lstart: &str) -> LivenessCheck {
    if pid == 0 {
        return LivenessCheck::Gone;
    }
    let pid_n = match i32::try_from(pid) {
        Ok(n) => n,
        Err(_) => return LivenessCheck::Gone,
    };
    // Signal 0 = "alive check, do not signal".
    if signal::kill(Pid::from_raw(pid_n), None).is_err() {
        return LivenessCheck::Gone;
    }
    // No recorded start time → trust the kill -0 result.
    if expected_lstart.is_empty() {
        return LivenessCheck::Live;
    }
    match current_lstart(pid) {
        Some(current) if current == expected_lstart => LivenessCheck::Live,
        Some(_) => LivenessCheck::Gone, // PID reused
        None => LivenessCheck::Live,    // ps couldn't read it; don't false-positive gone
    }
}

/// Read `ps -o lstart= -p $pid` and return its trimmed output. None on
/// failure or no such process.
pub fn current_lstart(pid: u32) -> Option<String> {
    let output = Command::new("ps")
        .arg("-o")
        .arg("lstart=")
        .arg("-p")
        .arg(pid.to_string())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pid_zero_is_gone() {
        assert_eq!(check(0, ""), LivenessCheck::Gone);
    }

    #[test]
    fn current_pid_is_live() {
        let me = std::process::id();
        let lstart = current_lstart(me).expect("ps should produce lstart for current process");
        assert_eq!(check(me, &lstart), LivenessCheck::Live);
    }

    #[test]
    fn current_pid_with_no_recorded_lstart_is_live() {
        let me = std::process::id();
        assert_eq!(check(me, ""), LivenessCheck::Live);
    }

    #[test]
    fn current_pid_with_mismatched_lstart_is_gone() {
        let me = std::process::id();
        assert_eq!(check(me, "Mon Jan  1 00:00:00 1970"), LivenessCheck::Gone);
    }

    #[test]
    fn very_high_pid_is_gone() {
        // PID > 4 million is essentially impossible on macOS/Linux.
        assert_eq!(check(4_000_000, ""), LivenessCheck::Gone);
    }

    #[test]
    fn killed_process_is_gone_even_if_pid_is_reused() {
        // Real-process counterpart to `current_pid_with_mismatched_lstart_is_gone`
        // (SPEC §14.6): spawn an actual child, record its true pid + lstart while
        // it is alive, kill it, and confirm the recorded identity no longer
        // validates as Live. Covers the production scenario the synthetic test
        // only approximates — a tracked process dying and its PID being freed or
        // reused by another process.
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep child");
        let pid = child.id();
        let lstart = current_lstart(pid).expect("ps should report lstart for a live child");

        // While alive, the child validates against its own recorded start time.
        assert_eq!(check(pid, &lstart), LivenessCheck::Live);

        child.kill().expect("kill child");
        child.wait().expect("reap child");

        // After death + reap: either kill -0 fails (PID is free) or, if the OS
        // immediately reused the PID, current lstart won't match the recorded one.
        // Both resolve to Gone.
        assert_eq!(check(pid, &lstart), LivenessCheck::Gone);
    }
}
