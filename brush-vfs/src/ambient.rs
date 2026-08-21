//! The codemod's target: a `std::fs`-shaped facade over a process-global
//! session.
//!
//! D4 forks the coreutils and routes their filesystem access through the vfs. A
//! utility like `uu_cat` calls `std::fs::File::open(path)` deep in its call
//! stack, with no session handle in scope, and D34 requires the rewrite be
//! *signature-preserving* — so the target cannot be a method on a `Session` the
//! utility would have to carry. It is instead a set of free functions that read
//! a **process-global session**, which is sound because each bundled utility
//! runs in its own child process (D2's re-invocation): one process, one
//! session, installed once at startup.
//!
//! The functions mirror `std::fs`'s free-function surface deliberately. A
//! utility that writes `use std::fs;` and calls `fs::metadata(p)` is rewritten
//! by swapping the import to `use brush_vfs::ambient as fs;` — every call site
//! is then byte-identical. Only inherent-method calls (`p.metadata()`,
//! `File::open(p)`) need a real rewrite, into the free function of the same
//! name here.
//!
//! Everything fails closed: with no session installed, every fallible call is an
//! error and every predicate is `false`. A utility reaching the filesystem
//! before a session exists is a bug, and the safe direction to fail is shut.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::RwLock;

use crate::fs::{OpenMode, Vfs};
use crate::path::VirtualPath;
use crate::session::Session;

/// The one session every ambient call resolves against.
///
/// A `RwLock` rather than a `OnceLock`: a utility may `cd`, which moves the
/// session's working directory, and the reader clones a cheap snapshot (an
/// `Arc` and a `PathBuf`) so a long readdir does not hold the lock.
static SESSION: RwLock<Option<Session>> = RwLock::new(None);

/// Installs the session all ambient filesystem calls resolve against.
///
/// The child process a bundled utility runs in calls this once at startup,
/// before the utility touches the filesystem. Installing again replaces it,
/// which is what a test that runs utilities in-process one after another wants.
pub fn install(session: Session) {
    // A poisoned lock means a previous holder panicked mid-write; the stored
    // session is still structurally valid to replace, so recover rather than
    // propagate a panic into every later filesystem call.
    let mut guard = SESSION
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    *guard = Some(session);
}

/// Removes the installed session, returning the facade to its fail-closed state.
///
/// Exists for tests that must not leak a session into the next one; ordinary
/// process teardown does not need it.
pub fn uninstall() {
    let mut guard = SESSION
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    *guard = None;
}

/// A cheap snapshot of the installed session, or an error if none is installed.
fn current() -> io::Result<Session> {
    let guard = SESSION
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    guard.clone().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::PermissionDenied,
            "no vfs session is installed; filesystem access is not permitted",
        )
    })
}

/// Resolves a caller-supplied path against the installed session's working
/// directory, applying the virtual-path grammar.
fn resolve(session: &Session, path: impl AsRef<Path>) -> io::Result<VirtualPath> {
    let path = path.as_ref();
    let text = path.to_str().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("path is not valid UTF-8: {}", path.display()),
        )
    })?;
    session
        .resolve(text)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e.to_string()))
}

/// Runs `op` against the installed session and a resolved path.
fn with<T>(
    path: impl AsRef<Path>,
    op: impl FnOnce(&Vfs, &VirtualPath) -> io::Result<T>,
) -> io::Result<T> {
    let session = current()?;
    let vpath = resolve(&session, path)?;
    op(session.vfs(), &vpath)
}

// -- Where we are ----------------------------------------------------------

/// The session's working directory, as a virtual path.
///
/// The rewrite target for `std::env::current_dir()` in code that goes on to
/// build an absolute path from it. `uucore::fs::canonicalize` is the motivating
/// case: it joins a relative argument onto the process cwd before resolving,
/// and leaving that join on the *host* cwd made the result half-virtual — the
/// prefix from the host, the existence checks from the namespace. A `Session`
/// carries its own cwd precisely so a subshell can differ from its parent, and
/// this is how routed code reads it.
///
/// # Errors
/// If no session is installed.
pub fn current_dir() -> io::Result<PathBuf> {
    Ok(PathBuf::from(current()?.cwd().as_str()))
}

