//! Confinement is invisible at the effect boundary.
//!
//! The plan's central gate, and the one its first draft lacked. Every effectful
//! call is driven twice through the [`PlatformEffects`] trait — once under a
//! restrictive mount whose root is a *subdirectory*, once under the identity
//! policy — and the two are required to agree. Where they agree, the sandbox is
//! invisible, which is the whole claim. Where a deliberate escape makes them
//! disagree, the differential must *notice*, which is
//! [`the_differential_notices_an_escape`].
//!
//! This runs with cargo alone -- no Roc toolchain, no cross-language link --
//! which is why it is the gate the milestone's confinement claim leans on: the
//! trait is a seam a Rust test drives directly, so the property is checkable
//! here and now rather than only through a full rocjust build.
//!
//! Two rules borrowed from `brush-shell/tests/confinement_tests.rs`, because
//! they are what make a differential mean anything:
//!
//! - **The mount is a subdirectory.** With `/work` mapped to `tempdir/work`, a
//!   write that escapes lands in `tempdir`, which is *not* the same place a
//!   contained write lands — so the comparison can see an escape. Mapping the
//!   whole tempdir would hide one.
//! - **Effect paths are relative, or virtual-root-relative within the tree.**
//!   The two sessions live at different absolute host paths, so a path-bearing
//!   result (a listing, a canonicalization) differs by that prefix and nothing
//!   else. Stripping each side's own root leaves identical tails.

#![cfg(unix)]
#![cfg(test)]
#![allow(
    clippy::disallowed_methods,
    clippy::expect_used,
    reason = "the differential builds fixtures on the host and compares them to \
              the namespace -- the host is the one place that is the point"
)]

use std::path::Path;

use brush_platform::{PathKind, PlatformEffects, PlatformError, SessionFacts, VfsPlatform};
use brush_vfs::{Access, MountTable, Policy, Session, Vfs, VirtualPath};

/// A comparable summary of one effect's result. Path-bearing variants have had
/// the session root stripped, so the two sides are comparable.
type Outcome = Result<String, PlatformError>;

/// Builds the fixture tree and returns its canonical root.
///
/// ```text
/// <root>/
///   secret.txt          outside the /work mount; reachable only under identity
///   work/               the mount root -- a subdirectory, deliberately
///     data.txt "hello\n"
///     run.sh   (0755)
///     link -> data.txt
///     sub/
///       inner.txt "in\n"
/// ```
fn fixture() -> (tempfile::TempDir, std::path::PathBuf) {
    use std::os::unix::fs::PermissionsExt as _;

    let temp = tempfile::tempdir().expect("temp dir");
    // Canonical so the identity session's cwd has no symlink prefix to fight
    // canonicalize's output (macOS `/var` -> `/private/var`).
    let root = temp.path().canonicalize().expect("canonicalize root");
    let work = root.join("work");
    std::fs::create_dir(&work).expect("mkdir work");
    std::fs::write(root.join("secret.txt"), b"secret\n").expect("write secret");
    std::fs::write(work.join("data.txt"), b"hello\n").expect("write data");
    std::fs::write(work.join("run.sh"), b"#!/bin/sh\n").expect("write run.sh");
    std::fs::set_permissions(work.join("run.sh"), std::fs::Permissions::from_mode(0o755))
        .expect("chmod run.sh");
    std::os::unix::fs::symlink("data.txt", work.join("link")).expect("symlink");
    std::fs::create_dir(work.join("sub")).expect("mkdir sub");
    std::fs::write(work.join("sub").join("inner.txt"), b"in\n").expect("write inner");
    (temp, work)
}

/// The confined host: `/work` maps to the fixture's `work/` subdirectory.
fn confined(work: &Path) -> (VfsPlatform, String) {
    let mounts = MountTable::builder()
        .mount("/work", work, Access::ReadWrite)
        .expect("mount")
        .build()
        .expect("build");
    let mut session = Session::new(std::sync::Arc::new(Vfs::new(mounts)));
    session.set_cwd("/work").expect("cd /work");
    (
        VfsPlatform::new(session, SessionFacts::neutral()),
        "/work".to_owned(),
    )
}

/// The identity host: the whole host filesystem, cwd inside the fixture.
fn identity(work: &Path) -> (VfsPlatform, String) {
    let mounts = Policy::identity().expect("identity policy");
    let mut session = Session::new(std::sync::Arc::new(Vfs::new(mounts)));
    let cwd = work.to_str().expect("utf-8 path");
    session.set_cwd(cwd).expect("cd into fixture");
    (
        VfsPlatform::new(session, SessionFacts::neutral()),
        cwd.to_owned(),
    )
}

