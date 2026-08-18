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

/// Process-wide ambient state that names a place on the host.
fn ambient_state() {
    std::env::current_dir();
    std::env::set_current_dir(path());
    std::env::current_exe();
    std::env::home_dir();
    std::env::temp_dir();
}
