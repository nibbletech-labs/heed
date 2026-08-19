//! Heed — daemon + CLI for watching Claude Code and Codex threads on the local machine.
//!
//! See the README for an overview; `~/.heed/state.json` is the consumption contract.

pub mod cli_detect;
pub mod codex_binder;
pub mod commands;
pub mod daemon;
pub mod daemon_spawn;
pub mod install;
pub mod liveness;
pub mod state;
pub mod succession;
pub mod tool_display;
pub mod transcript;
pub mod tui;

pub const HEED_VERSION: &str = env!("CARGO_PKG_VERSION");
pub const CLAUDE_HOOK_SCRIPTS_VERSION: &str =
    include_str_trimmed!("../resources/claude-hooks/VERSION");
pub const CODEX_HOOK_SCRIPTS_VERSION: &str =
    include_str_trimmed!("../resources/codex-hooks/VERSION");

/// `include_str!` but with trailing whitespace trimmed at compile time. Uses a macro
/// because `str::trim` isn't `const fn` on stable yet.
#[macro_export]
macro_rules! include_str_trimmed {
    ($path:literal) => {{
        const RAW: &str = include_str!($path);
        const fn trim(s: &str) -> &str {
            let bytes = s.as_bytes();
            let mut end = bytes.len();
            while end > 0 {
                let b = bytes[end - 1];
                if b == b'\n' || b == b'\r' || b == b' ' || b == b'\t' {
                    end -= 1;
                } else {
                    break;
                }
            }
            // SAFETY: trimming ASCII whitespace from a valid UTF-8 prefix is valid UTF-8.
            unsafe {
                std::str::from_utf8_unchecked(std::slice::from_raw_parts(bytes.as_ptr(), end))
            }
        }
        trim(RAW)
    }};
}