/// Strips the session root from a virtual path, so the two sides are comparable.
fn strip(path: &str, root: &str) -> String {
    path.strip_prefix(root).unwrap_or(path).to_owned()
}

/// Runs the full script of effects against one host, returning a labelled
/// outcome per call. `root` is that host's cwd, stripped from path results.
///
/// The script mutates the tree, so both hosts must run the identical sequence
/// for the comparison to hold. Every effect on the trait is exercised; the
/// enumeration test asserts that against a list it owns.
fn script(host: &VfsPlatform, root: &str) -> Vec<(&'static str, Outcome)> {
    let paths = |r: Effect<Vec<String>>| {
        r.map(|mut v| {
            v.sort();
            v.iter()
                .map(|p| strip(p, root))
                .collect::<Vec<_>>()
                .join(",")
        })
    };
    vec![
        ("path_type file", host.path_type("data.txt").map(kind)),
        ("path_type dir", host.path_type("sub").map(kind)),
        ("path_type symlink", host.path_type("link").map(kind)),
        ("path_type missing", host.path_type("nope").map(kind)),
        ("read_utf8", host.file_read_utf8("data.txt")),
        (
            "read_bytes len",
            host.file_read_bytes("data.txt")
                .map(|b| b.len().to_string()),
        ),
        (
            "is_executable false",
            host.file_is_executable("data.txt").map(|b| b.to_string()),
        ),
        (
            "is_executable true",
            host.file_is_executable("run.sh").map(|b| b.to_string()),
        ),
        (
            "is_executable missing",
            host.file_is_executable("nope").map(|b| b.to_string()),
        ),
        ("dir_list", paths(host.dir_list("."))),
        (
            "canonicalize link",
            host.path_canonicalize("link").map(|p| strip(&p, root)),
        ),
        (
            "canonicalize dotdot",
            host.path_canonicalize("sub/../data.txt")
                .map(|p| strip(&p, root)),
        ),
        ("create_all", host.dir_create_all("made/deep").map(unit)),
        ("path_type made", host.path_type("made/deep").map(kind)),
        // Effects added with the Roc host (step 9). Each must be as invisible to
        // confinement as the originals -- the differential holds them to it.
        ("dir_create", host.dir_create("solo").map(unit)),
        (
            "write_bytes",
            host.file_write_bytes("bin.dat", b"\x00\x01\x02\x03")
                .map(unit),
        ),
        (
            "read_bytes after write",
            host.file_read_bytes("bin.dat").map(|b| b.len().to_string()),
        ),
        (
            "size",
            host.file_size_in_bytes("bin.dat").map(|n| n.to_string()),
        ),
        (
            "is_readable",
            host.file_is_readable("data.txt").map(|b| b.to_string()),
        ),
        (
            "is_writable",
            host.file_is_writable("data.txt").map(|b| b.to_string()),
        ),
        (
            "hard_link",
            host.file_hard_link("data.txt", "hardlink").map(unit),
        ),
        ("rename", host.file_rename("hardlink", "renamed").map(unit)),
        // D46: the time effects are the deferred Unsupported on both sides, so
        // they agree -- which is exactly the invisibility the gate asserts.
        (
            "time_accessed",
            host.file_time_accessed("data.txt").map(|n| n.to_string()),
        ),
        (
            "time_modified",
            host.file_time_modified("data.txt").map(|n| n.to_string()),
        ),
        (
            "time_created",
            host.file_time_created("data.txt").map(|n| n.to_string()),
        ),
        (
            "write_utf8",
            host.file_write_utf8("out.txt", "written").map(unit),
        ),
        ("read back", host.file_read_utf8("out.txt")),
        (
            "set_executable on",
            host.file_set_executable("out.txt", true).map(unit),
        ),
        (
            "is_executable after",
            host.file_is_executable("out.txt").map(|b| b.to_string()),
        ),
        ("delete file", host.file_delete("out.txt").map(unit)),
        ("delete empty", host.dir_delete_empty("made/deep").map(unit)),
        ("delete all", host.dir_delete_all("made").map(unit)),
    ]
}

type Effect<T> = Result<T, PlatformError>;

fn kind(k: PathKind) -> String {
    format!("{k:?}")
}

fn unit((): ()) -> String {
    "ok".to_owned()
}