/// A caller's path made absolute against the session's working directory,
/// still expressed virtually. Mirrors `std::path::absolute`.
///
/// The host's version is `getcwd(2)` plus a join, so routed code that used it
/// built a host-rooted prefix and then asked the namespace about it -- the same
/// half-virtual shape [`current_dir`] exists to fix, one layer up. `uu_mv`
/// is the motivating case: it makes both operands absolute before comparing
/// them for same-file detection, and under a mount every one of those
/// comparisons started with the host's working directory.
///
/// Unlike the host's version this applies the virtual-path grammar, so the
/// result is already normalised and a path that leaves the namespace is an
/// error rather than a string. That is the direction a confined caller wants
/// to fail in.
///
/// # Errors
/// If no session is installed, if the path is not UTF-8, or if it does not
/// resolve within the namespace.
pub fn absolute(path: impl AsRef<Path>) -> io::Result<PathBuf> {
    let session = current()?;
    Ok(PathBuf::from(resolve(&session, path)?.as_str()))
}

/// Opens a directory as a capability, for `*at`-anchored traversal.
///
/// The narrow exception to the path-based facade; see [`crate::dir`] and D3's
/// amendment. Callers that are already descriptor-shaped — `safe_traversal`'s
/// recursive walk — use this rather than re-resolving a path per operation,
/// since every re-resolution is a fresh chance for a component to change under
/// them.
///
/// # Errors
/// As [`open`], and if the path is not a directory.
pub fn open_dir(path: impl AsRef<Path>) -> io::Result<crate::dir::Dir> {
    with(path, crate::fs::Vfs::open_dir)
}

/// Opens a directory and surrenders its descriptor for `*at` traversal.
///
/// The rewrite target for `nix::fcntl::open(path, O_DIRECTORY, ..)` in
/// `uucore::safe_traversal::DirFd::open`. Read
/// [`crate::dir::Dir::into_owned_fd_for_at_traversal`] before using it: the
/// descriptor is confined where it lands but does not refuse `..`, so this
/// roots a traversal in the namespace rather than sealing it inside one.
///
/// `follow` false refuses a final symlink, matching the `O_NOFOLLOW` the caller
/// would otherwise have passed.
///
/// # Errors
/// As [`open_dir`], plus `ELOOP` when `follow` is false and the final component
/// is a symlink.
#[cfg(unix)]
pub fn open_dir_fd(path: impl AsRef<Path>, follow: bool) -> io::Result<std::os::fd::OwnedFd> {
    with(path, |vfs, p| {
        if !follow && vfs.is_symlink(p) {
            return Err(io::Error::from_raw_os_error(crate::fs::ELOOP));
        }
        vfs.open_dir(p)
    })
    .map(crate::dir::Dir::into_owned_fd_for_at_traversal)
}

/// A recursive walk rooted at `path`. The rewrite target for `WalkDir::new`.
///
/// Infallible like `WalkDir::new`, and for the same reason: a failure to resolve
/// the root surfaces as the iterator's first item, so a call site needs no `?`
/// the original did not have. See [`crate::walk`].
pub fn walk<P: AsRef<Path>>(path: P) -> crate::walk::Walk {
    let Ok(session) = current() else {
        return crate::walk::Walk::failed(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "no vfs session is installed; filesystem access is not permitted",
        ));
    };
    let display = path.as_ref().to_path_buf();
    match resolve(&session, path) {
        Ok(root) => crate::walk::Walk::rooted(session.vfs_arc(), root, display),
        Err(e) => crate::walk::Walk::failed(e),
    }
}

// -- Opening ---------------------------------------------------------------

/// Opens a file for reading. The rewrite target for `File::open(path)`.
///
/// # Errors
/// If no session is installed, the path is ungrammatical or unmounted, or the
/// open fails.
pub fn open(path: impl AsRef<Path>) -> io::Result<std::fs::File> {
    with(path, |vfs, p| vfs.open(p))
}

