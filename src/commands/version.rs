//! `heed version` — print binary + hook script versions.

pub fn run() -> Result<(), String> {
    println!("heed {}", crate::HEED_VERSION);
    println!("  claude hooks: {}", crate::CLAUDE_HOOK_SCRIPTS_VERSION);
    println!("  codex hooks:  {}", crate::CODEX_HOOK_SCRIPTS_VERSION);
    Ok(())
}
