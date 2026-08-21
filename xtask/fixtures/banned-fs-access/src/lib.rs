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

    nix::fcntl::open(
        path(),
        nix::fcntl::OFlag::O_RDONLY,
        nix::sys::stat::Mode::empty(),
    );
    nix::unistd::mkdir(path(), nix::sys::stat::Mode::empty());
    nix::unistd::unlinkat(
        std::io::stdin(),
        path(),
        nix::unistd::UnlinkatFlags::NoRemoveDir,
    );
    nix::unistd::truncate(path(), 0);
    nix::unistd::chown(path(), None, None);
    nix::unistd::chroot(path());
    nix::sys::stat::mkdirat(std::io::stdin(), path(), nix::sys::stat::Mode::empty());
    nix::sys::stat::utimes(
        path(),
        &nix::sys::time::TimeVal::new(0, 0),
        &nix::sys::time::TimeVal::new(0, 0),
    );

    // SAFETY: never executed; the fixture exists to be linted, not run.
    unsafe {
        let p = c"/".as_ptr();
        libc::open(p, 0);
        libc::openat(0, p, 0);
        libc::creat(p, 0);
        libc::chdir(p);
        libc::getcwd(std::ptr::null_mut(), 0);
        libc::access(p, 0);
        libc::stat(p, std::ptr::null_mut());
        libc::lstat(p, std::ptr::null_mut());
        libc::realpath(p, std::ptr::null_mut());
        libc::readlink(p, std::ptr::null_mut(), 0);
        libc::opendir(p);
        libc::mkdir(p, 0);
        libc::rmdir(p);
        libc::unlink(p);
        libc::rename(p, p);
        libc::link(p, p);
        libc::symlink(p, p);
        libc::truncate(p, 0);
        libc::chmod(p, 0);
        libc::chroot(p);
    }

    nix::unistd::fchdir(std::io::stdin());
    nix::unistd::linkat(
        std::io::stdin(),
        path(),
        std::io::stdin(),
        path(),
        nix::unistd::LinkatFlags::NoSymlinkFollow,
    );
    nix::unistd::mkfifo(path(), nix::sys::stat::Mode::empty());
    nix::unistd::pathconf(path(), nix::unistd::PathconfVar::NAME_MAX);
    nix::fcntl::openat(
        std::io::stdin(),
        path(),
        nix::fcntl::OFlag::O_RDONLY,
        nix::sys::stat::Mode::empty(),
    );
    nix::fcntl::renameat(std::io::stdin(), path(), std::io::stdin(), path());
    nix::fcntl::readlinkat(std::io::stdin(), path());
    nix::sys::stat::fstatat(
        std::io::stdin(),
        path(),
        nix::fcntl::AtFlags::empty(),
    );
    nix::sys::stat::fchmodat(
        std::io::stdin(),
        path(),
        nix::sys::stat::Mode::empty(),
        nix::sys::stat::FchmodatFlags::FollowSymlink,
    );
    nix::sys::stat::mknod(path(), nix::sys::stat::SFlag::S_IFREG, nix::sys::stat::Mode::empty(), 0);
    nix::sys::statvfs::statvfs(path());

    // SAFETY: never executed; the fixture exists to be linted, not run.
    unsafe {
        let p = c"/".as_ptr();
        libc::fopen(p, p);
        libc::freopen(p, p, std::ptr::null_mut());
        libc::tmpfile();
        libc::remove(p);
        libc::renameat(0, p, 0, p);
        libc::linkat(0, p, 0, p, 0);
        libc::symlinkat(p, 0, p);
        libc::readlinkat(0, p, std::ptr::null_mut(), 0);
        libc::faccessat(0, p, 0, 0);
        libc::fchmodat(0, p, 0, 0);
        libc::fchownat(0, p, 0, 0, 0);
        libc::utimensat(0, p, std::ptr::null(), 0);
        libc::utimes(p, std::ptr::null());
        libc::mkdirat(0, p, 0);
        libc::unlinkat(0, p, 0);
        libc::mkfifo(p, 0);
        libc::mkstemp(std::ptr::null_mut());
        libc::mkdtemp(std::ptr::null_mut());
        libc::fchdir(0);
        libc::lchown(p, 0, 0);
        libc::opendir(p);
        libc::fdopendir(0);
        libc::readdir(std::ptr::null_mut());
        libc::pathconf(p, 0);
        libc::execv(p, std::ptr::null());
        libc::execve(p, std::ptr::null(), std::ptr::null());
        libc::execvp(p, std::ptr::null());
        libc::system(p);
        libc::popen(p, p);
    }

    std::os::unix::net::UnixStream::connect(path());
    std::os::unix::net::UnixListener::bind(path());
    std::os::unix::net::UnixDatagram::bind(path());
    let _ = std::os::unix::net::UnixDatagram::unbound().map(|d| d.connect(path()));
    std::os::unix::net::SocketAddr::from_pathname(path());
}