/// Creates or truncates a file for writing. The target for `File::create`.
///
/// # Errors
/// As [`open`], and if the containing directory is read-only.
pub fn create(path: impl AsRef<Path>) -> io::Result<std::fs::File> {
    with(path, |vfs, p| vfs.create(p))
}

/// Opens a file with an explicit mode. The target for an `OpenOptions` chain.
///
/// # Errors
/// As [`open`].
pub fn open_with(path: impl AsRef<Path>, mode: OpenMode) -> io::Result<std::fs::File> {
    with(path, |vfs, p| vfs.open_with(p, mode))
}

// -- Reading whole files ---------------------------------------------------

/// Reads a file's whole contents. Mirrors `std::fs::read`.
///
/// # Errors
/// As [`open`], plus any read error.
pub fn read(path: impl AsRef<Path>) -> io::Result<Vec<u8>> {
    use io::Read as _;
    let mut file = open(path)?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf)?;
    Ok(buf)
}

/// Reads a file's whole contents as a UTF-8 string. Mirrors
/// `std::fs::read_to_string`.
///
/// # Errors
/// As [`read`], plus an error if the contents are not valid UTF-8.
pub fn read_to_string(path: impl AsRef<Path>) -> io::Result<String> {
    use io::Read as _;
    let mut file = open(path)?;
    let mut buf = String::new();
    file.read_to_string(&mut buf)?;
    Ok(buf)
}

/// Writes a slice as the whole contents of a file. Mirrors `std::fs::write`.
///
/// # Errors
/// As [`create`], plus any write error.
pub fn write(path: impl AsRef<Path>, contents: impl AsRef<[u8]>) -> io::Result<()> {
    use io::Write as _;
    let mut file = create(path)?;
    file.write_all(contents.as_ref())
}

// -- Metadata and predicates ----------------------------------------------

/// Metadata for a path, following symlinks. Mirrors `std::fs::metadata` and the
/// `Path::metadata` method.
///
/// # Errors
/// As [`open`].
pub fn metadata(path: impl AsRef<Path>) -> io::Result<std::fs::Metadata> {
    with(path, |vfs, p| vfs.metadata(p))
}

/// Metadata for a path *without* following a final symlink. Mirrors
/// `std::fs::symlink_metadata` and `Path::symlink_metadata`.
///
/// # Errors
/// As [`open`]. On non-Unix this follows the final link (a documented Windows
/// limitation); see [`Vfs::symlink_metadata`](crate::fs::Vfs::symlink_metadata).
pub fn symlink_metadata(path: impl AsRef<Path>) -> io::Result<std::fs::Metadata> {
    with(path, |vfs, p| vfs.symlink_metadata(p))
}

/// Whether a path exists in the namespace, following symlinks. Mirrors
/// `Path::exists` and `std::fs::exists`. Fail-closed: `false` with no session.
#[must_use]
pub fn exists(path: impl AsRef<Path>) -> bool {
    with(path, |vfs, p| Ok(vfs.exists(p))).unwrap_or(false)
}

/// Whether a path is a directory, following symlinks. Mirrors `Path::is_dir`.
#[must_use]
pub fn is_dir(path: impl AsRef<Path>) -> bool {
    with(path, |vfs, p| {
        Ok(vfs.facts(p, true).is_some_and(|f| f.is_dir))
    })
    .unwrap_or(false)
}

/// Whether a path is a regular file, following symlinks. Mirrors
/// `Path::is_file`.
#[must_use]
pub fn is_file(path: impl AsRef<Path>) -> bool {
    with(path, |vfs, p| {
        Ok(vfs.facts(p, true).is_some_and(|f| f.is_file))
    })
    .unwrap_or(false)
}

/// Whether a path is a symlink (not followed). Mirrors `Path::is_symlink`.
#[must_use]
pub fn is_symlink(path: impl AsRef<Path>) -> bool {
    with(path, |vfs, p| Ok(vfs.is_symlink(p))).unwrap_or(false)
}

// -- Links -----------------------------------------------------------------

