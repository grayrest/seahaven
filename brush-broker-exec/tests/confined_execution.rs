//! The broker-backed executor, end to end against a real trampoline.
//!
//! Unlike the platform crate's differential (which drives the effect trait
//! directly), this spawns an actual process — the `brush` binary, which is a
//! bundled-dispatch trampoline exactly as the linked Roc app will be — and hands
//! it a session over the real D24 broker. So it exercises the whole path the unit
//! tests cannot: `Rendezvous::create`, the spawn, `serve`'s pid check and
//! `SCM_RIGHTS` transfer, and the child confining a bundled utility to the served
//! mounts.
//!
//! The confinement claim is the point. A `cat` of a file *inside* the served
//! mount succeeds; a `cat` of a host path *outside* it fails — driven entirely by
//! which mounts the executor served, not by any flag on the child.

#![cfg(unix)]
#![cfg(test)]
#![allow(
    clippy::disallowed_methods,
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "a test builds fixtures on the host and asserts against a real child"
)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use brush_broker_exec::BrokerExecutor;
use brush_platform::{Cmd, Executor, Exit, IoPlan};
use brush_vfs::{Access, MountTable, Session, Vfs};

/// The dispatch flag the trampoline recognizes (`brush-shell`'s
/// `bundled::DISPATCH_FLAG`). Kept as a literal here rather than depending on the
/// whole shell for one constant; the test fails loudly if it ever drifts.
const DISPATCH_FLAG: &str = "--invoke-bundled";

/// The `brush` binary, the trampoline this test spawns. Cargo builds workspace
/// binaries into `<target>/debug/`; the test binary itself lives one directory
/// deeper, in `deps/`.
fn trampoline() -> PathBuf {
    let mut dir = std::env::current_exe().expect("test exe path");
    dir.pop(); // the test binary's file name
    if dir.ends_with("deps") {
        dir.pop();
    }
    let brush = dir.join("brush");
    assert!(
        brush.exists(),
        "the `brush` trampoline is not built at {}; run `cargo build -p brush-shell --bin brush`",
        brush.display()
    );
    brush
}

/// A fixture: `work/inside.txt` inside the mount, `secret.txt` outside it.
fn fixture() -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().expect("temp dir");
    let root = temp.path().canonicalize().expect("canonicalize");
    let work = root.join("work");
    std::fs::create_dir(&work).expect("mkdir work");
    std::fs::write(work.join("inside.txt"), b"inside the mount\n").expect("write inside");
    std::fs::write(root.join("secret.txt"), b"a secret\n").expect("write secret");
    (temp, work)
}

/// A session with `work/` mounted read-write at `/work`, cwd `/work`.
fn confined_session(work: &Path) -> Session {
    let mounts = MountTable::builder()
        .mount("/work", work, Access::ReadWrite)
        .expect("mount")
        .build()
        .expect("build");
    let mut session = Session::new(Arc::new(Vfs::new(mounts)));
    session.set_cwd("/work").expect("cd /work");
    session
}

fn executor(work: &Path) -> BrokerExecutor {
    BrokerExecutor::new(confined_session(work), trampoline(), DISPATCH_FLAG)
}

#[test]
fn a_bundled_command_reads_a_file_inside_the_mount() {
    let (_temp, work) = fixture();
    let mut exec = executor(&work);

    let result = exec
        .run(&Cmd::new("cat").args(["inside.txt"]), b"")
        .expect("run cat");

    assert_eq!(
        result.exit,
        Exit::Code(0),
        "cat of an in-mount file succeeds"
    );
    assert_eq!(
        result.stdout, b"inside the mount\n",
        "the child read the file through the served mount"
    );
}

#[test]
fn confinement_holds_a_host_path_outside_the_mount_is_unreachable() {
    // The load-bearing assertion: the child was served only `/work`, so a host
    // path outside it cannot be read -- and nothing but the served session makes
    // that so. `/etc/hosts` exists on the host and is world-readable; an
    // unconfined `cat` would print it.
    let (_temp, work) = fixture();
    let mut exec = executor(&work);

    let result = exec
        .run(&Cmd::new("cat").args(["/etc/hosts"]), b"")
        .expect("run cat");

    assert_ne!(
        result.exit,
        Exit::Code(0),
        "cat of a host path outside the mount must fail"
    );
    assert!(
        result.stdout.is_empty(),
        "no host bytes escaped the mount: {:?}",
        String::from_utf8_lossy(&result.stdout)
    );
}

#[test]
fn the_parent_climbing_out_of_the_mount_is_also_unreachable() {
    // The sibling `secret.txt` sits one level above the mount root. A `..` out of
    // `/work` cannot name it -- the served namespace has no parent to climb to.
    let (_temp, work) = fixture();
    let mut exec = executor(&work);

    let result = exec
        .run(&Cmd::new("cat").args(["../secret.txt"]), b"")
        .expect("run cat");

    assert_ne!(result.exit, Exit::Code(0), "the escape must fail");
    assert!(
        !result.stdout.windows(6).any(|w| w == b"secret"),
        "the secret escaped: {:?}",
        String::from_utf8_lossy(&result.stdout)
    );
}

#[test]
fn stdin_is_the_bytes_the_guest_supplied() {
    // `cat` with no argument reads stdin; the executor feeds it the bytes the
    // guest passed, and nothing of the parent's own stdin.
    let (_temp, work) = fixture();
    let mut exec = executor(&work);

    let result = exec
        .run(&Cmd::new("cat"), b"piped through the broker\n")
        .expect("run cat");

    assert_eq!(result.exit, Exit::Code(0));
    assert_eq!(result.stdout, b"piped through the broker\n");
}

#[test]
fn an_unknown_command_fails_promptly_not_by_broker_timeout() {
    // The dispatched child connects to the broker before it looks up the command
    // name, so an unknown command is a prompt non-zero exit the parent collects
    // -- not a child that exits without connecting and leaves the parent waiting
    // out the 10s accept timeout, reported as a broker error.
    let (_temp, work) = fixture();
    let mut exec = executor(&work);

    let start = std::time::Instant::now();
    let result = exec
        .run(&Cmd::new("definitely-not-a-bundled-utility"), b"")
        .expect("an unknown command is a completed run, not a broker error");
    let elapsed = start.elapsed();

    assert_ne!(
        result.exit,
        Exit::Code(0),
        "an unknown command exits non-zero"
    );
    assert!(
        elapsed < std::time::Duration::from_secs(5),
        "the run took {elapsed:?}; the broker accept timeout is 10s, so this is the timeout bug"
    );
}

#[test]
fn a_nonzero_exit_is_a_successful_run_not_an_error() {
    // A command that runs and exits non-zero is `Ok` with a non-zero code, not an
    // `Err` -- the error channel is for a command that could not be run at all.
    // `false` is a bundled utility that exits 1.
    let (_temp, work) = fixture();
    let mut exec = executor(&work);

    let result = exec.run(&Cmd::new("false"), b"").expect("run false");
    assert_eq!(result.exit, Exit::Code(1));
}

/// The `IoPlan` is the host's to apply; the executor always captures. This is a
/// compile-time reminder that the trait returns captured output regardless of
/// how the host will present it.
#[test]
fn the_executor_always_captures() {
    let _ = IoPlan::inherit();
    let (_temp, work) = fixture();
    let mut exec = executor(&work);
    let result = exec
        .run(&Cmd::new("cat").args(["inside.txt"]), b"")
        .unwrap();
    assert!(!result.stdout.is_empty());
}
