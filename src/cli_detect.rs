//! Detect installed Claude / Codex CLIs and check for known-bad versions.
//!
//! Heed installs hooks unconditionally per spec §8 (the gate is *config
//! directory existence*, not whether the CLI is on PATH). The exception is
//! Codex's known regression list (§9.2) where a recent build crashes when
//! hook config is present — we must skip Codex install on those.

use std::process::Command;

/// Substrings of `codex --version` output that indicate a build with a
/// crashing-on-hook-config regression. Matched as substring against stdout,
/// trimmed.
const CODEX_KNOWN_BAD_VERSION_PREFIXES: &[&str] = &["codex 0.124.", "codex-cli 0.124."];

/// Returns `Some(reason)` if the installed Codex is on the known-bad list.
/// Returns `None` if Codex isn't installed or the version is safe.
pub fn codex_known_bad_reason() -> Option<String> {
    let output = Command::new("codex").arg("--version").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
    for bad in CODEX_KNOWN_BAD_VERSION_PREFIXES {
        if version.starts_with(bad) {
            return Some(format!(
                "{version} has a hook-config startup regression (skipping)"
            ));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke test: the function returns either Some or None without panicking
    /// regardless of whether Codex is installed.
    #[test]
    fn codex_known_bad_reason_does_not_panic() {
        let _ = codex_known_bad_reason();
    }

    /// Verify the known-bad prefixes match the shape we expect Codex to emit.
    #[test]
    fn known_bad_prefixes_match_versionish_strings() {
        // A future regression check should look like this; if the format ever
        // changes ("codex-cli x.y.z" vs "codex x.y.z"), this list needs an
        // update.
        let synthetic = "codex 0.124.5";
        let matches = CODEX_KNOWN_BAD_VERSION_PREFIXES
            .iter()
            .any(|p| synthetic.starts_with(p));
        assert!(matches);
    }
}
