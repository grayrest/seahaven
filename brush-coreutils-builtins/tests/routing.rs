//! Proof that a codemod'd fork's filesystem access is actually routed through
//! the vfs (D4).
//!
//! The shell-level path cannot show this yet: a bundled command runs in a child
//! process that is handed the identity namespace (D24 is unbuilt), so a
//! restrictive `--mount` on the parent does not reach it. Here the session is
//! installed directly, in-process, and the bundled `cat` is called against it.
//! If `uu_cat` still reached raw `std::fs`, a host path outside the mount would
//! open regardless of the session; that it does not is the routing.

#![cfg(feature = "coreutils.cat")]
#![expect(
    clippy::expect_used,
    clippy::disallowed_methods,
    clippy::tests_outside_test_module,
    reason = "clippy's allow-unwrap-in-tests reaches `#[test]` bodies but not the \
              helpers beside them, and an integration test crate is not a \
              `cfg(test)` module, so every test here reads as outside one. A \
              failed fixture should abort the test rather than be handled, and \
              the fixture tree is built on the host, which is the point."
)]

use std::ffi::OsString;
use std::sync::{Arc, Mutex};

use brush_vfs::{Access, MountTable, Session, Vfs};

// The ambient session is process-global, so the tests that install it must not
// run concurrently.
static GUARD: Mutex<()> = Mutex::new(());

/// Installs a session that mounts `dir` at `/work` and nothing else.
fn confine_to(dir: &std::path::Path) {
    let mounts = MountTable::builder()
        .mount("/work", dir, Access::ReadWrite)
        .expect("mount")
        .build()
        .expect("build");
    brush_vfs::ambient::install(Session::new(Arc::new(Vfs::new(mounts))));
}

/// Runs the bundled `cat` with the given path argument, returning its exit code.
fn cat(path: &str) -> i32 {
    // See `run_util`: reset the process-global exit code so a prior test's
    // failure does not stick to this call.
    uucore::error::set_exit_code(0);
    let cmds = brush_coreutils_builtins::bundled_commands();
    let cat = cmds.get("cat").expect("cat is bundled under this feature");
    cat(vec![OsString::from("cat"), OsString::from(path)])
}

#[test]
fn a_file_inside_the_mount_is_readable() {
    let _g = GUARD
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("inside.txt"), b"hello\n").unwrap();
    confine_to(tmp.path());

    assert_eq!(
        cat("/work/inside.txt"),
        0,
        "a file in the mount must be readable"
    );
    brush_vfs::ambient::uninstall();
}

#[test]
fn a_host_path_outside_the_mount_is_unreachable() {
    let _g = GUARD
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let tmp = tempfile::tempdir().unwrap();
    // A real file that exists on the host but is not in the namespace. If cat
    // still used raw std::fs it would open this; routed through the vfs, the
    // path names nothing.
    let outside = tmp.path().join("outside.txt");
    std::fs::write(&outside, b"secret\n").unwrap();

    // Mount a *different* empty directory, so `outside` is genuinely unmounted.
    let jail = tempfile::tempdir().unwrap();
    confine_to(jail.path());

    let by_host_path = cat(&outside.to_string_lossy());
    assert_ne!(
        by_host_path, 0,
        "a host path outside the mount must not open"
    );

    // And the virtual spelling of it is equally unreachable.
    assert_ne!(
        cat("/etc/passwd"),
        0,
        "an unmounted virtual path must not open"
    );
    brush_vfs::ambient::uninstall();
}

#[test]
fn climbing_out_of_the_mount_is_refused() {
    let _g = GUARD
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("inside.txt"), b"hello\n").unwrap();
    confine_to(tmp.path());

    // `..` from the mount root cannot escape to the parent directory.
    assert_ne!(
        cat("/work/../inside.txt"),
        0,
        "climbing above the root must fail"
    );
    brush_vfs::ambient::uninstall();
}

/// Runs a bundled command with a single path argument, returning its exit code.
///
/// uutils keep a *process-global* exit code (`uucore::error::set_exit_code`);
/// in production each bundled command is its own child process, so it starts at
/// zero, but running several in one test process would let one util's failure
/// stick to the next. Reset it first so each call is judged on its own.
fn run_util(name: &str, path: &str) -> i32 {
    uucore::error::set_exit_code(0);
    let cmds = brush_coreutils_builtins::bundled_commands();
    let f = cmds.get(name).expect("util is bundled under its feature");
    f(vec![OsString::from(name), OsString::from(path)])
}

