//! SMAppService-era service management: bundle detection, the `dev.heed.agent`
//! launch agent, legacy (`dev.heed.daemon`) plist migration, and the launchd
//! log redirection the bundle plist can no longer express.

use std::path::{Path, PathBuf};

pub mod bundle;
pub mod launchd_log;
pub mod legacy;
pub mod sm;

pub use bundle::{bundle_layout, detect_bundle, BundleLayout};
pub use sm::{heed_agent, AgentService};

/// Label + plist name of the SMAppService agent (HD-2). Never the legacy label.
pub const AGENT_LABEL: &str = "dev.heed.agent";
pub const AGENT_PLIST_NAME: &str = "dev.heed.agent.plist";
/// Legacy launchd label written by `install::service::render_macos_plist`.
/// Referenced only to boot out and delete; never passed to SMAppService.
pub const LEGACY_LABEL: &str = "dev.heed.daemon";
pub const LEGACY_PLIST_NAME: &str = "dev.heed.daemon.plist";
/// Minimum macOS major for the SMAppService path.
pub const MIN_MACOS_MAJOR: u32 = 13;

/// `SMAppService.status`, mirrored so callers never touch the ObjC type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServiceStatus {
    Enabled,
    RequiresApproval,
    NotRegistered,
    NotFound,
}

impl ServiceStatus {
    /// Apple's spelling, as printed by `heed service status`.
    pub fn as_str(self) -> &'static str {
        match self {
            ServiceStatus::Enabled => "enabled",
            ServiceStatus::RequiresApproval => "requiresApproval",
            ServiceStatus::NotRegistered => "notRegistered",
            ServiceStatus::NotFound => "notFound",
        }
    }

    /// Enabled or RequiresApproval: BTM has a record for us; do not re-register.
    pub fn is_registered(self) -> bool {
        matches!(
            self,
            ServiceStatus::Enabled | ServiceStatus::RequiresApproval
        )
    }
}

/// What `heed install` does with the agent, decided from `SMAppService.status`
/// alone. There is deliberately no "unsealed bundle" pre-check: `notFound`
/// is what a fresh, valid, Developer-ID-signed bundle reports before its
/// first ever `register()` (HD-2), and ServiceManagement cannot tell
/// "never seen" from "unsealed" — only `register()` can, so it is the probe.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RegisterAction {
    /// `notFound` / `notRegistered`: call `register()`.
    Register,
    /// `enabled` / `requiresApproval`: BTM already has our record.
    AlreadyRegistered,
}

/// Pure status → action mapping for [`RegisterAction`].
pub fn register_action(status: ServiceStatus) -> RegisterAction {
    if status.is_registered() {
        RegisterAction::AlreadyRegistered
    } else {
        RegisterAction::Register
    }
}

/// Criterion 6: a double-clicked bundle is launched with no arguments and no
/// controlling terminal. Exit quietly instead of printing clap help.
pub fn quiet_exit_wanted(argc: usize, stdout_is_terminal: bool) -> bool {
    argc <= 1 && !stdout_is_terminal
}

/// `Some(canonical)` when the process was started through a symlink
/// (`given != canonical`) whose real file lives inside an app bundle;
/// `None` otherwise. Pure.
///
/// SMAppService resolves `Bundle.main` from the path the process was
/// started with, so `~/.heed/bin/heed` (a symlink into `Heed.app`) must
/// re-exec the real binary before any ServiceManagement call.
pub fn reexec_target(given: &Path, canonical: &Path) -> Option<PathBuf> {
    if given == canonical {
        return None;
    }
    bundle_layout(canonical).map(|b| b.executable)
}

/// Runtime half of [`reexec_target`]: replace this process with the
/// canonical bundle binary, same argv (minus argv[0]), environment and cwd.
/// Returns only if there is nothing to do or the exec failed (the caller
/// then proceeds as today; a failed exec is reported on stderr).
pub fn reexec_if_symlinked_into_bundle() {
    let Ok(given) = std::env::current_exe() else {
        return;
    };
    let Ok(canonical) = std::fs::canonicalize(&given) else {
        return;
    };
    let Some(target) = reexec_target(&given, &canonical) else {
        return;
    };
    use std::os::unix::process::CommandExt;
    let err = std::process::Command::new(&target)
        .args(std::env::args_os().skip(1))
        .exec();
    eprintln!("heed: could not re-exec {}: {err}", target.display());
}

/// Parse `sw_vers -productVersion` output (`"13.6.1\n"` → `Some(13)`).
pub fn macos_major_version(product_version: &str) -> Option<u32> {
    product_version
        .trim()
        .split('.')
        .next()
        .and_then(|major| major.parse().ok())
}

