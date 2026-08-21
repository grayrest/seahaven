//! The shell-level half of D11's default-deny builtin allowlist.
//!
//! These spawn the built brush binary rather than living in `cases/brush`
//! (YAML) because the property under test is *which builtins exist*, which is
//! selected by a command-line flag and cannot be reached from inside a script.
//! Two of them also need a process the shell did not start, and one needs the
//! launcher's own host path — neither of which a YAML case can name.
//!
//! What is asserted is that a denied builtin is **absent from the registry**,
//! not merely refused at dispatch. That distinction is the whole design: the
//! registry's own `disabled` flag is cleared by `enable NAME` from inside the
//! shell, so a policy expressed that way is undone by the script it governs.

#![cfg(unix)]
#![cfg(test)]
#![allow(
    clippy::disallowed_methods,
    reason = "test fixtures are built on the host, which is the one place that is the point"
)]

use std::process::Command;

/// Runs the built brush with the given argv and returns (exit code, stdout, stderr).
fn run_brush(args: &[&str]) -> (Option<i32>, String, String) {
    run_brush_with_env(args, &[])
}

fn run_brush_with_env(args: &[&str], env: &[(&str, &str)]) -> (Option<i32>, String, String) {
    let shell_path = assert_cmd::cargo::cargo_bin!("brush");
    let mut command = Command::new(shell_path);
    command.args(["--norc", "--noprofile"]).args(args);
    for (k, v) in env {
        command.env(k, v);
    }
    let output = command.output().expect("failed to spawn brush");
    (
        output.status.code(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// A builtin the default allowlist denies, and the one the policy exists to
/// keep out: `enable` is what un-does a `disabled` flag.
const DENIED: &str = "enable";

/// Another denied builtin, used where the test needs to *run* `enable` to
/// inspect the registry and so must admit it.
const ALSO_DENIED: &str = "history";

#[test]
fn an_unrestricted_shell_still_has_every_builtin() {
    // The control. Without it, every assertion below could pass because the
    // shell is broken rather than because the policy works.
    let (code, _, _) = run_brush(&["-c", "enable -a > /dev/null"]);
    assert_eq!(code, Some(0), "`enable` must exist under an open policy");
}

#[test]
fn a_denied_builtin_is_absent_by_every_route() {
    // Four routes rather than one: the three dispatch reads (`NAME`,
    // `builtin NAME`, `command -v NAME`) are separate code paths, and `enable`
    // is the fourth because it is the one that could put the name back.
    for script in [
        "enable -a",
        "builtin enable -a",
        "command -v enable",
        "enable enable",
    ] {
        let (code, stdout, _) = run_brush(&["--restrict-builtins", "-c", script]);
        assert_ne!(
            code,
            Some(0),
            "`{script}` must fail when `{DENIED}` is not registered"
        );
        assert!(
            stdout.trim().is_empty(),
            "`{script}` must print nothing about a builtin that does not exist, got {stdout:?}"
        );
    }
}

#[test]
fn enable_cannot_resurrect_a_denied_builtin() {
    // The measured bypass: `enable -n kill` blocks dispatch and `enable kill`
    // restores it, so a policy carried in the `disabled` flag is undone from
    // inside the shell. Absent from the registry, there is nothing to restore.
    let (code, _, stderr) = run_brush(&[
        "--restrict-builtins",
        "--allow-builtin",
        DENIED,
        "-c",
        &format!("{ALSO_DENIED} && echo SHOULD-NOT-REACH"),
    ]);
    assert_ne!(code, Some(0));
    assert!(
        stderr.contains("command not found"),
        "expected `{ALSO_DENIED}` to be missing entirely, got {stderr:?}"
    );

    let (code, stdout, _) = run_brush(&[
        "--restrict-builtins",
        "--allow-builtin",
        DENIED,
        "-c",
        &format!("enable {ALSO_DENIED}; {ALSO_DENIED} && echo SHOULD-NOT-REACH"),
    ]);
    assert_ne!(
        code,
        Some(0),
        "`enable` must not bring a denied builtin back"
    );
    assert!(
        !stdout.contains("SHOULD-NOT-REACH"),
        "the resurrected builtin ran: {stdout:?}"
    );
}

#[test]
fn the_policy_is_not_vacuous() {
    // A gate that cannot fail is worse than no gate. Four of the foundation
    // milestone's nine were built that way, so this one counts.
    let count = |args: &[&str]| -> usize {
        let (_, stdout, _) = run_brush(args);
        stdout.lines().filter(|l| !l.trim().is_empty()).count()
    };
    let open = count(&["-c", "enable -a"]);
    let restricted = count(&[
        "--restrict-builtins",
        "--allow-builtin",
        DENIED,
        "-c",
        "enable -a",
    ]);

    assert!(open > 0, "the open registry must not be empty");
    assert!(
        restricted < open,
        "the allowlist removed nothing: {restricted} of {open} builtins remain"
    );
    assert!(
        restricted > 0,
        "the allowlist removed everything, which would make the other cases pass for the wrong reason"
    );
}

#[test]
fn an_allowed_builtin_still_works() {
    let (code, stdout, _) = run_brush(&["--restrict-builtins", "-c", "echo hello"]);
    assert_eq!(code, Some(0));
    assert_eq!(stdout.trim(), "hello");
}

#[test]
fn kill_refuses_a_process_this_shell_did_not_start() {
    // An allowlist is a statement about names, so it cannot bound what a
    // permitted builtin reaches -- and `kill` is permitted, because a recipe
    // runner needs job control. Measured before this policy existed: the
    // victim died.
    let mut victim = Command::new("/bin/sleep")
        .arg("30")
        .spawn()
        .expect("failed to spawn the victim");
    let pid = victim.id();

    let (code, _, stderr) = run_brush(&["--restrict-builtins", "-c", &format!("kill -TERM {pid}")]);
    assert_ne!(code, Some(0), "signalling a foreign pid must fail");
    assert!(
        stderr.contains("not a job of this shell"),
        "expected the job-table refusal, got {stderr:?}"
    );

    // The point of the test: the process is still there. A non-zero exit code
    // with a dead victim would be a passing assertion about a failed policy.
    assert!(
        matches!(victim.try_wait(), Ok(None)),
        "the victim was signalled anyway"
    );
    let _ = victim.kill();
    let _ = victim.wait();
}

#[test]
fn an_exported_function_in_the_environment_is_refused() {
    // Shellshock's shape. Not a builtin at all, so the list does not reach it;
    // it is gated on the policy being restrictive.
    let injected = ("BASH_FUNC_pwned%%", "() { echo INJECTED; }");

    let (_, stdout, _) = run_brush_with_env(&["-c", "pwned"], &[injected]);
    assert_eq!(
        stdout.trim(),
        "INJECTED",
        "the control failed: bash's behaviour must be kept under an open policy"
    );

    let (code, stdout, _) =
        run_brush_with_env(&["--restrict-builtins", "-c", "pwned"], &[injected]);
    assert_ne!(code, Some(0));
    assert!(
        !stdout.contains("INJECTED"),
        "the environment defined a function in a restricted shell: {stdout:?}"
    );
}

/// The bundled utilities are a *second* registry, process-global and dispatched
/// from `main()` before any shell exists, so these need them compiled in.
#[cfg(feature = "experimental-bundled-coreutils")]
mod bundled {
    use super::{Command, run_brush};

    fn launcher() -> String {
        assert_cmd::cargo::cargo_bin!("brush")
            .to_string_lossy()
            .into_owned()
    }

    #[test]
    fn a_bundled_utility_is_admitted_by_default() {
        // They are the point of the closed world; an allowlist that removed
        // them would leave a shell that can run nothing.
        let (code, stdout, _) = run_brush(&["--restrict-builtins", "-c", "echo hi | cat"]);
        assert_eq!(code, Some(0));
        assert_eq!(stdout.trim(), "hi");
    }

    #[test]
    fn a_denied_bundled_utility_is_refused_on_the_re_exec_route() {
        // The route around a shell-side allowlist. `<launcher> --invoke-bundled
        // find` runs in a fresh process that reads the process-global registry,
        // so keeping `find` out of *this* shell's registry is not enough --
        // D2's predicate has to see the utility name too.
        let launcher = launcher();
        let (code, _, stderr) = run_brush(&[
            "--restrict-builtins",
            "--deny-builtin",
            "find",
            "--closed-world",
            "-c",
            &format!("{launcher} --invoke-bundled find ."),
        ]);
        assert_ne!(
            code,
            Some(0),
            "the dispatch of a denied utility must be refused"
        );
        assert!(
            stderr.contains("external execution is disabled"),
            "expected the closed-world refusal, got {stderr:?}"
        );
    }

    #[test]
    fn an_admitted_bundled_utility_still_dispatches() {
        // The control for the case above: without it, that assertion would pass
        // just as well if the dispatch route were broken outright.
        let launcher = launcher();
        let (code, stdout, _) = run_brush(&[
            "--restrict-builtins",
            "--closed-world",
            "-c",
            &format!("{launcher} --invoke-bundled echo hi"),
        ]);
        assert_eq!(code, Some(0), "an admitted utility must still dispatch");
        assert_eq!(stdout.trim(), "hi");
    }

    #[test]
    fn a_denied_bundled_utility_does_not_fall_through_to_the_host() {
        // Worth pinning because the naive reading is wrong: denying a builtin
        // *promotes* the name to an external lookup, so under an open world
        // `find` becomes the host's `/usr/bin/find`. Only the closed world
        // makes denial mean what it looks like it means.
        let (code, _, stderr) = run_brush(&[
            "--restrict-builtins",
            "--deny-builtin",
            "find",
            "--closed-world",
            "-c",
            "find .",
        ]);
        assert_ne!(code, Some(0));
        assert!(
            stderr.contains("external execution is disabled"),
            "expected the host binary to be refused, got {stderr:?}"
        );

        // And the same command without the closed world reaches the host, which
        // is the trap this case exists to document rather than to fix here.
        let host_find_exists = Command::new("/usr/bin/find").arg("--help").output().is_ok();
        if host_find_exists {
            let (code, _, _) = run_brush(&[
                "--restrict-builtins",
                "--deny-builtin",
                "find",
                "-c",
                "find .",
            ]);
            assert_eq!(
                code,
                Some(0),
                "denying a builtin without a closed world promotes it to the host binary"
            );
        }
    }
}