/// Reads a symlink's target. Mirrors `std::fs::read_link` and
/// `Path::read_link`. The target is returned verbatim as stored.
///
/// # Errors
/// As [`open`], plus an error if the path is not a symlink.
pub fn read_link(path: impl AsRef<Path>) -> io::Result<PathBuf> {
    with(path, |vfs, p| vfs.read_link(p))
}

/// The symlink-free form of a path, as a **virtual** path (D4). Mirrors
/// `std::fs::canonicalize` and `Path::canonicalize`.
///
/// The host's `canonicalize` would return a host path, which sandboxed code
/// must never receive and could not use; this returns the canonical path within
/// the namespace instead — the same answer `cd -P` gives.
///
/// # Errors
/// As [`open`].
pub fn canonicalize(path: impl AsRef<Path>) -> io::Result<PathBuf> {
    with(path, |vfs, p| {
        Ok(PathBuf::from(vfs.canonicalize(p)?.as_str()))
    })
}

/// Whether a path exists, distinguishing "does not exist" from an error.
/// Mirrors `Path::try_exists` and `std::fs::exists`.
///
/// # Errors
/// Returns an error only for a reason other than absence (an ungrammatical
/// path, or no session). A missing file is `Ok(false)`.
pub fn try_exists(path: impl AsRef<Path>) -> io::Result<bool> {
    with(path, |vfs, p| Ok(vfs.exists(p)))
}

/// Creates a symlink at `link` pointing at `target`, validating that the target
/// stays within the mount (D26). Mirrors the unix `std::os::unix::fs::symlink`
/// argument order (target first).
///
/// # Errors
/// As [`open`], plus an error if the target would leave the mount.
pub fn symlink(target: impl AsRef<Path>, link: impl AsRef<Path>) -> io::Result<()> {
    let target = target.as_ref().to_str().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "symlink target is not UTF-8")
    })?;
    with(link, |vfs, p| vfs.symlink(p, target))
}

// -- Directories -----------------------------------------------------------

/// Recursively creates a directory and any missing parents. Mirrors
/// `std::fs::create_dir_all`.
///
/// # Errors
/// As [`open`].
pub fn create_dir_all(path: impl AsRef<Path>) -> io::Result<()> {
    with(path, |vfs, p| vfs.create_dir_all(p))
}

/// Creates a directory with an explicit mode, optionally creating parents.
/// The rewrite target for a `std::fs::DirBuilder` chain.
///
/// # Errors
/// As [`create_dir_all`], and if the mode cannot be applied.
#[cfg(unix)]
pub fn create_dir_with_mode(path: impl AsRef<Path>, mode: u32, recursive: bool) -> io::Result<()> {
    with(path, |vfs, p| vfs.create_dir_with_mode(p, mode, recursive))
}

/// Removes a file. Mirrors `std::fs::remove_file`.
///
/// # Errors
/// As [`open`].
pub fn remove_file(path: impl AsRef<Path>) -> io::Result<()> {
    with(path, |vfs, p| vfs.remove_file(p))
}

/// Removes an empty directory. Mirrors `std::fs::remove_dir`.
///
/// # Errors
/// As [`open`].
pub fn remove_dir(path: impl AsRef<Path>) -> io::Result<()> {
    with(path, |vfs, p| vfs.remove_dir(p))
}

/// Recursively removes a directory and its contents. Mirrors
/// `std::fs::remove_dir_all`.
///
/// # Errors
/// As [`open`].
pub fn remove_dir_all(path: impl AsRef<Path>) -> io::Result<()> {
    with(path, |vfs, p| vfs.remove_dir_all(p))
}

/// Renames a path, refusing moves that cross a mount boundary (reported as
/// `CrossesDevices` so a caller falls back to copy-and-delete). Mirrors
/// `std::fs::rename`.
///
/// # Errors
/// As [`open`], plus `CrossesDevices` for a cross-mount move.
pub fn rename(from: impl AsRef<Path>, to: impl AsRef<Path>) -> io::Result<()> {
    let session = current()?;
    let from = resolve(&session, from)?;
    let to = resolve(&session, to)?;
    session.vfs().rename(&from, &to)
}
/// Creates a single directory. Mirrors `std::fs::create_dir`.
///
/// # Errors
/// As [`open`], and if the parent does not exist -- which is the point of it
/// being separate from [`create_dir_all`].
pub fn create_dir(path: impl AsRef<Path>) -> io::Result<()> {
    with(path, Vfs::create_dir)
}

