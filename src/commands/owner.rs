//! `heed owner register | unregister`.

use crate::daemon::owners::{self, OwnerRecord};
use crate::state::Cli;

#[derive(Clone, Debug)]
pub struct RegisterArgs {
    pub cli: Cli,
    pub native_thread_id: String,
    pub owner_product: String,
    pub owner_thread_id: Option<String>,
    pub cwd: Option<String>,
}

#[derive(Clone, Debug)]
pub struct UnregisterArgs {
    pub cli: Cli,
    pub native_thread_id: String,
}

pub fn register(args: RegisterArgs) -> Result<(), String> {
    let home = std::env::var("HOME")
        .map(std::path::PathBuf::from)
        .map_err(|_| "HOME env var not set".to_string())?;
    let path = crate::install::owners_path(&home);
    owners::register(
        &path,
        args.cli,
        &args.native_thread_id,
        OwnerRecord {
            owner_product: Some(args.owner_product),
            owner_thread_id: args.owner_thread_id,
            cwd: args.cwd,
        },
    )?;
    println!(
        "registered {}:{} → {} (owners.json updated)",
        args.cli,
        args.native_thread_id,
        path.display()
    );
    Ok(())
}

pub fn unregister(args: UnregisterArgs) -> Result<(), String> {
    let home = std::env::var("HOME")
        .map(std::path::PathBuf::from)
        .map_err(|_| "HOME env var not set".to_string())?;
    let path = crate::install::owners_path(&home);
    let removed = owners::unregister(&path, args.cli, &args.native_thread_id)?;
    if removed {
        println!("unregistered {}:{}", args.cli, args.native_thread_id);
    } else {
        println!("no owner entry for {}:{}", args.cli, args.native_thread_id);
    }
    Ok(())
}
