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
    let mut notes = bootout_legacy(&plan);
    if let Some(plist) = plan.delete_plist {
        notes.push(delete_legacy_plist(&plist)?);
    }
    Ok(notes)
}

/// Phase 1 of the migration around `register()`: boot the legacy agent out
/// (so two daemons never overlap on `state.json` / `heedd.pid`) but leave its
/// plist file in place. If `register()` then fails, that untouched file is
/// bootstrapped again and the machine is exactly as it was. Best-effort;
/// notes for the report.
pub fn bootout_legacy(plan: &LegacyPlan) -> Vec<String> {
    let mut notes = Vec::new();
    if plan.bootout {
        if let Some(target) = bootout_legacy_agent() {
            notes.push(format!("booted out {target}"));
        }
    }
    notes
}

/// Phase 2, decided from the `register()` result. Pure: nothing on disk moves.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MigrationOutcome {
    /// `register()` succeeded: the legacy plist (if any) can go.
    Commit { delete_plist: Option<PathBuf> },
    /// `register()` failed: bootstrap the legacy plist (if any) again and
    /// report the error. The plist is never deleted on this path.
    Rollback {
        bootstrap: Option<PathBuf>,
        error: String,
    },
}

/// Pure decision for [`finish_migration`].
pub fn decide_after_register(plan: &LegacyPlan, register: Result<(), String>) -> MigrationOutcome {
    match register {
        Ok(()) => MigrationOutcome::Commit {
            delete_plist: plan.delete_plist.clone(),
        },
        Err(error) => MigrationOutcome::Rollback {
            bootstrap: plan.delete_plist.clone(),
            error,
        },
    }
}

/// Apply a [`MigrationOutcome`]: delete the plist on `Commit`; on `Rollback`
/// re-bootstrap the legacy plist (best-effort) and hand back the `register()`
/// error text verbatim so the caller exits 1 with the machine as it was.
pub fn finish_migration(outcome: MigrationOutcome) -> Result<Vec<String>, String> {
    match outcome {
        MigrationOutcome::Commit { delete_plist } => Ok(delete_plist
            .map(|p| delete_legacy_plist(&p))
            .transpose()?
            .into_iter()
            .collect()),
        MigrationOutcome::Rollback { bootstrap, error } => {
            if let Some(plist) = bootstrap {
                match bootstrap_legacy_agent(&plist) {
                    Some(true) => eprintln!("Restored legacy agent from {}", plist.display()),
                    Some(false) => eprintln!(
                        "Warning: launchctl bootstrap {} failed; run it by hand: \
                         launchctl bootstrap gui/$(id -u) {}",
                        plist.display(),
                        plist.display()
                    ),
                    None => {}
                }
            }
            Err(error)
        }
    }
}

fn delete_legacy_plist(plist: &Path) -> Result<String, String> {
    std::fs::remove_file(plist).map_err(|e| format!("remove {}: {e}", plist.display()))?;
    Ok(format!("removed {}", plist.display()))
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
    launchctl(&["bootout", &target]).map(|_| target)
}

#[cfg(not(target_os = "macos"))]
fn bootout_legacy_agent() -> Option<String> {
    None
}

/// `launchctl bootstrap gui/<uid> <plist>` — the inverse of
/// [`bootout_legacy_agent`], used only to undo a bootout after a failed
/// `register()`. `Some(exit status success)`, or `None` when launchctl could
/// not be run at all.
#[cfg(target_os = "macos")]
fn bootstrap_legacy_agent(plist: &Path) -> Option<bool> {
    let domain = format!("gui/{}", nix::unistd::getuid());
    launchctl(&["bootstrap", &domain, &plist.to_string_lossy()])
}

#[cfg(not(target_os = "macos"))]
fn bootstrap_legacy_agent(_plist: &Path) -> Option<bool> {
    None
}

/// Run `launchctl` with stdio discarded; `Some(success)` or `None` if it
/// could not be spawned.
#[cfg(target_os = "macos")]
fn launchctl(args: &[&str]) -> Option<bool> {
    std::process::Command::new("launchctl")
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .ok()
        .map(|s| s.success())
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

    fn home_with_legacy_plist() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let plist = legacy_plist_path(tmp.path());
        std::fs::create_dir_all(plist.parent().unwrap()).unwrap();
        std::fs::write(&plist, b"<plist/>").unwrap();
        (tmp, plist)
    }

    /// Criterion 1, failure path: register() Err ⇒ the legacy plist (still
    /// on disk, only booted out) is bootstrapped again and the NSError text
    /// is returned; the plist is never deleted.
    #[test]
    fn after_register_failure_rolls_back_legacy_and_keeps_plist() {
        let (tmp, plist) = home_with_legacy_plist();
        let plan = plan_legacy_cleanup(tmp.path());
        let err = "Operation not permitted (SMAppServiceErrorDomain code 1)".to_string();
        let outcome = decide_after_register(&plan, Err(err.clone()));
        assert_eq!(
            outcome,
            MigrationOutcome::Rollback {
                bootstrap: Some(plist.clone()),
                error: err,
            }
        );
        assert!(plist.exists(), "decision must not touch the filesystem");
    }

    /// Criterion 1, success path: register() Ok ⇒ delete the legacy plist.
    #[test]
    fn after_register_success_commits_plist_deletion() {
        let (tmp, plist) = home_with_legacy_plist();
        let plan = plan_legacy_cleanup(tmp.path());
        assert_eq!(
            decide_after_register(&plan, Ok(())),
            MigrationOutcome::Commit {
                delete_plist: Some(plist),
            }
        );
    }

    /// Fresh Mac (no legacy plist): nothing to restore, nothing to delete;
    /// the register() error still propagates.
    #[test]
    fn after_register_without_legacy_plist_has_nothing_to_restore() {
        let tmp = tempfile::tempdir().unwrap();
        let plan = plan_legacy_cleanup(tmp.path());
        assert_eq!(
            decide_after_register(&plan, Ok(())),
            MigrationOutcome::Commit { delete_plist: None }
        );
        assert_eq!(
            decide_after_register(&plan, Err("boom".into())),
            MigrationOutcome::Rollback {
                bootstrap: None,
                error: "boom".into(),
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