/// Sets a path's permissions. Mirrors `std::fs::set_permissions`.
///
/// # Errors
/// As [`open`], and if the mount is read-only.
#[expect(
    clippy::needless_pass_by_value,
    reason = "D34: the rewrite is an identifier swap, so this must accept exactly what \
              `std::fs::set_permissions` accepts -- taking a reference would make every \
              call site need editing, which is the property the codemod exists to avoid"
)]
pub fn set_permissions(path: impl AsRef<Path>, perm: std::fs::Permissions) -> io::Result<()> {
    with(path, |vfs, p| vfs.set_permissions(p, &perm))
}

/// Copies a file's contents, returning the bytes written. Mirrors
/// `std::fs::copy`.
///
/// Both ends resolve through the namespace, so copying *out* of it is not
/// expressible.
///
/// # Errors
/// As [`open`] for either path, and if the destination mount is read-only.
pub fn copy(from: impl AsRef<Path>, to: impl AsRef<Path>) -> io::Result<u64> {
    let session = current()?;
    let from = resolve(&session, from)?;
    let to = resolve(&session, to)?;
    session.vfs().copy(&from, &to)
}

/// Creates a hard link. Mirrors `std::fs::hard_link`.
///
/// # Errors
/// As [`open`] for either path, and `CrossesDevices` if the two paths are on
/// different mounts.
pub fn hard_link(original: impl AsRef<Path>, link: impl AsRef<Path>) -> io::Result<()> {
    let session = current()?;
    let original = resolve(&session, original)?;
    let link = resolve(&session, link)?;
    session.vfs().hard_link(&original, &link)
}

/// The entries of a directory. The rewrite target for `std::fs::read_dir`.
///
/// # Errors
/// As [`open`].
pub fn read_dir(path: impl AsRef<Path>) -> io::Result<ReadDir> {
    let session = current()?;
    let dir = resolve(&session, path)?;
    let names = session.vfs().read_dir_names(&dir)?;
    Ok(ReadDir {
        dir,
        names: names.into_iter(),
    })
}

/// An iterator over a directory's entries, shaped like `std::fs::ReadDir`.
///
/// `std::fs::ReadDir` has no public constructor, so the facade cannot hand one
/// back; this stands in for it. Each item is an [`io::Result`] of a
/// [`DirEntry`], matching `std::fs::ReadDir`'s item type so a `for entry in
/// read_dir(p)? { let entry = entry?; ... }` loop is unchanged.
#[derive(Debug)]
pub struct ReadDir {
    dir: VirtualPath,
    names: std::vec::IntoIter<String>,
}

impl Iterator for ReadDir {
    type Item = io::Result<DirEntry>;

    fn next(&mut self) -> Option<Self::Item> {
        let name = self.names.next()?;
        // The join is grammatical by construction -- a directory entry name has
        // no separators -- so a failure here is a real error, not an empty
        // iterator.
        match self.dir.resolve(&name) {
            Ok(path) => Some(Ok(DirEntry { path, name })),
            Err(e) => Some(Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                e.to_string(),
            ))),
        }
    }
}

/// A single directory entry, shaped like `std::fs::DirEntry`.
///
/// It carries the entry's virtual path so `file_type` and `metadata` can ask
/// the namespace, rather than a `cap_std` handle that would not survive the
/// std-typed contract.
#[derive(Debug)]
pub struct DirEntry {
    path: VirtualPath,
    name: String,
}

impl DirEntry {
    /// The entry's file name, with no directory component. Mirrors
    /// `std::fs::DirEntry::file_name`.
    #[must_use]
    pub fn file_name(&self) -> std::ffi::OsString {
        std::ffi::OsString::from(&self.name)
    }

