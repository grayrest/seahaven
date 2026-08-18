//! The shell-level half of the D2 closed-world suite.
//!
//! These spawn the built brush binary rather than living in `cases/brush`
//! (YAML) because the property under test is *external execution*, and the one
//! program a closed world must still run is the launcher itself — whose host
//! path a YAML file cannot name. They also need to distinguish "refused by the
//! predicate" (exit 126) from "not present in the namespace" (exit 127), which
//! is exactly what the launcher-path cases exercise.
//!
//! What is asserted here is the predicate, not child confinement: a bundled
//! command that runs is proof the shim is *permitted*, not that the child it
//! spawns is itself sandboxed. That is D24's job.

#![cfg(unix)]
#![cfg(test)]
#![allow(
    clippy::disallowed_methods,
    reason = "test fixtures are built on the host, which is the one place that is the point"
)]

use std::path::Path;
use std::process::Command;

/// Runs the built brush with the given argv and returns (exit code, stderr).
fn run_brush(args: &[&str]) -> (Option<i32>, String) {
    let shell_path = assert_cmd::cargo::cargo_bin!("brush");
    let output = Command::new(shell_path)
        .args(args)
        .output()
        .expect("failed to spawn brush");
    (
        output.status.code(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// The message the closed-world refusal renders. Asserting on it keeps a future
/// change that swallows the refusal into a plain "command not found" from
/// passing silently.
const REFUSED: &str = "external execution is disabled";

#[test]
fn open_world_runs_an_external_program() {
    // The control: without the flag, a real external program runs. If this ever
    // fails, a "closed world refuses it" assertion below proves nothing.
    let (code, _) = run_brush(&["--norc", "--noprofile", "-c", "/bin/echo hi"]);
    assert_eq!(code, Some(0), "an external echo should run under identity");
}

#[test]
fn closed_world_refuses_an_external_program() {
    let (code, stderr) = run_brush(&["--norc", "--noprofile", "--closed-world", "-c", "/bin/echo hi"]);
    assert_eq!(
        code,
        Some(126),
        "a refused external is 'found but not executable' (126), not 'not found' (127)"
    );
    assert!(
        stderr.contains(REFUSED),
        "the refusal must be legible, got: {stderr}"
    );
}

#[test]
fn closed_world_refuses_the_launcher_without_the_dispatch_flag() {
    // The escape the two-part predicate exists to close: naming the launcher
    // and asking it to be a fresh shell rather than the bundled dispatch. If
    // only the path were checked, this would start an unconfined brush.
    let launcher = assert_cmd::cargo::cargo_bin!("brush");
    let launcher = launcher.to_string_lossy();
    let script = format!("exec {launcher} -c 'echo PWNED'");
    let (code, stderr) = run_brush(&["--norc", "--noprofile", "--closed-world", "-c", &script]);
    assert_eq!(code, Some(126), "the launcher-as-fresh-shell must be refused");
    assert!(
        !stderr.contains("PWNED"),
        "the fresh shell must never have run, got: {stderr}"
    );
    assert!(stderr.contains(REFUSED), "the refusal must be legible");
}

#[test]
fn open_world_control_runs_the_launcher_as_a_fresh_shell() {
    // The same invocation the previous test refuses runs fine under identity,
    // so it is the closed world doing the refusing and not some other error.
    let launcher = assert_cmd::cargo::cargo_bin!("brush");
    let launcher = launcher.to_string_lossy();
    let script = format!("{launcher} --norc --noprofile -c 'echo OPEN'");
    let (code, stderr) = run_brush(&["--norc", "--noprofile", "-c", &script]);
    assert_eq!(code, Some(0), "under identity the nested shell runs, got: {stderr}");
}

/// A bundled command still runs under a closed world — the shim exemption. Only
/// meaningful when the coreutils are actually bundled in; without the feature
/// there are no shims to exercise.
#[cfg(feature = "experimental-bundled-coreutils")]
#[test]
fn closed_world_still_runs_a_bundled_command() {
    // `ls` is a bundled coreutil dispatched through a shim, not a native
    // builtin (unlike `true`), so under a closed world it exercises the
    // launcher-permit path rather than trivially running in-process. It must
    // succeed where `/bin/echo` was refused.
    let (code, stderr) = run_brush(&["--norc", "--noprofile", "--closed-world", "-c", "ls /"]);
    assert_eq!(
        code,
        Some(0),
        "a bundled command must still run under a closed world, got: {stderr}"
    );
    assert!(
        !stderr.contains(REFUSED),
        "a bundled command must not be refused as external, got: {stderr}"
    );
}

/// A closed world under a restrictive mount whose tree does *not* contain the
/// launcher binary still runs bundled commands: the launcher is named directly,
/// not resolved through the namespace.
#[cfg(feature = "experimental-bundled-coreutils")]
#[test]
fn closed_world_runs_bundled_commands_from_outside_the_mount() {
    let tmp = tempfile::tempdir().expect("tempdir");
    std::fs::write(tmp.path().join("note.txt"), b"hello\n").expect("seed file");
    let mount = format!("/work:{}", tmp.path().display());
    // `cat note.txt` after cd into the mount: a relative path, resolved against
    // the child's cwd, which the parent translates to the host mount directory.
    let (code, stderr) = run_brush(&[
        "--norc",
        "--noprofile",
        "--mount",
        &mount,
        "--closed-world",
        "-c",
        "cd /work && cat note.txt",
    ]);
    assert_eq!(
        code,
        Some(0),
        "a bundled cat must run though the launcher is outside the mount, got: {stderr}"
    );
}

#[test]
fn the_flag_is_documented_in_help() {
    // Cheap guard that the flag exists and is discoverable.
    let (_, _) = run_brush(&["--norc", "--noprofile", "-c", "true"]);
    let shell_path = assert_cmd::cargo::cargo_bin!("brush");
    assert!(Path::new(&shell_path).exists());
    let output = Command::new(shell_path).arg("--help").output().expect("help");
    let help = String::from_utf8_lossy(&output.stdout);
    assert!(
        help.contains("--closed-world"),
        "the --closed-world flag should appear in --help"
    );
}