/// `rustix`, the third spelling of the Unix surface. Its `fs` module is
/// `cfg(not(windows))`, and it gates several functions per platform, so the
/// uses that cannot exist everywhere carry the same gate the function does --
/// `resolves_on_this_platform` applies the matching one when deciding which
/// bans must fire.
#[cfg(unix)]
fn rustix_fs() {
    use rustix::fs::{
        Access, AtFlags, CWD, Mode, OFlags, RenameFlags, Timestamps, access, accessat, chmod,
        chmodat, chown, chownat, link, linkat, lstat, mkdir, mkdirat, open, openat, readlink,
        readlinkat, readlinkat_raw, rename, renameat, renameat_with, rmdir, stat, statat, statfs,
        statvfs, symlink, symlinkat, unlink, unlinkat, utimensat,
    };

    fn timestamps() -> Timestamps {
        unimplemented!()
    }

    open(path(), OFlags::empty(), Mode::empty());
    openat(CWD, path(), OFlags::empty(), Mode::empty());
    stat(path());
    lstat(path());
    statat(CWD, path(), AtFlags::empty());
    readlink(path(), Vec::new());
    readlinkat(CWD, path(), Vec::new());
    readlinkat_raw(CWD, path(), &mut [0u8; 1][..]);
    rename(path(), path());
    renameat(CWD, path(), CWD, path());
    renameat_with(CWD, path(), CWD, path(), RenameFlags::empty());
    unlink(path());
    unlinkat(CWD, path(), AtFlags::empty());
    rmdir(path());
    link(path(), path());
    linkat(CWD, path(), CWD, path(), AtFlags::empty());
    symlink(path(), path());
    symlinkat(path(), CWD, path());
    mkdir(path(), Mode::empty());
    mkdirat(CWD, path(), Mode::empty());
    access(path(), Access::EXISTS);
    accessat(CWD, path(), Access::EXISTS, AtFlags::empty());
    statfs(path());
    statvfs(path());
    chmod(path(), Mode::empty());
    chmodat(CWD, path(), Mode::empty(), AtFlags::empty());
    chown(path(), None, None);
    chownat(CWD, path(), None, None, AtFlags::empty());
    utimensat(CWD, path(), &timestamps(), AtFlags::empty());
}

/// The `rustix` entries that only some unixes have.
#[cfg(all(unix, not(target_vendor = "apple")))]
fn rustix_fs_not_on_apple() {
    rustix::fs::mknodat(
        rustix::fs::CWD,
        path(),
        rustix::fs::FileType::Fifo,
        rustix::fs::Mode::empty(),
        0,
    );
    rustix::fs::mkfifoat(rustix::fs::CWD, path(), rustix::fs::Mode::empty());
}

#[cfg(target_vendor = "apple")]
fn rustix_fs_apple_only() {
    rustix::fs::fclonefileat(
        std::io::stdin(),
        rustix::fs::CWD,
        path(),
        rustix::fs::CloneFlags::empty(),
    );
    rustix::fs::getpath(std::io::stdin());
}

#[cfg(target_os = "linux")]
fn rustix_fs_linux_only() {
    rustix::fs::statx(
        rustix::fs::CWD,
        path(),
        rustix::fs::AtFlags::empty(),
        rustix::fs::StatxFlags::empty(),
    );
}

/// `xattr`'s path surface. The crate's `FileExt` form is deliberately *not*
/// banned: a descriptor the vfs opened is the routed answer.
#[cfg(unix)]
fn xattr_by_path() {
    xattr::list(path());
    xattr::get(path(), "user.x");
    xattr::set(path(), "user.x", b"v");
    xattr::remove(path(), "user.x");
    xattr::list_deref(path());
    xattr::get_deref(path(), "user.x");
    xattr::set_deref(path(), "user.x", b"v");
    xattr::remove_deref(path(), "user.x");
}

/// Scratch space outside every mount, and a host path handed to a child.
fn scratch_and_children() {
    tempfile::tempfile();
    tempfile::tempfile_in(path());
    tempfile::tempdir();
    tempfile::tempdir_in(path());
    tempfile::spooled_tempfile(0);
    tempfile::NamedTempFile::new();
    tempfile::NamedTempFile::new_in(path());
    tempfile::TempDir::new();
    tempfile::TempDir::new_in(path());
    tempfile::Builder::new().tempfile();
    tempfile::Builder::new().tempfile_in(path());
    tempfile::Builder::new().tempdir();
    tempfile::Builder::new().tempdir_in(path());
    tempfile::Builder::new().make(|_| Ok(std::fs::File::options()));
    tempfile::Builder::new().make_in(path(), |_| Ok(std::fs::File::options()));
    tempfile::NamedTempFile::with_prefix("p");
    tempfile::NamedTempFile::with_prefix_in("p", path());
    tempfile::NamedTempFile::with_suffix("s");
    tempfile::NamedTempFile::with_suffix_in("s", path());
    tempfile::TempDir::with_prefix("p");
    tempfile::TempDir::with_prefix_in("p", path());
    tempfile::spooled_tempfile_in(0, path());
    tempfile::env::temp_dir();
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