    /// The entry's full virtual path. Mirrors `std::fs::DirEntry::path`. Unlike
    /// `cap_std`'s `DirEntry`, which has none, this can answer because it holds
    /// the resolved path.
    #[must_use]
    pub fn path(&self) -> PathBuf {
        PathBuf::from(self.path.as_str())
    }

    /// Metadata for the entry, following symlinks. Mirrors
    /// `std::fs::DirEntry::metadata` (which does *not* follow) — see the note:
    /// callers that must not follow use [`symlink_file_type`](Self::file_type).
    ///
    /// # Errors
    /// As [`open`].
    pub fn metadata(&self) -> io::Result<std::fs::Metadata> {
        let session = current()?;
        session.vfs().metadata(&self.path)
    }

    /// The entry's file type, not following a final symlink. Mirrors
    /// `std::fs::DirEntry::file_type`, **including its return type**.
    ///
    /// This returns `std::fs::FileType` rather than a lookalike, which matters
    /// out of proportion to its size: `uu_ls` puts the type in a trait method's
    /// signature and threads it through its colouring code, so a substitute type
    /// stops being a rewrite and becomes a restructure -- exactly what D34
    /// forbids. `std::fs::FileType` has no public constructor, but
    /// `Metadata::file_type()` is one, and the facade can produce a real
    /// `Metadata`, so the type never has to be replaced.
    ///
    /// # Errors
    /// As [`open`].
    pub fn file_type(&self) -> io::Result<std::fs::FileType> {
        let session = current()?;
        // `symlink_metadata`, not `metadata`: std's `DirEntry::file_type` does
        // not follow the final link, and reporting a symlink as its target is
        // how `ls -l` loses the `l` in its mode column.
        Ok(session.vfs().symlink_metadata(&self.path)?.file_type())
    }
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "test fixtures are built on the host, which is the one place that is the point"
)]
mod tests {
    use super::*;
    use crate::mount::{Access, MountTable};
    use std::sync::{Arc, Mutex};

    // The ambient session is process-global, so tests that install it cannot run
    // concurrently. This serializes them without depending on test-runner flags.
    static GUARD: Mutex<()> = Mutex::new(());

    fn install_over(dir: &Path) {
        let mounts = MountTable::builder()
            .mount("/work", dir, Access::ReadWrite)
            .expect("mount")
            .build()
            .expect("build");
        let mut session = Session::new(Arc::new(Vfs::new(mounts)));
        session.set_cwd("/work").expect("cwd");
        install(session);
    }

    #[test]
    fn no_session_fails_closed() {
        let _g = GUARD
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        uninstall();
        assert!(open("/work/x").is_err());
        assert!(!exists("/work/x"));
        assert!(read("/work/x").is_err());
    }

    #[test]
    fn open_read_write_round_trip_against_the_session() {
        let _g = GUARD
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let tmp = tempfile::tempdir().unwrap();
        install_over(tmp.path());

        write("/work/greeting.txt", b"hello vfs").unwrap();
        assert_eq!(read_to_string("/work/greeting.txt").unwrap(), "hello vfs");
        assert!(exists("/work/greeting.txt"));
        assert!(is_file("/work/greeting.txt"));
        assert!(!is_dir("/work/greeting.txt"));
        uninstall();
    }

    #[test]
    fn relative_paths_resolve_against_the_working_directory() {
        let _g = GUARD
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let tmp = tempfile::tempdir().unwrap();
        install_over(tmp.path());

        // cwd is /work, so a bare name lands there.
        write("note", b"x").unwrap();
        assert!(exists("/work/note"));
        assert_eq!(read("note").unwrap(), b"x");
        uninstall();
    }

    #[test]
    fn absolute_roots_a_relative_path_at_the_session_cwd() {
        let _g = GUARD
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let tmp = tempfile::tempdir().unwrap();
        install_over(tmp.path());

        // The host's `std::path::absolute` would answer with the *process*
        // working directory, which is what `uu_mv` was doing wrong.
        assert_eq!(absolute("note").unwrap(), Path::new("/work/note"));
        assert_eq!(absolute("/work/note").unwrap(), Path::new("/work/note"));
        // The grammar applies, so the result is already normalised.
        assert_eq!(absolute("./sub/../note").unwrap(), Path::new("/work/note"));
        // And a path that leaves the namespace is an error rather than a
        // string, which is the direction a confined caller wants to fail in.
        assert!(absolute("/work/../../etc/passwd").is_err());
        uninstall();
    }

