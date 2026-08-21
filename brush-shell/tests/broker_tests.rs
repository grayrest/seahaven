//! The shell-level half of D24's broker suite.
//!
//! These spawn the built brush binary because the property under test is what a
//! *child process* can reach, and there is no other way to have one. Each case
//! is one of the three transcripts recorded in the plan, inverted.
//!
//! The third case matters most and is the one an eye skips: `cat /work/x`
//! succeeding proves the namespace **arrived**. Without it, a child that
//! received nothing at all would pass every "cannot reach the host" assertion
//! here for exactly the wrong reason.

#![cfg(unix)]
#![cfg(test)]
#![cfg(feature = "experimental-bundled-coreutils")]
#![allow(
    clippy::disallowed_methods,
    reason = "test fixtures are built on the host, which is the one place that is the point"
)]

use std::process::Command;

/// A directory with two files in it, and a host file outside it.
struct Jail {
    _root: tempfile::TempDir,
    mounted: std::path::PathBuf,
    outside: std::path::PathBuf,
}

fn jail() -> Jail {
    let root = tempfile::tempdir().expect("temp dir");
    let jail = root.path().join("jail");
    std::fs::create_dir(&jail).expect("mkdir jail");
    std::fs::write(jail.join("inside.txt"), b"needle\n").expect("write inside");
    let outside = root.path().join("outside.txt");
    std::fs::write(&outside, b"secret\n").expect("write outside");
    Jail {
        _root: root,
        mounted: jail,
        outside,
    }
}

/// Runs the built brush confined to `jail`, with a closed world.
fn confined(jail: &Jail, script: &str) -> (Option<i32>, String, String) {
    let shell_path = assert_cmd::cargo::cargo_bin!("brush");
    let output = Command::new(shell_path)
        .args([
            "--norc",
            "--noprofile",
            "--mount",
            &format!("/work:{}:rw", jail.mounted.display()),
            "--closed-world",
            "-c",
            script,
        ])
        .output()
        .expect("failed to spawn brush");
    (
        output.status.code(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

#[test]
fn a_bundled_utility_cannot_read_a_host_file() {
    let j = jail();
    let (code, stdout, _) = confined(&j, &format!("cat {}", j.outside.display()));
    assert_ne!(code, Some(0), "a host path outside the mount must not open");
    assert!(
        !stdout.contains("secret"),
        "the child read a host file: {stdout:?}"
    );
}

#[test]
fn a_bundled_utility_cannot_enumerate_the_host() {
    // Reading and enumerating fail for different reasons and the second is the
    // one a walk leaks, so both are asserted.
    let j = jail();
    let (_, stdout, _) = confined(&j, "ls /etc");
    assert!(
        stdout.trim().is_empty(),
        "the child listed a host directory: {stdout:?}"
    );

    let (_, stdout, _) = confined(&j, "find /etc");
    assert!(
        stdout.trim().is_empty(),
        "the child walked a host directory: {stdout:?}"
    );
}

#[test]
fn a_bundled_utility_resolves_an_absolute_virtual_path() {
    // The proof the namespace arrived rather than the child being crippled.
    // Before the broker this failed: the child resolved `/work` against the
    // host, where it does not exist.
    let j = jail();
    let (code, stdout, stderr) = confined(&j, "cat /work/inside.txt");
    assert_eq!(code, Some(0), "expected success, stderr: {stderr:?}");
    assert_eq!(stdout.trim(), "needle");
}

#[test]
fn the_mounts_the_child_receives_are_the_ones_the_parent_had() {
    // Not vacuous: a child with an empty namespace passes every "cannot reach
    // the host" case above. This compares what it *can* see against what the
    // parent mounted, which an empty namespace fails.
    let j = jail();
    let (code, stdout, stderr) = confined(&j, "find /work | sort");
    assert_eq!(code, Some(0), "stderr: {stderr:?}");
    let seen: Vec<&str> = stdout.lines().map(str::trim).collect();
    assert_eq!(
        seen,
        vec!["/work", "/work/inside.txt"],
        "the child's namespace must be the parent's, entry for entry"
    );
}

#[test]
fn a_read_only_mount_stays_read_only_in_the_child() {
    // Access mode is one byte on the wire and would be the easiest thing to
    // drop silently, turning every mount writable in the child.
    let j = jail();
    let shell_path = assert_cmd::cargo::cargo_bin!("brush");
    let output = Command::new(shell_path)
        .args([
            "--norc",
            "--noprofile",
            "--mount",
            &format!("/work:{}:ro", j.mounted.display()),
            "--closed-world",
            "-c",
            "cp /work/inside.txt /work/copy.txt",
        ])
        .output()
        .expect("failed to spawn brush");
    assert_ne!(
        output.status.code(),
        Some(0),
        "a read-only mount must refuse a write in the child too"
    );
    assert!(
        !j.mounted.join("copy.txt").exists(),
        "the child wrote into a read-only mount"
    );
}

#[test]
fn redirections_still_cross() {
    // They crossed before the broker and must still. This is the regression
    // that a change to `compose_std_command` would most plausibly cause.
    let j = jail();
    let (code, _, stderr) = confined(&j, "cd /work && cat inside.txt > copied.txt");
    assert_eq!(code, Some(0), "stderr: {stderr:?}");
    assert_eq!(
        std::fs::read_to_string(j.mounted.join("copied.txt")).expect("the redirect landed"),
        "needle\n"
    );
}

#[test]
fn a_child_that_is_told_to_expect_a_session_and_gets_none_fails_closed() {
    // The failure mode the whole design turns on. A child pointed at a
    // rendezvous nobody serves must refuse everything rather than fall back to
    // the host -- that fallback is the hole this milestone closes.
    let shell_path = assert_cmd::cargo::cargo_bin!("brush");
    let output = Command::new(shell_path)
        .args(["--invoke-bundled", "cat", "/etc/hosts"])
        .env(
            "BRUSH_SESSION_RENDEZVOUS",
            "/nonexistent/brush-broker/socket",
        )
        .output()
        .expect("failed to spawn brush");
    assert_ne!(output.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.trim().is_empty(),
        "a child whose handshake failed read a host file: {stdout:?}"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("session broker"),
        "the failure must name the broker rather than look like a missing file: {stderr:?}"
    );
}

#[test]
fn exec_cannot_be_used_to_reach_an_unbrokered_bundled_child() {
    // `exec` leaves no parent to serve the handshake, so a bundled dispatch
    // through it is refused rather than producing an unconfined child.
    let j = jail();
    let launcher = assert_cmd::cargo::cargo_bin!("brush")
        .to_string_lossy()
        .into_owned();
    let (code, stdout, _) = confined(
        &j,
        &format!("exec {launcher} --invoke-bundled cat /etc/hosts"),
    );
    assert_ne!(code, Some(0));
    assert!(
        !stdout.contains("Host Database") && !stdout.contains("localhost"),
        "exec produced an unconfined bundled child: {stdout:?}"
    );
}
