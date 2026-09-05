//! Under launchd the bundle's agent plist carries no `StandardOutPath` /
//! `StandardErrorPath` (a plist sealed inside a signed bundle cannot name a
//! per-user home path), so the daemon points its own stdio at
//! `~/.heed/heedd.{out,err}.log` when it detects it was spawned by launchd.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// launchd sets `XPC_SERVICE_NAME` to the job label for the jobs it spawns
/// (`dev.heed.agent` from the bundle, `dev.heed.daemon` from the legacy
/// plist). Only an exact match counts: LaunchServices sets the same variable
/// to `application.<bundle-id>.N.N` on every GUI-descended process and login
/// shells carry `0`, so a `heed daemon` run by hand in a terminal must keep
/// its own stdio.
pub fn under_launchd(xpc_service_name: Option<&OsStr>) -> bool {
    xpc_service_name.is_some_and(|v| {
        v == OsStr::new(crate::service::AGENT_LABEL)
            || v == OsStr::new(crate::service::LEGACY_LABEL)
    })
}

/// `(~/.heed/heedd.out.log, ~/.heed/heedd.err.log)` — the same files the
/// legacy plist and the foreground spawner use.
pub fn launchd_log_paths(home: &Path) -> (PathBuf, PathBuf) {
    let dir = crate::install::heed_dir(home);
    (dir.join("heedd.out.log"), dir.join("heedd.err.log"))
}

/// Open both log files `O_APPEND|O_CREAT`, dup2 them onto fds 1 and 2, then
/// log one startup line naming the launchd label — the deterministic proof
/// that redirection happened. Creates `~/.heed` if needed. Under the legacy
/// plist launchd already pointed fds 1/2 at these files; re-pointing them is
/// harmless.
pub fn redirect_stdio_to_logs(home: &Path, label: &str) -> Result<(), String> {
    use std::os::unix::io::AsRawFd;

    let (out_path, err_path) = launchd_log_paths(home);
    let dir = crate::install::heed_dir(home);
    std::fs::create_dir_all(&dir).map_err(|e| format!("create_dir_all {}: {e}", dir.display()))?;
    let open = |p: &Path| {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(p)
            .map_err(|e| format!("open {}: {e}", p.display()))
    };
    let out = open(&out_path)?;
    let err = open(&err_path)?;
    nix::unistd::dup2(out.as_raw_fd(), 1).map_err(|e| format!("dup2 stdout: {e}"))?;
    nix::unistd::dup2(err.as_raw_fd(), 2).map_err(|e| format!("dup2 stderr: {e}"))?;
    // `out`/`err` may drop now: the dup'd fds 1 and 2 keep the files open.
    log::info!(
        "daemon: started under launchd ({label}); stdio -> {} / {}",
        out_path.display(),
        err_path.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn under_launchd_detects_xpc_service_name() {
        assert!(under_launchd(Some(OsStr::new("dev.heed.agent"))));
        assert!(under_launchd(Some(OsStr::new("dev.heed.daemon"))));
        // Login shells carry XPC_SERVICE_NAME=0; every GUI-descended process
        // (a Codezilla or Terminal.app shell) carries application.<bundle>.N.N.
        // Neither is launchd running *our* job.
        assert!(!under_launchd(Some(OsStr::new("0"))));
        assert!(!under_launchd(Some(OsStr::new(
            "application.com.nibbletech.codezilla.123.456"
        ))));
        assert!(!under_launchd(Some(OsStr::new("dev.heed.agent.plist"))));
        assert!(!under_launchd(Some(OsStr::new(""))));
        assert!(!under_launchd(None));
    }

    #[test]
    fn launchd_log_paths_live_under_heed_dir() {
        let (out, err) = launchd_log_paths(Path::new("/Users/t"));
        assert_eq!(out, Path::new("/Users/t/.heed/heedd.out.log"));
        assert_eq!(err, Path::new("/Users/t/.heed/heedd.err.log"));
    }
}