/// A utility that has **no filesystem code of its own** must still be confined,
/// because `uucore` is now routed (D4's uucore increment).
///
/// This is the case the milestone exists for, and it is the cleanest available
/// isolation of what routing `uucore` bought. `cargo xtask codemod` over
/// `uu_cksum`'s own sources reports `0 site(s) routed, 0 unrouted`: every byte
/// it reads goes through `uucore::checksum`. So before `uucore` was forked this
/// utility was completely unconfined no matter what the namespace said, and
/// nothing in `uu_cksum` could have been changed to fix it.
///
/// `uu_cksum` is *not* one of the five forked leaves — it is still the stock
/// crates.io crate. It is confined here purely because the `[patch]` repointed
/// the `uucore` underneath it.
#[cfg(feature = "coreutils.cksum")]
#[test]
fn a_utility_with_no_filesystem_code_of_its_own_is_confined_through_uucore() {
    let _g = GUARD
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let tmp = tempfile::tempdir().unwrap();
    let outside = tmp.path().join("outside.txt");
    std::fs::write(&outside, b"secret\n").unwrap();

    let jail = tempfile::tempdir().unwrap();
    std::fs::write(jail.path().join("inside.txt"), b"hello\n").unwrap();
    confine_to(jail.path());

    assert_eq!(
        run_util("cksum", "/work/inside.txt"),
        0,
        "a file in the mount must be checksummable"
    );
    assert_ne!(
        run_util("cksum", &outside.to_string_lossy()),
        0,
        "a host path outside the mount must not be reachable through uucore::checksum"
    );
    brush_vfs::ambient::uninstall();
}

/// Every routed utility, not just `cat`, must be confined: a host path outside
/// the mount is unreachable, and a path inside it works. If any of these still
/// used raw `std::fs`, the outside path would open.
#[cfg(all(
    feature = "coreutils.head",
    feature = "coreutils.wc",
    feature = "coreutils.tac",
    feature = "coreutils.nl",
))]
#[test]
fn the_whole_routed_batch_is_confined() {
    let _g = GUARD
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("inside.txt"), b"one\ntwo\n").unwrap();
    let outside = tmp.path().join("outside.txt");
    std::fs::write(&outside, b"secret\n").unwrap();

    let jail = tempfile::tempdir().unwrap();
    std::fs::write(jail.path().join("inside.txt"), b"one\ntwo\n").unwrap();
    confine_to(jail.path());

    for util in ["cat", "head", "wc", "tac", "nl"] {
        assert_eq!(
            run_util(util, "/work/inside.txt"),
            0,
            "{util}: a file in the mount must be readable"
        );
        assert_ne!(
            run_util(util, &outside.to_string_lossy()),
            0,
            "{util}: a host path outside the mount must not open"
        );
    }
    brush_vfs::ambient::uninstall();
}

/// Every forked utility that reads a path argument, confined in one sweep.
///
/// The batch matters more than any single case: routing is per-crate work, so
/// the failure mode is one utility quietly left on ambient `std::fs` while the
/// rest are routed. A list that grows with the fork set catches that; spot
/// checks do not.
///
/// Each is invoked with a host path that exists but is outside the mount. A
/// utility still using raw `std::fs` opens it and exits 0; a routed one cannot
/// name it at all.
#[cfg(feature = "coreutils.all")]
#[test]
fn the_whole_fork_set_refuses_a_path_outside_the_mount() {
    let _g = GUARD
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let tmp = tempfile::tempdir().unwrap();
    let outside = tmp.path().join("outside.txt");
    std::fs::write(&outside, b"secret\n").unwrap();
    let outside = outside.to_string_lossy().into_owned();

    let jail = tempfile::tempdir().unwrap();
    std::fs::write(jail.path().join("inside.txt"), b"one\ntwo\n").unwrap();
    confine_to(jail.path());

    // Utilities that take a path, read it, and report failure through the exit
    // code. Deliberately excludes ones whose single-argument form does not read
    // a file (`echo`, `printf`), writes rather than reads (`mkdir`, `touch`), or
    // would block.
    //
    // `df` is excluded because it is **not confined, by decision**. Its whole
    // filesystem surface is `filesystem.rs`, which canonicalizes mount *device
    // names* (`/dev/disk1s1`) out of the host mount table -- host introspection,
    // not namespace access, the same class as `uucore::fsext`. Routing it makes
    // `df` report nothing at all: upstream's own `test_dev_name_match` failed
    // with `MountMissing` until the module was exempted. It passed this sweep
    // for an incidental reason, which is worse than failing it.
    //
    // `more` is excluded for a different and more interesting reason: it *is*
    // routed -- both of its sites go through `ambient::exists` and
    // `ambient::open` -- but upstream reports a missing file with
    // `USimpleError::new(0, ..)`, an explicit exit code of zero. So the exit
    // code carries no signal for it and including it would assert the wrong
    // thing. Its routing is evidenced by the codemod's own report (no unrouted
    // sites) rather than by this sweep.
    let readers = [
        "cat",
        "head",
        "wc",
        "tac",
        "nl",
        "cksum",
        "base32",
        "base64",
        "basenc",
        "comm",
        "csplit",
        "cut",
        "expand",
        "fmt",
        "fold",
        "md5sum",
        "od",
        "paste",
        "pr",
        "ptx",
        "readlink",
        "realpath",
        "sha1sum",
        "sha256sum",
        "shuf",
        "sort",
        "split",
        "sum",
        "tail",
        "tsort",
        "unexpand",
        "uniq",
        "b2sum",
        "sha224sum",
        "sha384sum",
        "sha512sum",
        "du",
        "ls",
        "dircolors",
    ];

    let cmds = brush_coreutils_builtins::bundled_commands();
    let mut unconfined = Vec::new();
    for util in readers {
        let Some(f) = cmds.get(util) else { continue };
        uucore::error::set_exit_code(0);
        let code = f(vec![OsString::from(util), OsString::from(&outside)]);
        if code == 0 {
            unconfined.push(util);
        }
    }

    assert!(
        unconfined.is_empty(),
        "these utilities read a host path outside the mount, so they are not \
         routed: {unconfined:?}"
    );
    brush_vfs::ambient::uninstall();
}

