//! Installer for Heed's bundled hook scripts.
//!
//! Public entry points:
//! - [`install`]: idempotent self-heal. Run on every `heed install`.
//! - [`uninstall`]: remove Heed entries from Claude / Codex configs.
//!
//! Behavior per SPEC §8: each CLI is gated by the *existence of its config
//! directory* — `~/.claude/` for Claude, `~/.codex/` for Codex. If the
//! directory is missing, the CLI is treated as not installed and we don't
//! create config files for it. We intentionally do NOT probe `which claude`
//! / `which codex` (per spec direction; Codezilla did, Heed does not).

use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

pub mod claude;
pub mod codex;
pub mod service;

#[cfg(test)]
mod tests;

const CLAUDE_HOOK_SCRIPTS: &[(&str, &str)] = &[
    (
        "user-prompt-submit.sh",
        include_str!("../../resources/claude-hooks/user-prompt-submit.sh"),
    ),
    (
        "pre-tool-use.sh",
        include_str!("../../resources/claude-hooks/pre-tool-use.sh"),
    ),
    (
        "post-tool-use.sh",
        include_str!("../../resources/claude-hooks/post-tool-use.sh"),
    ),
    (
        "stop.sh",
        include_str!("../../resources/claude-hooks/stop.sh"),
    ),
    (
        "session-end.sh",
        include_str!("../../resources/claude-hooks/session-end.sh"),
    ),
];

const CODEX_HOOK_SCRIPTS: &[(&str, &str)] = &[
    (
        "user-prompt-submit.sh",
        include_str!("../../resources/codex-hooks/user-prompt-submit.sh"),
    ),
    (
        "pre-tool-use.sh",
        include_str!("../../resources/codex-hooks/pre-tool-use.sh"),
    ),
    (
        "post-tool-use.sh",
        include_str!("../../resources/codex-hooks/post-tool-use.sh"),
    ),
    (
        "stop.sh",
        include_str!("../../resources/codex-hooks/stop.sh"),
    ),
    (
        "session-end.sh",
        include_str!("../../resources/codex-hooks/session-end.sh"),
    ),
];

const CLAUDE_HOOK_VERSION: &str = crate::CLAUDE_HOOK_SCRIPTS_VERSION;
const CODEX_HOOK_VERSION: &str = crate::CODEX_HOOK_SCRIPTS_VERSION;

/// Options for [`install`]. Tests construct these directly; the CLI driver
/// translates clap flags into these.
#[derive(Clone, Debug)]
pub struct InstallOptions {
    /// Override `$HOME` — used by tests to redirect filesystem operations.
    pub home: PathBuf,
    pub skip_claude: bool,
    pub skip_codex: bool,
}

impl InstallOptions {
    pub fn from_env() -> Result<Self, String> {
        let home = std::env::var("HOME").map_err(|_| "HOME not set".to_string())?;
        Ok(Self {
            home: PathBuf::from(home),
            skip_claude: false,
            skip_codex: false,
        })
    }
}

/// Summary returned from [`install`] so the CLI driver can render `heed install`'s output.
#[derive(Clone, Debug, Default)]
pub struct InstallReport {
    pub heed_dir: PathBuf,
    pub claude: Option<CliInstallReport>,
    pub codex: Option<CliInstallReport>,
}

#[derive(Clone, Debug)]
pub struct CliInstallReport {
    /// `~/.heed/{claude,codex}-hooks` path the scripts are extracted to.
    pub scripts_dir: PathBuf,
    /// Path to the settings file we edited (`~/.claude/settings.json` or
    /// `~/.codex/config.toml`). `None` if the CLI's directory wasn't present.
    pub settings_path: Option<PathBuf>,
    /// Did we actually write the settings file (vs no-op self-heal)?
    pub settings_changed: bool,
    /// Did we re-extract scripts because of a version bump or fresh install?
    pub scripts_extracted: bool,
    /// User-facing notes (warnings, skip reasons, follow-up steps).
    pub notes: Vec<String>,
}

/// `~/.heed/` — Heed's root state directory.
pub fn heed_dir(home: &Path) -> PathBuf {
    home.join(".heed")
}

pub fn claude_scripts_dir(home: &Path) -> PathBuf {
    heed_dir(home).join("claude-hooks")
}