    #[test]
    fn absolute_fails_closed_with_no_session() {
        let _g = GUARD
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        uninstall();
        assert!(absolute("note").is_err());
    }

    #[test]
    #[cfg(unix)]
    fn create_dir_with_mode_applies_the_mode_at_creation() {
        use std::os::unix::fs::PermissionsExt as _;
        let _g = GUARD
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let tmp = tempfile::tempdir().unwrap();
        install_over(tmp.path());

        create_dir_with_mode("/work/tight", 0o700, false).unwrap();
        let mode = metadata("/work/tight").unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o700,
            "the mode must be the one asked for, not the umask's"
        );

        // The recursive form makes parents, as `DirBuilder::recursive` does.
        create_dir_with_mode("/work/a/b/c", 0o755, true).unwrap();
        assert!(is_dir("/work/a/b/c"));
        // And the non-recursive form still refuses a missing parent.
        assert!(create_dir_with_mode("/work/x/y", 0o755, false).is_err());
        uninstall();
    }

    #[test]
    #[cfg(unix)]
    fn custom_open_flags_reach_the_open() {
        let _g = GUARD
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let tmp = tempfile::tempdir().unwrap();
        install_over(tmp.path());
        write("/work/f", b"x").unwrap();

        // `O_NONBLOCK` is observable on the descriptor afterwards, which is how
        // `dd`'s `iflag=` and `tail`'s FIFO open can be trusted to carry.
        let file = open_with(
            "/work/f",
            crate::fs::OpenMode::read().with_custom_flags(libc::O_NONBLOCK),
        )
        .unwrap();
        // SAFETY: `fcntl(F_GETFL)` only reads the descriptor's flag word.
        let flags = unsafe { libc::fcntl(std::os::fd::AsRawFd::as_raw_fd(&file), libc::F_GETFL) };
        assert!(flags >= 0, "fcntl failed");
        assert!(
            flags & libc::O_NONBLOCK != 0,
            "O_NONBLOCK did not reach the open"
        );
        uninstall();
    }

    #[test]
    fn read_dir_yields_entries_that_can_answer_about_themselves() {
        let _g = GUARD
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let tmp = tempfile::tempdir().unwrap();
        install_over(tmp.path());
        write("/work/a.txt", b"a").unwrap();
        create_dir_all("/work/sub").unwrap();

        let mut names: Vec<String> = read_dir("/work")
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, vec!["a.txt", "sub"]);

        let entries: Vec<DirEntry> = read_dir("/work").unwrap().map(Result::unwrap).collect();
        for entry in entries {
            if entry.file_name() == "sub" {
                assert!(entry.file_type().unwrap().is_dir());
            } else {
                assert!(entry.file_type().unwrap().is_file());
                assert_eq!(entry.path(), PathBuf::from("/work/a.txt"));
            }
        }
        uninstall();
    }

    #[test]
    fn canonicalize_returns_a_virtual_path_not_a_host_one() {
        let _g = GUARD
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let tmp = tempfile::tempdir().unwrap();
        install_over(tmp.path());
        create_dir_all("/work/a/b").unwrap();

        // The answer is inside the namespace, never the host directory the mount
        // is backed by.
        let real = canonicalize("/work/a/../a/b").unwrap();
        assert_eq!(real, PathBuf::from("/work/a/b"));
        assert!(try_exists("/work/a/b").unwrap());
        assert!(!try_exists("/work/missing").unwrap());
        uninstall();
    }

    #[test]
    fn a_path_outside_the_mount_is_unreachable() {
        let _g = GUARD
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let tmp = tempfile::tempdir().unwrap();
        install_over(tmp.path());
        assert!(open("/etc/passwd").is_err());
        assert!(!exists("/etc/passwd"));
        uninstall();
    }
}
