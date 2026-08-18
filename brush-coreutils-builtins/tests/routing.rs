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