pub fn codex_scripts_dir(home: &Path) -> PathBuf {
    heed_dir(home).join("codex-hooks")
}

pub fn event_log_path(home: &Path) -> PathBuf {
    heed_dir(home).join("events.jsonl")
}

pub fn owners_path(home: &Path) -> PathBuf {
    heed_dir(home).join("owners.json")
}

pub fn state_path(home: &Path) -> PathBuf {
    heed_dir(home).join("state.json")
}

pub fn claude_config_dir(home: &Path) -> PathBuf {
    home.join(".claude")
}

pub fn claude_settings_path(home: &Path) -> PathBuf {
    claude_config_dir(home).join("settings.json")
}

pub fn codex_config_dir(home: &Path) -> PathBuf {
    home.join(".codex")
}

pub fn codex_config_path(home: &Path) -> PathBuf {
    codex_config_dir(home).join("config.toml")
}

/// Top-level install entry point. Idempotent.
pub fn install(opts: &InstallOptions) -> Result<InstallReport, String> {
    let heed_dir = heed_dir(&opts.home);
    fs::create_dir_all(&heed_dir).map_err(|e| format!("create_dir_all {:?}: {}", heed_dir, e))?;

    // Touch events.jsonl and owners.json so the daemon's watcher always has
    // something to watch. SPEC §8 step 5.
    touch_file_if_missing(&event_log_path(&opts.home))?;
    touch_empty_owners_file_if_missing(&owners_path(&opts.home))?;

    let mut report = InstallReport {
        heed_dir,
        ..Default::default()
    };

    if !opts.skip_claude {
        report.claude = Some(install_claude(opts)?);
    }
    if !opts.skip_codex {
        report.codex = Some(install_codex(opts)?);
    }

    Ok(report)
}

fn install_claude(opts: &InstallOptions) -> Result<CliInstallReport, String> {
    let scripts_dir = claude_scripts_dir(&opts.home);
    let mut notes = Vec::new();

    // Always extract scripts so a downstream consumer (Codezilla / Muxra)
    // can use them without `~/.claude/` existing first. Cost is trivial.
    let scripts_extracted =
        extract_scripts(&scripts_dir, CLAUDE_HOOK_SCRIPTS, CLAUDE_HOOK_VERSION)?;

    let claude_dir = claude_config_dir(&opts.home);
    if !claude_dir.exists() {
        notes.push(format!(
            "{} does not exist; skipping settings.json edit (run Claude Code once to create it, then re-run `heed install`)",
            claude_dir.display()
        ));
        return Ok(CliInstallReport {
            scripts_dir,
            settings_path: None,
            settings_changed: false,
            scripts_extracted,
            notes,
        });
    }

    let settings_path = claude_settings_path(&opts.home);
    let settings_changed = claude::ensure_hooks_in_settings_json(&settings_path, &scripts_dir)?;

    Ok(CliInstallReport {
        scripts_dir,
        settings_path: Some(settings_path),
        settings_changed,
        scripts_extracted,
        notes,
    })
}

fn install_codex(opts: &InstallOptions) -> Result<CliInstallReport, String> {
    let scripts_dir = codex_scripts_dir(&opts.home);
    let mut notes = Vec::new();

    let scripts_extracted = extract_scripts(&scripts_dir, CODEX_HOOK_SCRIPTS, CODEX_HOOK_VERSION)?;

    let codex_dir = codex_config_dir(&opts.home);
    if !codex_dir.exists() {
        notes.push(format!(
            "{} does not exist; skipping config.toml edit",
            codex_dir.display()
        ));
        return Ok(CliInstallReport {
            scripts_dir,
            settings_path: None,
            settings_changed: false,
            scripts_extracted,
            notes,
        });
    }

    // SPEC §9.2 version safety: skip install on known-bad Codex builds.
    if let Some(reason) = crate::cli_detect::codex_known_bad_reason() {
        notes.push(format!(
            "Codex install skipped: {reason}. config.toml left unchanged."
        ));
        return Ok(CliInstallReport {
            scripts_dir,
            settings_path: None,
            settings_changed: false,
            scripts_extracted,
            notes,
        });
    }

    let config_path = codex_config_path(&opts.home);
    let settings_changed = codex::ensure_hooks_in_config_toml(&config_path, &scripts_dir)?;

    // SPEC §8.2: warn about Codex hook trust.
    notes.push(
        "Codex hook entries written. If Codex prompts that hooks need review, open Codex and run /hooks to approve the ~/.heed/codex-hooks commands."
            .to_string(),
    );

    Ok(CliInstallReport {
        scripts_dir,
        settings_path: Some(config_path),
        settings_changed,
        scripts_extracted,
        notes,
    })
}