/// Runtime check via `sw_vers -productVersion`, run once per process and
/// cached. On any failure to run or parse it, assume supported: the binary's
/// `minos 13.0` already refuses to load below 13. Always `false` off macOS.
#[cfg(target_os = "macos")]
pub fn macos_supports_smappservice() -> bool {
    static SUPPORTED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *SUPPORTED.get_or_init(|| {
        let Ok(out) = std::process::Command::new("sw_vers")
            .arg("-productVersion")
            .output()
        else {
            return true;
        };
        match macos_major_version(&String::from_utf8_lossy(&out.stdout)) {
            Some(major) => major >= MIN_MACOS_MAJOR,
            None => true,
        }
    })
}

#[cfg(not(target_os = "macos"))]
pub fn macos_supports_smappservice() -> bool {
    false
}

/// Resolve `current_exe()` (canonicalised), require an in-bundle layout and
/// macOS >= 13. `None` on Linux, from a bare binary, or on macOS 12. After
/// [`reexec_if_symlinked_into_bundle`] has run, `current_exe()` is already
/// the canonical bundle path whenever this returns `Some`.
pub fn bundle_context() -> Option<BundleLayout> {
    let exe = std::env::current_exe().ok()?;
    let exe = std::fs::canonicalize(&exe).unwrap_or(exe);
    let layout = detect_bundle(&exe)?;
    macos_supports_smappservice().then_some(layout)
}

/// `~/.heed/bin/heed` — Codezilla's stable path to the CLI.
pub fn cli_symlink_path(home: &Path) -> PathBuf {
    home.join(".heed/bin/heed")
}

/// Scratch name beside [`cli_symlink_path`] used to swap the link in
/// atomically. Per-pid so two concurrent installs never share it.
pub fn cli_symlink_temp_path(home: &Path) -> PathBuf {
    home.join(format!(".heed/bin/.heed.{}.tmp", std::process::id()))
}