#[test]
fn confinement_is_invisible() {
    let (_temp, work) = fixture();
    let (confined_host, confined_root) = confined(&work);
    let confined_out = script(&confined_host, &confined_root);

    // A fresh fixture for the identity run, so the first run's mutations do not
    // leak into the second and mask a divergence.
    let (_temp2, work2) = fixture();
    let (identity_host, identity_root) = identity(&work2);
    let identity_out = script(&identity_host, &identity_root);

    assert_eq!(
        confined_out.len(),
        identity_out.len(),
        "the two runs executed different scripts"
    );
    for ((label, confined), (_, identity)) in confined_out.iter().zip(&identity_out) {
        assert_eq!(
            confined, identity,
            "`{label}` differs: confined {confined:?} vs identity {identity:?}"
        );
    }
}

#[test]
fn the_differential_notices_an_escape() {
    // The non-vacuity half. A gate that agrees on everything proves nothing
    // unless it can also disagree. The escape is a `..` out of the mount: the
    // confined host cannot name the parent (NotFound), the identity host reads
    // it (Ok) -- so the two disagree, which is the differential working.
    let (_temp, work) = fixture();
    let (confined_host, _) = confined(&work);
    let (identity_host, _) = identity(&work);

    let confined = confined_host.file_read_utf8("../secret.txt");
    let identity = identity_host.file_read_utf8("../secret.txt");

    // The control: the escape genuinely succeeds under identity. Without it,
    // "the two disagree" could mean both are broken in different ways.
    assert_eq!(
        identity.as_deref(),
        Ok("secret\n"),
        "the escape must genuinely read the secret under identity, or the test proves nothing"
    );
    assert_eq!(
        confined,
        Err(PlatformError::NotFound),
        "the confined host named a file outside its mount"
    );
    assert_ne!(
        confined, identity,
        "the differential did not notice the escape"
    );
}

#[test]
fn cwd_is_a_virtual_path_and_set_cwd_does_not_move_the_process() {
    // Gate 5. The weaker assertion -- that two sessions have different cwds --
    // passes while the guest is handed host layout. This asserts the answer is
    // a *virtual* path, and that moving the session leaves the host process
    // where it was (D15).
    let (_temp, work) = fixture();
    let (mut host, _) = confined(&work);

    let before = std::env::current_dir().expect("host cwd");

    assert_eq!(host.env_cwd(), "/work");
    host.env_set_cwd("sub").expect("cd sub");
    assert_eq!(
        host.env_cwd(),
        "/work/sub",
        "cwd must be the virtual path, not a host one"
    );
    assert!(
        !host.env_cwd().contains(work.to_str().expect("utf8")),
        "cwd leaked a host path: {}",
        host.env_cwd()
    );

    // The host process did not move.
    assert_eq!(
        std::env::current_dir().expect("host cwd"),
        before,
        "env_set_cwd moved the host process"
    );

    // An escape via set_cwd is refused, not silently followed to the host.
    assert!(
        host.env_set_cwd("../..").is_err() || host.env_cwd().starts_with("/work"),
        "set_cwd walked out of the mount"
    );
}

#[test]
fn env_reads_the_policy_set_not_the_host_process() {
    // The platform-crate analogue of the shell's host-leak tests. The effects
    // read `SessionFacts`, which the launcher reduced to D21's classes -- so a
    // variable the host process holds is not visible through `env_var` or
    // `env_dict` unless policy put it there. Gate 7's "one policy object" is at
    // the shell level; this pins that the effect layer reads the reduced set
    // rather than reaching around it to `std::env`.
    let (_temp, work) = fixture();
    let (host, _) = confined(&work);

    // The neutral facts carry no environment, and the host process's own
    // variables (PATH is always set) are not reachable through the effects.
    assert_eq!(host.env_var("PATH"), None, "env_var reached the host PATH");
    assert!(
        host.env_dict().is_empty(),
        "env_dict returned host variables: {:?}",
        host.env_dict()
    );
}