/// Top-level uninstall entry point. Idempotent.
pub fn uninstall(opts: &InstallOptions) -> Result<(), String> {
    if !opts.skip_claude {
        let scripts_dir = claude_scripts_dir(&opts.home);
        let settings_path = claude_settings_path(&opts.home);
        if settings_path.exists() {
            claude::remove_hooks_from_settings_json(&settings_path, &scripts_dir)?;
        }
    }
    if !opts.skip_codex {
        let scripts_dir = codex_scripts_dir(&opts.home);
        let config_path = codex_config_path(&opts.home);
        if config_path.exists() {
            codex::remove_hooks_from_config_toml(&config_path, &scripts_dir)?;
        }
    }
    Ok(())
}

/// Extract embedded scripts to `dir` and chmod 0o755. Returns `true` if we
/// wrote anything (fresh install or version bump), `false` on no-op self-heal.
///
/// `VERSION` is copied **last** so a partial failure leaves the install
/// looking stale — a re-run will retry.
fn extract_scripts(
    dir: &Path,
    scripts: &[(&str, &str)],
    bundled_version: &str,
) -> Result<bool, String> {
    fs::create_dir_all(dir).map_err(|e| format!("create_dir_all {:?}: {}", dir, e))?;

    let installed_version = fs::read_to_string(dir.join("VERSION"))
        .ok()
        .map(|s| s.trim().to_string());
    if installed_version.as_deref() == Some(bundled_version) {
        return Ok(false);
    }

    for (name, contents) in scripts {
        let path = dir.join(name);
        let mut file = fs::File::create(&path).map_err(|e| format!("create {:?}: {}", path, e))?;
        file.write_all(contents.as_bytes())
            .map_err(|e| format!("write {:?}: {}", path, e))?;
        file.sync_all()
            .map_err(|e| format!("fsync {:?}: {}", path, e))?;
        let mut perms = fs::metadata(&path)
            .map_err(|e| format!("metadata {:?}: {}", path, e))?
            .permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&path, perms)
            .map_err(|e| format!("set_permissions {:?}: {}", path, e))?;
    }

    let version_path = dir.join("VERSION");
    fs::write(&version_path, format!("{bundled_version}\n"))
        .map_err(|e| format!("write VERSION: {}", e))?;

    Ok(true)
}

fn touch_file_if_missing(path: &Path) -> Result<(), String> {
    if path.exists() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("create_dir_all {:?}: {}", parent, e))?;
    }
    fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| format!("create {:?}: {}", path, e))?;
    Ok(())
}

fn touch_empty_owners_file_if_missing(path: &Path) -> Result<(), String> {
    if path.exists() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("create_dir_all {:?}: {}", parent, e))?;
    }
    fs::write(path, "{}\n").map_err(|e| format!("write {:?}: {}", path, e))?;
    Ok(())
}

/// Atomic `tmp + fsync + rename` write of `contents` to `path`.
/// Tmp file lives at `<path>.heed.tmp` so multiple installers can't collide.
pub(crate) fn atomic_write(path: &Path, contents: &[u8]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("create_dir_all {:?}: {}", parent, e))?;
    }
    let tmp_path = {
        let mut p = path.as_os_str().to_owned();
        p.push(".heed.tmp");
        PathBuf::from(p)
    };
    {
        let mut tmp =
            fs::File::create(&tmp_path).map_err(|e| format!("create tmp {:?}: {}", tmp_path, e))?;
        tmp.write_all(contents)
            .map_err(|e| format!("write tmp: {}", e))?;
        tmp.sync_all().map_err(|e| format!("fsync tmp: {}", e))?;
    }
    fs::rename(&tmp_path, path)
        .map_err(|e| format!("rename {:?} -> {:?}: {}", tmp_path, path, e))?;
    Ok(())
}