/// `~/.heed/bin/heed -> target`. Creates `~/.heed/bin`, replaces any existing
/// file or symlink at the link path, idempotent. Fails if the link path is a
/// directory.
///
/// The replacement is atomic: the new link is created under a temp name in
/// the same directory and `rename(2)`d over the old entry, so there is no
/// window in which `~/.heed/bin/heed` does not exist (Codezilla resolves it
/// at any moment).
pub fn ensure_cli_symlink(home: &Path, target: &Path) -> Result<(), String> {
    let link = cli_symlink_path(home);
    let dir = link.parent().expect("symlink path has a parent");
    std::fs::create_dir_all(dir).map_err(|e| format!("create_dir_all {}: {e}", dir.display()))?;
    match std::fs::symlink_metadata(&link) {
        Ok(meta) if meta.is_dir() => {
            return Err(format!(
                "{} is a directory; refusing to replace it with a symlink",
                link.display()
            ));
        }
        Ok(meta) => {
            if meta.is_symlink() && std::fs::read_link(&link).ok().as_deref() == Some(target) {
                return Ok(());
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("stat {}: {e}", link.display())),
    }

    let temp = cli_symlink_temp_path(home);
    // A stale entry from an interrupted earlier run would make symlink()
    // fail with EEXIST; it is ours to clear.
    match std::fs::remove_file(&temp) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("remove {}: {e}", temp.display())),
    }
    std::os::unix::fs::symlink(target, &temp)
        .map_err(|e| format!("symlink {} -> {}: {e}", temp.display(), target.display()))?;
    std::fs::rename(&temp, &link).map_err(|e| {
        let _ = std::fs::remove_file(&temp);
        format!("rename {} -> {}: {e}", temp.display(), link.display())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn macos_major_version_parses_sw_vers_output() {
        assert_eq!(macos_major_version("13.6.1\n"), Some(13));
        assert_eq!(macos_major_version("26.6.1"), Some(26));
        assert_eq!(macos_major_version("12.7.6"), Some(12));
        assert_eq!(macos_major_version(""), None);
        assert_eq!(macos_major_version("beta"), None);
    }

    #[test]
    fn quiet_exit_only_without_args_and_without_tty() {
        assert!(quiet_exit_wanted(1, false));
        assert!(!quiet_exit_wanted(1, true));
        assert!(!quiet_exit_wanted(2, false));
        assert!(quiet_exit_wanted(0, false));
    }

    #[test]
    fn ensure_cli_symlink_replaces_file_and_retargets() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let link = cli_symlink_path(home);
        assert_eq!(link, home.join(".heed/bin/heed"));

        // Pre-existing regular file (today's layout) is replaced by a symlink.
        std::fs::create_dir_all(link.parent().unwrap()).unwrap();
        std::fs::write(&link, b"old binary").unwrap();
        let target_a = home.join("A.app/Contents/MacOS/heed");
        ensure_cli_symlink(home, &target_a).unwrap();
        assert_eq!(std::fs::read_link(&link).unwrap(), target_a);

        // Idempotent.
        ensure_cli_symlink(home, &target_a).unwrap();
        assert_eq!(std::fs::read_link(&link).unwrap(), target_a);

        // Retargets.
        let target_b = home.join("B.app/Contents/MacOS/heed");
        ensure_cli_symlink(home, &target_b).unwrap();
        assert_eq!(std::fs::read_link(&link).unwrap(), target_b);

        // The swap goes through a temp name beside the link; nothing but the
        // link itself may be left in ~/.heed/bin afterwards.
        assert_eq!(bin_dir_entries(home), vec!["heed".to_string()]);
    }

    fn bin_dir_entries(home: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(cli_symlink_path(home).parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    /// HD-11 (2): the symlink is swapped in atomically (create at a temp
    /// name, rename over the link) so there is never a moment without
    /// `~/.heed/bin/heed`. A stale temp entry from an interrupted earlier
    /// run must neither break the swap nor be left behind.
    #[test]
    fn ensure_cli_symlink_swaps_atomically_and_cleans_stale_temp() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let link = cli_symlink_path(home);
        std::fs::create_dir_all(link.parent().unwrap()).unwrap();
        let stale = cli_symlink_temp_path(home);
        assert_ne!(stale, link);
        assert_eq!(stale.parent(), link.parent());
        std::os::unix::fs::symlink("/nonexistent/old", &stale).unwrap();

        let target = home.join("A.app/Contents/MacOS/heed");
        ensure_cli_symlink(home, &target).unwrap();
        assert_eq!(std::fs::read_link(&link).unwrap(), target);
        assert_eq!(bin_dir_entries(home), vec!["heed".to_string()]);

        // Replacing an existing link never removes it first: a stale temp
        // entry is cleared, the new link is created there, then renamed.
        let target_b = home.join("B.app/Contents/MacOS/heed");
        std::fs::write(&stale, b"stale").unwrap();
        ensure_cli_symlink(home, &target_b).unwrap();
        assert_eq!(std::fs::read_link(&link).unwrap(), target_b);
        assert_eq!(bin_dir_entries(home), vec!["heed".to_string()]);
    }

    #[test]
    fn service_status_as_str_uses_apple_spelling() {
        assert_eq!(ServiceStatus::Enabled.as_str(), "enabled");
        assert_eq!(ServiceStatus::RequiresApproval.as_str(), "requiresApproval");
        assert_eq!(ServiceStatus::NotRegistered.as_str(), "notRegistered");
        assert_eq!(ServiceStatus::NotFound.as_str(), "notFound");
        assert!(ServiceStatus::Enabled.is_registered());
        assert!(ServiceStatus::RequiresApproval.is_registered());
        assert!(!ServiceStatus::NotRegistered.is_registered());
        assert!(!ServiceStatus::NotFound.is_registered());
    }

    /// Criterion 1: there is no status()-based pre-check. `notFound` is what
    /// a fresh, valid bundle reports before its first ever register() (HD-2),
    /// so it must lead to register(), exactly like `notRegistered`.
    #[test]
    fn register_action_registers_on_not_found_and_not_registered() {
        assert_eq!(
            register_action(ServiceStatus::NotFound),
            RegisterAction::Register
        );
        assert_eq!(
            register_action(ServiceStatus::NotRegistered),
            RegisterAction::Register
        );
        assert_eq!(
            register_action(ServiceStatus::Enabled),
            RegisterAction::AlreadyRegistered
        );
        assert_eq!(
            register_action(ServiceStatus::RequiresApproval),
            RegisterAction::AlreadyRegistered
        );
    }

    #[test]
    fn reexec_target_only_for_symlink_into_bundle() {
        let link = Path::new("/Users/t/.heed/bin/heed");
        let bundle_exe =
            Path::new("/Users/t/Library/Application Support/Heed/Heed.app/Contents/MacOS/heed");
        assert_eq!(
            reexec_target(link, bundle_exe),
            Some(bundle_exe.to_path_buf())
        );
        assert_eq!(reexec_target(bundle_exe, bundle_exe), None);
        assert_eq!(reexec_target(link, Path::new("/usr/local/bin/heed")), None);
        assert_eq!(
            reexec_target(
                Path::new("/x/Heed.app/Contents/MacOS/heed"),
                Path::new("/y/Heed.app/Contents/MacOS/heed")
            ),
            Some(PathBuf::from("/y/Heed.app/Contents/MacOS/heed"))
        );
    }
}