/// The other three uutils projects D4 names: `find`, `xargs`, `grep`, `sed`.
///
/// Separate from the coreutils sweep because they are separate upstreams with
/// separate entry shapes -- `findutils` predates the `uumain` convention and
/// takes `&[&str]` -- so a regression in one says nothing about the others.
///
/// `find` is the interesting case. It is a *traversal*, not a single open: it
/// walks from a root and reports what it sees, so an unrouted `find` would
/// enumerate the host tree rather than merely read one file outside the mount.
#[cfg(all(feature = "findutils.all", feature = "textutils.all"))]
#[test]
fn findutils_and_textutils_are_confined() {
    let _g = GUARD
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let tmp = tempfile::tempdir().unwrap();
    let outside = tmp.path().join("outside.txt");
    std::fs::write(&outside, b"secret\n").unwrap();
    let outside = outside.to_string_lossy().into_owned();

    let jail = tempfile::tempdir().unwrap();
    std::fs::write(jail.path().join("inside.txt"), b"needle\n").unwrap();
    confine_to(jail.path());

    let cmds = brush_coreutils_builtins::bundled_commands();
    let run = |name: &str, args: &[&str]| -> i32 {
        uucore::error::set_exit_code(0);
        let f = cmds.get(name).expect("bundled under its feature");
        let mut argv = vec![OsString::from(name)];
        argv.extend(args.iter().map(OsString::from));
        f(argv)
    };

    // Inside the mount, each works.
    assert_eq!(
        run("grep", &["needle", "/work/inside.txt"]),
        0,
        "grep in-mount"
    );
    assert_eq!(
        run("sed", &["s/needle/x/", "/work/inside.txt"]),
        0,
        "sed in-mount"
    );

    // Outside it, none can reach the file.
    assert_ne!(
        run("grep", &["secret", &outside]),
        0,
        "grep must not read outside"
    );
    assert_ne!(
        run("sed", &["s/secret/x/", &outside]),
        0,
        "sed must not read outside"
    );

    // `xargs` reads its argument file through the facade -- its only filesystem
    // site, and one the codemod missed until this survey, since `fs::File::open`
    // through a module alias sat between the two forms the visitor handled.
    assert_ne!(
        run("xargs", &["-a", &outside, "echo"]),
        0,
        "xargs must not read an argument file outside the mount"
    );

    // `find` is the case the walker was built for. Rooted outside the mount it
    // must enumerate *nothing* -- asserted on output rather than exit code,
    // because "enumerated then refused every read" and "never enumerated" are
    // the same exit code and only one of them is confinement.
    assert_ne!(
        run("find", &[&outside]),
        0,
        "find must not walk a host tree outside the mount"
    );
    assert_eq!(run("find", &["/work"]), 0, "find in-mount");

    brush_vfs::ambient::uninstall();
}
