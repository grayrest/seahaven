//! The host-layout leaks D6 lists against its own claim.
//!
//! D6 says virtualisation makes host paths *unnameable* — "an escape has no
//! syntax" — and then names five places where that is not true. These are those
//! five. None is a filesystem escape: `cd ~` fails either way. What they leak is
//! host *layout*, which is the thing the sentence forbids.
//!
//! Every case has a control asserting the unconfined answer is unchanged,
//! because the failure mode for all of them is a fix that removes the behaviour
//! everywhere and quietly diverges from bash.

#![cfg(unix)]
#![cfg(test)]
#![allow(
    clippy::disallowed_methods,
    reason = "test fixtures are built on the host, which is the one place that is the point"
)]

use std::process::Command;

/// Runs the built brush and returns (stdout, stderr).
fn run(args: &[&str], clear_home: bool) -> (String, String) {
    let shell_path = assert_cmd::cargo::cargo_bin!("brush");
    let mut command = Command::new(shell_path);
    command.args(["--norc", "--noprofile"]).args(args);
    if clear_home {
        command.env_remove("HOME");
    }
    let output = command.output().expect("failed to spawn brush");
    (
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// A namespace holding one empty directory, which is enough to be confined.
struct Confined {
    _root: tempfile::TempDir,
    mount: String,
}

fn confined() -> Confined {
    let root = tempfile::tempdir().expect("temp dir");
    let dir = root.path().join("work");
    std::fs::create_dir(&dir).expect("mkdir");
    Confined {
        mount: format!("/work:{}:rw", dir.display()),
        _root: root,
    }
}

#[test]
fn tilde_does_not_fall_back_to_the_host_passwd_database() {
    // D31's fail-open hole, and the only place in the tree that failed open:
    // with HOME unset, `home_dir` asked the host and answered `~` with a path
    // the namespace does not contain and cannot reach.
    let c = confined();
    let (stdout, _) = run(&["--mount", &c.mount, "-c", "echo ~"], true);
    assert!(
        !stdout.contains('/'),
        "`~` named a host path with HOME unset: {stdout:?}"
    );

    // And an inherited `HOME` does not rescue it either. D21 puts `HOME` in the
    // "synthesized from policy, never inherited" class, so a host value is
    // denied along with the rest of the environment -- the host's home is not a
    // directory a bare `--mount` namespace contains, and answering `~` with it
    // would name a path the shell cannot reach. A `--project` grant is the case
    // that *has* a home to synthesize from; see `derive_project_namespace`.
    let (stdout, _) = run(&["--mount", &c.mount, "-c", "echo ~"], false);
    assert!(
        !stdout.contains('/'),
        "an inherited HOME reached `~` under a policy: {stdout:?}"
    );
}

#[test]
fn the_host_environment_does_not_survive_a_policy() {
    // D21's denied class is "everything else", so the test names something the
    // shell has no opinion about: an inherited variable that is neither
    // synthesized nor passthrough must simply be gone.
    let c = confined();
    let shell_path = assert_cmd::cargo::cargo_bin!("brush");
    let mut command = std::process::Command::new(shell_path);
    let output = command
        .args(["--norc", "--noprofile", "--mount", &c.mount])
        .args([
            "-c",
            "echo \"[$SSH_AUTH_SOCK][$AWS_SECRET_ACCESS_KEY][$NO_COLOR]\"",
        ])
        .env("SSH_AUTH_SOCK", "/tmp/agent.sock")
        .env("AWS_SECRET_ACCESS_KEY", "hunter2")
        .env("NO_COLOR", "1")
        .output()
        .expect("failed to spawn brush");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    assert_eq!(
        stdout.trim(),
        "[][][1]",
        "denied variables survived, or the passthrough class did not"
    );
}

#[test]
fn another_users_home_is_not_resolved_through_the_host() {
    // `echo ~root` printing /var/root is D6's own example.
    let c = confined();
    let (stdout, _) = run(&["--mount", &c.mount, "-c", "echo ~root"], false);
    assert_eq!(
        stdout.trim(),
        "~root",
        "a namespace has no user database, so the expression stays as written"
    );
}

#[test]
fn shell_is_not_inherited_into_a_confined_shell() {
    // D21 puts SHELL in the "synthesized from policy, never inherited" class.
    // Nothing is synthesized in its place: until D22 builds /bin from the
    // builtin registry there is no path that would resolve.
    let c = confined();
    let (stdout, _) = run(&["--mount", &c.mount, "-c", "echo \"[$SHELL]\""], false);
    assert_eq!(stdout.trim(), "[]");
}

#[test]
fn the_user_and_group_databases_are_empty_to_a_confined_shell() {
    // Deliberately *not* relying on `--restrict-builtins` denying `compgen`,
    // which also closes these. A leak closed by a list whose purpose is
    // something else reopens the day the list changes.
    let c = confined();
    for action in ["-u", "-g", "-A hostname"] {
        let script = format!("compgen {action}");
        let (stdout, _) = run(&["--mount", &c.mount, "-c", &script], false);
        assert!(
            stdout.trim().is_empty(),
            "`{script}` leaked host data: {stdout:?}"
        );
    }

    // And with `compgen` explicitly admitted, so the allowlist is demonstrably
    // not what is doing the work.
    let (stdout, _) = run(
        &[
            "--mount",
            &c.mount,
            "--restrict-builtins",
            "--allow-builtin",
            "compgen",
            "-c",
            "compgen -u",
        ],
        false,
    );
    assert!(
        stdout.trim().is_empty(),
        "the allowlist, not the namespace, was closing this leak: {stdout:?}"
    );
}

/// Controls. Each asserts the unconfined answer is untouched, because the
/// tempting fix for every case above is one that removes the behaviour outright.
mod unconfined {
    use super::run;

    #[test]
    fn tilde_and_another_users_home_still_resolve() {
        let (stdout, _) = run(&["-c", "echo ~; echo ~root"], false);
        let lines: Vec<&str> = stdout.lines().collect();
        assert!(
            lines.first().is_some_and(|l| l.starts_with('/')),
            "`~` must still answer as bash does: {stdout:?}"
        );
        assert!(
            lines.get(1).is_some_and(|l| l.starts_with('/')),
            "`~root` must still answer as bash does: {stdout:?}"
        );
    }

    #[test]
    fn shell_is_still_inherited() {
        let (stdout, _) = run(&["-c", "echo \"[$SHELL]\""], false);
        assert_ne!(stdout.trim(), "[]", "SHELL must survive under identity");
    }

    #[test]
    fn the_user_database_still_answers() {
        let (stdout, _) = run(&["-c", "compgen -u"], false);
        assert!(
            !stdout.trim().is_empty(),
            "compgen -u must still answer under identity"
        );
    }
}
