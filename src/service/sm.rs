//! Thin wrapper over `SMAppService.agent(plistName:)`. The plist name is fixed
//! to `dev.heed.agent.plist` by construction — [`heed_agent`] is the only way
//! to obtain an [`AgentService`], so the legacy `dev.heed.daemon` label can
//! never reach ServiceManagement.

#[cfg(target_os = "macos")]
mod imp {
    use objc2_foundation::{NSError, NSString};
    use objc2_service_management::{SMAppService, SMAppServiceStatus};

    use crate::service::ServiceStatus;

    /// Handle on `SMAppService.agent(plistName:)`. Resolves against the
    /// calling process's main bundle, so it only means something when `heed`
    /// runs from `Heed.app/Contents/MacOS/heed` (the symlink re-exec in
    /// `main()` guarantees that).
    pub struct AgentService {
        plist_name: String,
    }

    impl AgentService {
        pub(super) fn new(plist_name: &str) -> Self {
            AgentService {
                plist_name: plist_name.to_owned(),
            }
        }

        pub fn status(&self) -> ServiceStatus {
            let name = NSString::from_str(&self.plist_name);
            // SAFETY: `name` outlives the call; SMAppService methods are
            // plain ObjC message sends with no threading requirements.
            let raw = unsafe { SMAppService::agentServiceWithPlistName(&name).status() };
            match raw {
                SMAppServiceStatus::Enabled => ServiceStatus::Enabled,
                SMAppServiceStatus::RequiresApproval => ServiceStatus::RequiresApproval,
                SMAppServiceStatus::NotRegistered => ServiceStatus::NotRegistered,
                _ => ServiceStatus::NotFound,
            }
        }

        pub fn register(&self) -> Result<(), String> {
            let name = NSString::from_str(&self.plist_name);
            // SAFETY: as for `status`; the NSError out-param is handled by
            // the binding.
            unsafe { SMAppService::agentServiceWithPlistName(&name).registerAndReturnError() }
                .map_err(|e| describe(&e))
        }

        pub fn unregister(&self) -> Result<(), String> {
            let name = NSString::from_str(&self.plist_name);
            // SAFETY: as for `register`.
            unsafe { SMAppService::agentServiceWithPlistName(&name).unregisterAndReturnError() }
                .map_err(|e| describe(&e))
        }
    }

    fn describe(e: &NSError) -> String {
        format!(
            "{} ({} code {})",
            e.localizedDescription(),
            e.domain(),
            e.code()
        )
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    use crate::service::ServiceStatus;

    /// Unit struct: no fields, so nothing is unread on Linux.
    pub struct AgentService;

    impl AgentService {
        pub(super) fn new(_plist_name: &str) -> Self {
            AgentService
        }

        pub fn status(&self) -> ServiceStatus {
            ServiceStatus::NotFound
        }

        pub fn register(&self) -> Result<(), String> {
            Err("SMAppService is only available on macOS (dev.heed.agent)".into())
        }

        pub fn unregister(&self) -> Result<(), String> {
            Err("SMAppService is only available on macOS (dev.heed.agent)".into())
        }
    }
}

pub use imp::AgentService;

/// The one agent heed owns. The only constructor exposed outside this module.
pub fn heed_agent() -> AgentService {
    AgentService::new(crate::service::AGENT_PLIST_NAME)
}
