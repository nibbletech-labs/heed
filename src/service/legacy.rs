//! Migration away from the legacy `dev.heed.daemon` launchd plist that
//! `install::service::render_macos_plist` writes under `~/Library/LaunchAgents`.
//!
//! The legacy label is only ever booted out and its plist deleted. It is
//! never handed to SMAppService (`unregister()` on it flips the legacy BTM
//! record to disabled — HD-2).

use std::path::{Path, PathBuf};

use crate::service::LEGACY_PLIST_NAME;

/// `~/Library/LaunchAgents/dev.heed.daemon.plist`.
pub fn legacy_plist_path(home: &Path) -> PathBuf {
    home.join("Library/LaunchAgents").join(LEGACY_PLIST_NAME)
}

/// What [`run_legacy_cleanup`] will do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LegacyPlan {
    /// Only true when the plist exists under `home` — the guard that keeps a
    /// tmp-HOME `cargo test` from booting out the real user's agent (launchd
    /// domains are per-uid, not per-HOME).
    pub bootout: bool,
    pub delete_plist: Option<PathBuf>,
}

/// Pure decision (filesystem read only).
pub fn plan_legacy_cleanup(home: &Path) -> LegacyPlan {
    let plist = legacy_plist_path(home);
    if plist.exists() {
        LegacyPlan {
            bootout: true,
            delete_plist: Some(plist),
        }
    } else {
        LegacyPlan {
            bootout: false,
            delete_plist: None,
        }
    }
}

/// Execute the plan, best-effort: `launchctl bootout gui/<uid>/dev.heed.daemon`
/// (exit status ignored, output discarded) then delete the plist. Returns
/// human-readable notes for the install report. A failed bootout is never an
/// error; `Err` only if the plist exists but cannot be deleted. Off macOS the
/// bootout is skipped.
pub fn run_legacy_cleanup(home: &Path) -> Result<Vec<String>, String> {
    let plan = plan_legacy_cleanup(home);
    let mut notes = Vec::new();
    if plan.bootout {
        if let Some(target) = bootout_legacy_agent() {
            notes.push(format!("booted out {target}"));
        }
    }
    if let Some(plist) = plan.delete_plist {
        std::fs::remove_file(&plist).map_err(|e| format!("remove {}: {e}", plist.display()))?;
        notes.push(format!("removed {}", plist.display()));
    }
    Ok(notes)
}

/// `launchctl bootout gui/<uid>/dev.heed.daemon`. Returns the service target
/// that was booted out (whether or not launchd had it loaded), or `None` when
/// launchctl could not be run at all.
#[cfg(target_os = "macos")]
fn bootout_legacy_agent() -> Option<String> {
    let target = format!(
        "gui/{}/{}",
        nix::unistd::getuid(),
        crate::service::LEGACY_LABEL
    );
    std::process::Command::new("launchctl")
        .args(["bootout", &target])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .ok()
        .map(|_| target)
}

#[cfg(not(target_os = "macos"))]
fn bootout_legacy_agent() -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_plan_is_noop_when_plist_absent() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(
            plan_legacy_cleanup(tmp.path()),
            LegacyPlan {
                bootout: false,
                delete_plist: None
            }
        );
    }

    #[test]
    fn legacy_plan_boots_out_and_deletes_when_plist_present() {
        let tmp = tempfile::tempdir().unwrap();
        let plist = tmp
            .path()
            .join("Library/LaunchAgents/dev.heed.daemon.plist");
        std::fs::create_dir_all(plist.parent().unwrap()).unwrap();
        std::fs::write(&plist, b"<plist/>").unwrap();
        assert_eq!(legacy_plist_path(tmp.path()), plist);
        assert_eq!(
            plan_legacy_cleanup(tmp.path()),
            LegacyPlan {
                bootout: true,
                delete_plist: Some(plist),
            }
        );
    }
}
