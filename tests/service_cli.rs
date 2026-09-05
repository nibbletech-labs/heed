//! CLI-level tests for the SMAppService-era surface: quiet no-arg exit,
//! `heed service status|unregister`, symlink re-exec safety, and launchd
//! log redirection. Every test runs against a tempdir HOME.

use std::process::Command;

use assert_cmd::cargo::CommandCargoExt;
use assert_cmd::prelude::OutputAssertExt;
use predicates::prelude::*;

fn heed_cmd(home: &std::path::Path) -> Command {
    let mut cmd = Command::cargo_bin("heed").unwrap();
    cmd.env("HOME", home);
    cmd
}

/// Criterion 6: LaunchServices runs a double-clicked bundle with no
/// arguments and no TTY. That must exit 0 and print nothing, not clap help.
/// (assert_cmd pipes stdio, so stdout is not a terminal here.)
#[test]
fn no_args_without_tty_exits_zero_and_prints_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    heed_cmd(tmp.path())
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::is_empty());
}

/// Guard for the symlink re-exec: a symlink to a *bare* binary (not inside
/// an app bundle) must run as-is — no re-exec, no loop.
#[test]
fn symlink_to_bare_binary_does_not_reexec() {
    let tmp = tempfile::tempdir().unwrap();
    let bin_dir = tmp.path().join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    let link = bin_dir.join("heed");
    std::os::unix::fs::symlink(assert_cmd::cargo::cargo_bin("heed"), &link).unwrap();
    Command::new(&link)
        .env("HOME", tmp.path())
        .arg("--version")
        .assert()
        .success()
        .stdout(predicate::str::starts_with("heed "));
}

/// Criterion 7: with `XPC_SERVICE_NAME` set (launchd), the daemon appends its
/// own stdout/stderr to `~/.heed/heedd.{out,err}.log` because the bundle
/// plist carries no StandardOutPath. The startup line is the proof.
#[test]
fn daemon_under_launchd_env_appends_to_heed_logs() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let status = heed_cmd(home)
        .args(["install", "--skip-claude", "--skip-codex", "--no-spawn"])
        .status()
        .unwrap();
    assert!(status.success());

    let mut daemon = heed_cmd(home)
        .arg("daemon")
        .env("XPC_SERVICE_NAME", "dev.heed.agent")
        .env("RUST_LOG", "info")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn daemon");

    let state_path = home.join(".heed/state.json");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !state_path.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(state_path.exists(), "daemon never wrote state.json");

    // SIGINT: ctrlc handles it (SIGTERM is not caught without the
    // `termination` feature).
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(daemon.id() as i32),
        nix::sys::signal::Signal::SIGINT,
    )
    .unwrap();
    daemon.wait().unwrap();

    assert!(
        home.join(".heed/heedd.out.log").exists(),
        "heedd.out.log was not created"
    );
    let err_log =
        std::fs::read_to_string(home.join(".heed/heedd.err.log")).expect("heedd.err.log exists");
    assert!(
        err_log.contains("daemon: started under launchd (dev.heed.agent)"),
        "startup line missing from heedd.err.log:\n{err_log}"
    );
}

/// Criterion 5: exactly one status word, then the legacy-plist line. A bare
/// (non-bundle) binary has no SMAppService identity, so `notFound`.
#[test]
fn service_status_from_bare_binary_prints_not_found() {
    let tmp = tempfile::tempdir().unwrap();
    heed_cmd(tmp.path())
        .args(["service", "status"])
        .assert()
        .success()
        .stdout("notFound\nlegacy plist: absent\n");
}

/// This HOME contains a dev.heed.daemon.plist. Never reuse it for `install`
/// or `install --uninstall`: the plist gate would be satisfied and the binary
/// would `launchctl bootout` the real user agent (launchd domains are
/// per-uid, not per-HOME). `service status` only reads.
#[test]
fn service_status_reports_legacy_plist_present() {
    let tmp = tempfile::tempdir().unwrap();
    let plist = tmp
        .path()
        .join("Library/LaunchAgents/dev.heed.daemon.plist");
    std::fs::create_dir_all(plist.parent().unwrap()).unwrap();
    std::fs::write(&plist, b"<plist/>").unwrap();
    heed_cmd(tmp.path())
        .args(["service", "status"])
        .assert()
        .success()
        .stdout("notFound\nlegacy plist: present\n");
}

/// Criterion 10 (negative path): a bare binary has nothing to unregister and
/// must say so without touching anything.
#[test]
fn service_unregister_from_bare_binary_fails_cleanly() {
    let tmp = tempfile::tempdir().unwrap();
    heed_cmd(tmp.path())
        .args(["service", "unregister"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("not running from Heed.app"));
}
