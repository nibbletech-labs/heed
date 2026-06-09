//! launchd plist (macOS) and systemd user unit (Linux) writers for
//! `heed install --service-install`. Pure rendering — the install driver is
//! responsible for actually loading the unit.

use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub struct ServiceUnit {
    pub path: PathBuf,
    pub contents: String,
}

/// Render a macOS launchd plist that runs `heed_binary daemon` as a user agent.
///
/// SPEC §14.4: `~/.heed/` is space-free so we don't need the `/bin/sh -c`
/// wrapper trick that bit Codezilla.
pub fn render_macos_plist(home: &Path, heed_binary: &Path) -> ServiceUnit {
    let log_dir = home.join(".heed");
    let path = home.join("Library/LaunchAgents/dev.heed.daemon.plist");
    let contents = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>dev.heed.daemon</string>
    <key>ProgramArguments</key>
    <array>
        <string>{}</string>
        <string>daemon</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>StandardOutPath</key>
    <string>{}/heedd.out.log</string>
    <key>StandardErrorPath</key>
    <string>{}/heedd.err.log</string>
    <key>ProcessType</key>
    <string>Background</string>
</dict>
</plist>
"#,
        heed_binary.display(),
        log_dir.display(),
        log_dir.display(),
    );
    ServiceUnit { path, contents }
}

/// Render a systemd user unit that runs `heed_binary daemon`.
pub fn render_linux_systemd(home: &Path, heed_binary: &Path) -> ServiceUnit {
    let path = home.join(".config/systemd/user/heed.service");
    let contents = format!(
        r#"[Unit]
Description=Heed activity-detection daemon
After=default.target

[Service]
Type=simple
ExecStart={} daemon
Restart=on-failure
RestartSec=3
StandardOutput=append:%h/.heed/heedd.out.log
StandardError=append:%h/.heed/heedd.err.log

[Install]
WantedBy=default.target
"#,
        heed_binary.display(),
    );
    ServiceUnit { path, contents }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn macos_plist_contains_label_and_binary_path() {
        let unit = render_macos_plist(
            &PathBuf::from("/Users/tom"),
            &PathBuf::from("/usr/local/bin/heed"),
        );
        assert_eq!(
            unit.path,
            PathBuf::from("/Users/tom/Library/LaunchAgents/dev.heed.daemon.plist")
        );
        assert!(unit.contents.contains("<string>dev.heed.daemon</string>"));
        assert!(unit
            .contents
            .contains("<string>/usr/local/bin/heed</string>"));
        assert!(unit.contents.contains("<string>daemon</string>"));
        assert!(unit.contents.contains("<key>RunAtLoad</key>"));
        assert!(unit.contents.contains("/Users/tom/.heed/heedd.out.log"));
    }

    #[test]
    fn linux_systemd_unit_contains_user_install_target() {
        let unit = render_linux_systemd(
            &PathBuf::from("/home/tom"),
            &PathBuf::from("/home/tom/.cargo/bin/heed"),
        );
        assert_eq!(
            unit.path,
            PathBuf::from("/home/tom/.config/systemd/user/heed.service")
        );
        assert!(unit
            .contents
            .contains("ExecStart=/home/tom/.cargo/bin/heed daemon"));
        assert!(unit.contents.contains("WantedBy=default.target"));
        assert!(unit.contents.contains("%h/.heed/heedd.out.log"));
    }
}
