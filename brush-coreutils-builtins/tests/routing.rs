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
    let _g = GUARD.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("inside.txt"), b"hello\n").unwrap();
    confine_to(tmp.path());

    assert_eq!(cat("/work/inside.txt"), 0, "a file in the mount must be readable");
    brush_vfs::ambient::uninstall();
}

#[test]
fn a_host_path_outside_the_mount_is_unreachable() {
    let _g = GUARD.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
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
    assert_ne!(by_host_path, 0, "a host path outside the mount must not open");

    // And the virtual spelling of it is equally unreachable.
    assert_ne!(cat("/etc/passwd"), 0, "an unmounted virtual path must not open");
    brush_vfs::ambient::uninstall();
}

#[test]
fn climbing_out_of_the_mount_is_refused() {
    let _g = GUARD.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("inside.txt"), b"hello\n").unwrap();
    confine_to(tmp.path());

    // `..` from the mount root cannot escape to the parent directory.
    assert_ne!(cat("/work/../inside.txt"), 0, "climbing above the root must fail");
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
    let _g = GUARD.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
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
    let _g = GUARD.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
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