#[test]
fn declared_facts_are_reported_verbatim() {
    // `platform!`, `pid!`, `num_cpus!`, `temp_dir!` and `exe_path!` are policy,
    // not the machine. A launcher chooses them; the effect returns exactly what
    // was chosen, which is what keeps `platform!` from disclosing the host.
    use brush_platform::{PlatformTarget, TargetArch, TargetOs};

    let (_temp, work) = fixture();
    let mounts = MountTable::builder()
        .mount("/work", &work, Access::ReadWrite)
        .expect("mount")
        .build()
        .expect("build");
    let mut session = Session::new(std::sync::Arc::new(Vfs::new(mounts)));
    session.set_cwd("/work").expect("cd");

    let mut facts = SessionFacts::neutral();
    facts.platform = PlatformTarget {
        os: TargetOs::MacOs,
        arch: TargetArch::Aarch64,
    };
    facts.pid = 4242;
    facts.num_cpus = 3;
    facts
        .env
        .insert("JUST_CHOOSER".to_owned(), "fzf".to_owned());
    let host = VfsPlatform::new(session, facts);

    assert_eq!(
        host.env_platform(),
        PlatformTarget {
            os: TargetOs::MacOs,
            arch: TargetArch::Aarch64
        }
    );
    assert_eq!(host.env_pid(), 4242);
    assert_eq!(host.env_num_cpus(), 3);
    assert_eq!(host.env_var("JUST_CHOOSER").as_deref(), Some("fzf"));
    assert_eq!(host.env_exe_path(), "/bin/just");
    assert_eq!(host.env_temp_dir(), "/tmp");
}

#[test]
fn a_grammar_rejected_path_is_not_found() {
    // D12/D45's concession, measured. `VirtualPath` rejects a colon, a Windows
    // reserved device name and a trailing dot on every platform -- so a
    // justfile that names `notes:draft` or `con.txt` builds under basic-cli and
    // is unnameable here. The plan calls measuring this step 2's first task.
    //
    // Unnameable resolves to NotFound, not a distinct error: an unnameable path
    // is not-found from inside the namespace however it came to be so, which is
    // the same answer an out-of-grant path gets and the reason neither is an
    // existence oracle.
    let (_temp, work) = fixture();
    let (host, _) = confined(&work);

    for rejected in ["notes:draft", "con.txt", "trailing.", "a\\b"] {
        // Proven grammar-rejected, not merely absent -- otherwise a missing
        // file would satisfy the NotFound assertion for the wrong reason.
        assert!(
            VirtualPath::new(&format!("/work/{rejected}")).is_err(),
            "`{rejected}` is expected to be rejected by the grammar"
        );
        assert_eq!(
            host.file_read_utf8(rejected),
            Err(PlatformError::NotFound),
            "`{rejected}` is rejected by the grammar and must read as NotFound"
        );
        assert_eq!(
            host.path_type(rejected),
            Err(PlatformError::NotFound),
            "`{rejected}` must probe as NotFound"
        );
    }

    // The control: a name the grammar accepts and that is present resolves.
    assert_eq!(host.path_type("data.txt"), Ok(PathKind::File));
}

#[test]
fn the_script_exercises_every_filesystem_effect() {
    // Enumeration, not sampling (gate 4). The `ls`/`cp`/`du` regression got in
    // because a routing gate exercised some calls and not others; this asserts
    // the differential touches every effect on the trait, by name, against a
    // list the test owns.
    let (_temp, work) = fixture();
    let (host, root) = confined(&work);
    let labels: std::collections::BTreeSet<&str> =
        script(&host, &root).into_iter().map(|(l, _)| l).collect();

    // Each filesystem effect, mapped to the label(s) that exercise it. If an
    // effect is added to the trait, it belongs here and in the script.
    let effects_covered = [
        "path_type file",      // path_type
        "read_utf8",           // file_read_utf8
        "read_bytes len",      // file_read_bytes
        "write_utf8",          // file_write_utf8
        "dir_list",            // dir_list
        "create_all",          // dir_create_all
        "delete file",         // file_delete
        "canonicalize link",   // path_canonicalize
        "set_executable on",   // file_set_executable
        "is_executable false", // file_is_executable
        "delete empty",        // dir_delete_empty
        "delete all",          // dir_delete_all
        "dir_create",          // dir_create
        "write_bytes",         // file_write_bytes
        "size",                // file_size_in_bytes
        "is_readable",         // file_is_readable
        "is_writable",         // file_is_writable
        "hard_link",           // file_hard_link
        "rename",              // file_rename
        "time_accessed",       // file_time_accessed
        "time_modified",       // file_time_modified
        "time_created",        // file_time_created
    ];
    assert_eq!(
        effects_covered.len(),
        22,
        "there are 22 filesystem effects; update this when the trait grows"
    );
    for label in effects_covered {
        assert!(
            labels.contains(label),
            "the script does not exercise `{label}`"
        );
    }
}
