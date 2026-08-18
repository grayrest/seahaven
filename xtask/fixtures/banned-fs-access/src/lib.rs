//! Positive control for the filesystem ban in the repository's `clippy.toml`.
//!
//! Every entry in `disallowed-methods` is used exactly once below, so linting
//! this crate must produce exactly one diagnostic per entry. An entry naming a
//! path that no longer resolves -- a typo, or a rename in std -- is silently
//! ignored by clippy, so without this fixture a ban can switch itself off with
//! no signal at all.
//!
//! Nothing here is ever run. Values that would be awkward to obtain without
//! tripping another ban are conjured with `unimplemented!()`; only the types
//! matter.
//!
//! Run by `cargo xtask check ban`. Keep it in sync with `clippy.toml`: adding
//! a ban without adding a use here fails the check.

#![allow(dead_code, deprecated, unused_must_use, clippy::all, clippy::pedantic)]
#![deny(clippy::disallowed_methods)]

use std::path::{Path, PathBuf};

fn path() -> &'static Path {
    unimplemented!()
}

fn permissions() -> std::fs::Permissions {
    unimplemented!()
}

/// The free functions in `std::fs`.
fn free_functions() {
    std::fs::canonicalize(path());
    std::fs::copy(path(), path());
    std::fs::create_dir(path());
    std::fs::create_dir_all(path());
    std::fs::exists(path());
    std::fs::hard_link(path(), path());
    std::fs::metadata(path());
    std::fs::read(path());
    std::fs::read_dir(path());
    std::fs::read_link(path());
    std::fs::read_to_string(path());
    std::fs::remove_dir(path());
    std::fs::remove_dir_all(path());
    std::fs::remove_file(path());
    std::fs::rename(path(), path());
    std::fs::set_permissions(path(), permissions());
    std::fs::soft_link(path(), path());
    std::fs::symlink_metadata(path());
    std::fs::write(path(), "");
}

/// The constructors that mint a handle out of a path.
fn constructors() {
    std::fs::File::open(path());
    std::fs::File::create(path());
    std::fs::File::create_new(path());

    // `File::options` and `OpenOptions::open` are two entries, so they are
    // used in one chain rather than two, to keep the count exact.
    std::fs::File::options().open(path());

    std::fs::OpenOptions::new();

    // Likewise for the pair on `DirBuilder`.
    std::fs::DirBuilder::new().create(path());
}

/// Inherent `Path` methods that do I/O.
fn path_methods() {
    let p: PathBuf = path().to_path_buf();
    p.canonicalize();
    p.exists();
    p.is_dir();
    p.is_file();
    p.is_symlink();
    p.metadata();
    p.read_dir();
    p.read_link();
    p.symlink_metadata();
    p.try_exists();
}

/// Paths built from the process's own ambient position.
fn ambient_paths() {
    std::path::absolute(path());
}

/// The unix extension surface, and the crates beneath std. Gated because the
/// paths do not exist elsewhere -- `cargo xtask check ban` applies the same
/// gate when deciding which entries must fire.
#[cfg(unix)]
fn below_std() {
    std::os::unix::fs::symlink(path(), path());
    std::os::unix::fs::chown(path(), None, None);
    std::os::unix::fs::lchown(path(), None, None);
    std::os::unix::fs::chroot(path());

    nix::unistd::access(path(), nix::unistd::AccessFlags::F_OK);
    nix::unistd::chdir(path());
    nix::unistd::getcwd();
    nix::unistd::unlink(path());
    nix::unistd::symlinkat(path(), std::io::stdin(), path());
    nix::sys::stat::stat(path());
    nix::sys::stat::lstat(path());
    nix::fcntl::readlink(path());
    nix::dir::Dir::open(
        path(),
        nix::fcntl::OFlag::O_RDONLY,
        nix::sys::stat::Mode::empty(),
    );

    // SAFETY: never executed; the fixture exists to be linted, not run.
    unsafe {
        libc::open(c"/".as_ptr(), 0);
    }
    // SAFETY: as above.
    unsafe {
        libc::chdir(c"/".as_ptr());
    }
    // SAFETY: as above.
    unsafe {
        libc::unlink(c"/".as_ptr());
    }
}

/// Scratch space outside every mount, and a host path handed to a child.
fn scratch_and_children() {
    tempfile::tempfile();
    tempfile::tempdir();
    tempfile::NamedTempFile::new();
    std::process::Command::new("x").current_dir(path());
}

/// Process-wide ambient state that names a place on the host.
fn ambient_state() {
    std::env::current_dir();
    std::env::set_current_dir(path());
    std::env::current_exe();
    std::env::home_dir();
    std::env::temp_dir();
}
