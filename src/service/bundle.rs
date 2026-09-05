//! The in-bundle predicate: is this `heed` running from
//! `Heed.app/Contents/MacOS/heed` with the agent plist beside it?

use std::path::{Path, PathBuf};

use crate::service::AGENT_PLIST_NAME;

/// Where the pieces of a `Heed.app` live, derived from the executable path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BundleLayout {
    /// `/…/Heed.app`
    pub root: PathBuf,
    /// `/…/Heed.app/Contents/MacOS/heed` — the canonical binary the
    /// `~/.heed/bin/heed` symlink points at.
    pub executable: PathBuf,
    /// `/…/Heed.app/Contents/Library/LaunchAgents/dev.heed.agent.plist`
    pub agent_plist: PathBuf,
}

/// Pure path-shape check: `<root>.app/Contents/MacOS/heed`. No filesystem.
/// `None` for any other shape (bare binary, wrong executable name, `.app`
/// not three components up).
pub fn bundle_layout(exe: &Path) -> Option<BundleLayout> {
    if exe.file_name()? != "heed" {
        return None;
    }
    let macos = exe.parent()?;
    if macos.file_name()? != "MacOS" {
        return None;
    }
    let contents = macos.parent()?;
    if contents.file_name()? != "Contents" {
        return None;
    }
    let root = contents.parent()?;
    if root.extension()? != "app" {
        return None;
    }
    Some(BundleLayout {
        root: root.to_path_buf(),
        executable: exe.to_path_buf(),
        agent_plist: root
            .join("Contents/Library/LaunchAgents")
            .join(AGENT_PLIST_NAME),
    })
}

/// `bundle_layout` + "the agent plist exists on disk beside it".
pub fn detect_bundle(exe: &Path) -> Option<BundleLayout> {
    bundle_layout(exe).filter(|b| b.agent_plist.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundle_layout_accepts_app_bundle_executable() {
        let exe =
            Path::new("/Users/t/Library/Application Support/Heed/Heed.app/Contents/MacOS/heed");
        let layout = bundle_layout(exe).expect("in-bundle path is recognised");
        assert_eq!(
            layout.root,
            PathBuf::from("/Users/t/Library/Application Support/Heed/Heed.app")
        );
        assert_eq!(layout.executable, exe.to_path_buf());
        assert_eq!(
            layout.agent_plist,
            layout
                .root
                .join("Contents/Library/LaunchAgents/dev.heed.agent.plist")
        );
    }

    #[test]
    fn bundle_layout_rejects_bare_and_malformed_paths() {
        for p in [
            "/usr/local/bin/heed",
            "/home/t/.cargo/bin/heed",
            "/x/Heed.app/Contents/MacOS/other",
            "/x/Heed.app/heed",
            "/x/Heed/Contents/MacOS/heed",
        ] {
            assert_eq!(bundle_layout(Path::new(p)), None, "{p} must not match");
        }
    }

    #[test]
    fn detect_bundle_requires_agent_plist_on_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("T.app");
        let exe = root.join("Contents/MacOS/heed");
        std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
        std::fs::write(&exe, b"").unwrap();

        assert_eq!(detect_bundle(&exe), None, "no plist -> not a bundle");

        let plist = root.join("Contents/Library/LaunchAgents/dev.heed.agent.plist");
        std::fs::create_dir_all(plist.parent().unwrap()).unwrap();
        std::fs::write(&plist, b"<plist/>").unwrap();

        let layout = detect_bundle(&exe).expect("plist present -> bundle");
        assert_eq!(layout.root, root);
        assert_eq!(layout.agent_plist, plist);
    }
}
