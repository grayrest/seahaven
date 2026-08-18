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
    let mut guard = SESSION.write().unwrap_or_else(std::sync::PoisonError::into_inner);
    *guard = Some(session);
}

/// Removes the installed session, returning the facade to its fail-closed state.
///
/// Exists for tests that must not leak a session into the next one; ordinary
/// process teardown does not need it.
pub fn uninstall() {
    let mut guard = SESSION.write().unwrap_or_else(std::sync::PoisonError::into_inner);
    *guard = None;
}

/// A cheap snapshot of the installed session, or an error if none is installed.
fn current() -> io::Result<Session> {
    let guard = SESSION.read().unwrap_or_else(std::sync::PoisonError::into_inner);
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
            Err(e) => Some(Err(io::Error::new(io::ErrorKind::InvalidInput, e.to_string()))),
        }
    }
}

/// A single directory entry, shaped like `std::fs::DirEntry`.
///
/// It carries the entry's virtual path so `file_type` and `metadata` can ask
/// the namespace, rather than a `cap_std` handle that would not survive the
/// std-typed contract.
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

    /// The entry's file type without following symlinks. Mirrors
    /// `std::fs::DirEntry::file_type`.
    ///
    /// # Errors
    /// As [`open`].
    pub fn file_type(&self) -> io::Result<FileType> {
        let session = current()?;
        session
            .vfs()
            .facts(&self.path, false)
            .map(|facts| FileType {
                is_dir: facts.is_dir,
                is_file: facts.is_file,
                is_symlink: facts.is_symlink,
            })
            .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))
    }
}

/// A file's type, shaped like `std::fs::FileType`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileType {
    is_dir: bool,
    is_file: bool,
    is_symlink: bool,
}

impl FileType {
    /// Whether this is a directory. Mirrors `std::fs::FileType::is_dir`.
    #[must_use]
    pub const fn is_dir(self) -> bool {
        self.is_dir
    }

    /// Whether this is a regular file. Mirrors `std::fs::FileType::is_file`.
    #[must_use]
    pub const fn is_file(self) -> bool {
        self.is_file
    }

    /// Whether this is a symlink. Mirrors `std::fs::FileType::is_symlink`.
    #[must_use]
    pub const fn is_symlink(self) -> bool {
        self.is_symlink
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
        let _g = GUARD.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        uninstall();
        assert!(open("/work/x").is_err());
        assert!(!exists("/work/x"));
        assert!(read("/work/x").is_err());
    }

    #[test]
    fn open_read_write_round_trip_against_the_session() {
        let _g = GUARD.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
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
        let _g = GUARD.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let tmp = tempfile::tempdir().unwrap();
        install_over(tmp.path());

        // cwd is /work, so a bare name lands there.
        write("note", b"x").unwrap();
        assert!(exists("/work/note"));
        assert_eq!(read("note").unwrap(), b"x");
        uninstall();
    }

    #[test]
    fn read_dir_yields_entries_that_can_answer_about_themselves() {
        let _g = GUARD.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
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
        let _g = GUARD.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
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
        let _g = GUARD.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let tmp = tempfile::tempdir().unwrap();
        install_over(tmp.path());
        assert!(open("/etc/passwd").is_err());
        assert!(!exists("/etc/passwd"));
        uninstall();
    }
}
