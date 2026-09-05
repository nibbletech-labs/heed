//! `heed service status|unregister` — inspect or remove the SMAppService
//! login-item registration. Hooks and `~/.heed` are never touched here.

use std::path::PathBuf;

use crate::service::{self, legacy, ServiceStatus, AGENT_LABEL};

#[derive(Clone, Copy, Debug)]
pub enum Action {
    Status,
    Unregister,
}

pub fn run(action: Action) -> Result<(), String> {
    match action {
        Action::Status => {
            let home = home_dir()?;
            let status = if service::bundle_context().is_some() {
                service::heed_agent().status()
            } else {
                ServiceStatus::NotFound
            };
            let legacy_present = legacy::legacy_plist_path(&home).exists();
            print!("{}", status_report(status, legacy_present));
            Ok(())
        }
        Action::Unregister => {
            if service::bundle_context().is_none() {
                return Err(
                    "heed service unregister: not running from Heed.app; nothing to unregister"
                        .to_string(),
                );
            }
            let agent = service::heed_agent();
            if !agent.status().is_registered() {
                println!("{AGENT_LABEL}: not registered");
                return Ok(());
            }
            agent.unregister()?;
            println!("{AGENT_LABEL}: unregistered");
            Ok(())
        }
    }
}

/// Exactly two lines: the SMAppService status in Apple's spelling, then
/// whether a legacy `dev.heed.daemon.plist` is still under `~/Library`.
pub fn status_report(status: ServiceStatus, legacy_plist_present: bool) -> String {
    format!(
        "{}\nlegacy plist: {}\n",
        status.as_str(),
        if legacy_plist_present {
            "present"
        } else {
            "absent"
        }
    )
}

fn home_dir() -> Result<PathBuf, String> {
    std::env::var("HOME")
        .map(PathBuf::from)
        .map_err(|_| "HOME env var not set".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_report_is_two_lines() {
        assert_eq!(
            status_report(ServiceStatus::Enabled, true),
            "enabled\nlegacy plist: present\n"
        );
        assert_eq!(
            status_report(ServiceStatus::NotFound, false),
            "notFound\nlegacy plist: absent\n"
        );
    }
}
