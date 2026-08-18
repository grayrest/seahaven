//! Filesystem operations over the virtual namespace.
//!
//! The API is **path-based and `std`-typed**: it takes [`VirtualPath`] and hands
//! back `std::fs::File` and `std::fs::Metadata`. Callers never hold a directory
//! capability. That is not a stylistic choice — the utilities this eventually
//! has to accommodate carry owned paths and re-open them repeatedly, so an API
//! demanding a `Dir` would force them to be restructured rather than rewritten.
//!
//! It is sound because confinement comes from *resolution*, not from the handle
//! type. Once a descriptor has been opened beneath a mount it carries no ambient
//! authority, so returning a plain `std::fs::File` gives nothing away.
//!
//! # Symlinks
//!
//! `cap-std` rejects every absolute symlink, not just escaping ones, because
//! there is no root for an absolute target to be absolute *to*. Here there is:
//! an absolute target is interpreted as virtual-absolute and re-resolved from
//! the virtual root, which is what a chroot-like namespace should mean and what
//! keeps `/usr/local/bin` — 13 of 13 absolute symlinks on a typical host —
//! working. Relative targets keep `cap-std`'s beneath-the-root behaviour.
//!
//! Doing that resolution here rather than in `cap-std` is the one place this
//! design knowingly re-derives hardening it otherwise delegates. Two things
//! contain the cost. Traversal is bounded, so a symlink cycle terminates. And
//! the final operation still goes through `cap-std`, so if a component becomes
//! a symlink between resolution and use, the race produces an *error* rather
//! than an escape: `cap-std` re-checks beneath-ness and refuses.

use std::path::PathBuf;

use crate::mount::{Mount, MountTable};
use crate::path::VirtualPath;

/// Maximum symlinks followed while resolving one path.
///
/// Matches the conventional `ELOOP` threshold. Without it, `a -> b -> a` would
/// not terminate: the cycle is only visible as a hop count, since each
/// individual hop is a legitimate link.
const MAX_SYMLINK_HOPS: usize = 40;

/// Message identifying a symlink loop, since the matching `ErrorKind` is
/// unstable.
const SYMLINK_LOOP_MESSAGE: &str = "too many levels of symbolic links";

/// Whether an error from this module reports a symlink loop.
#[must_use]
pub fn is_symlink_loop(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::Other && error.to_string().contains(SYMLINK_LOOP_MESSAGE)
}

/// How a file is to be opened.
///
/// The write intent is carried explicitly rather than recovered from
/// `cap_std::fs::OpenOptions`, which does not expose its flags. Inferring it
/// would mean a read-only mount's enforcement depended on a formatting detail of
/// another crate — the kind of coupling that breaks silently and in the
/// permissive direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct OpenMode {
    read: bool,
    write: bool,
    append: bool,
    create: bool,
    create_new: bool,
    truncate: bool,
}

impl OpenMode {
    /// Opens an existing file for reading.
    #[must_use]
    pub const fn read() -> Self {
        Self {
            read: true,
            write: false,
            append: false,
            create: false,
            create_new: false,
            truncate: false,
        }
    }

    /// Creates or truncates a file for writing.
    #[must_use]
    pub const fn write() -> Self {
        Self {
            read: false,
            write: true,
            append: false,
            create: true,
            create_new: false,
            truncate: true,
        }
    }

    /// Appends to a file, creating it if absent.
    #[must_use]
    pub const fn append() -> Self {
        Self {
            read: false,
            write: false,
            append: true,
            create: true,
            create_new: false,
            truncate: false,
        }
    }

    /// Also opens for reading.
    #[must_use]
    pub const fn with_read(mut self, yes: bool) -> Self {
        self.read = yes;
        self
    }

    /// Also opens for writing.
    #[must_use]
    pub const fn with_write(mut self, yes: bool) -> Self {
        self.write = yes;
        self
    }

    /// Appends rather than overwriting.
    #[must_use]
    pub const fn with_append(mut self, yes: bool) -> Self {
        self.append = yes;
        self
    }

    /// Creates the file if it does not exist.
    #[must_use]
    pub const fn with_create(mut self, yes: bool) -> Self {
        self.create = yes;
        self
    }

    /// Fails if the file already exists, which is what `noclobber` needs.
    #[must_use]
    pub const fn with_create_new(mut self, yes: bool) -> Self {
        self.create_new = yes;
        self
    }

    /// Truncates an existing file.
    #[must_use]
    pub const fn with_truncate(mut self, yes: bool) -> Self {
        self.truncate = yes;
        self
    }

    /// Whether this mode would modify the filesystem in any way.
    ///
    /// Creation counts even without `write`: making a file is a change to the
    /// directory containing it.
    #[must_use]
    pub const fn is_write(self) -> bool {
        self.write || self.append || self.create || self.create_new || self.truncate
    }

    /// The equivalent `cap-std` options.
    fn to_cap_std(self) -> cap_std::fs::OpenOptions {
        let mut options = cap_std::fs::OpenOptions::new();
        options
            .read(self.read)
            .write(self.write)
            .append(self.append)
            .create(self.create)
            .create_new(self.create_new)
            .truncate(self.truncate);
        options
    }
}

/// Filesystem access confined to a [`MountTable`].
#[derive(Debug)]
pub struct Vfs {
    mounts: MountTable,
}

/// Where a virtual path landed after symlink resolution.
struct Located<'a> {
    mount: &'a Mount,
    /// Path relative to the mount's root; empty at the mount point itself.
    relative: PathBuf,
    /// The fully resolved virtual path, for diagnostics.
    virtual_path: VirtualPath,
}

fn not_found(path: &VirtualPath) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::NotFound,
        format!("no such file or directory: {path}"),
    )
}

fn read_only(path: &VirtualPath) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::PermissionDenied,
        format!("read-only mount: {path}"),
    )
}

impl Vfs {
    /// Wraps a mount table.
    #[must_use]
    pub const fn new(mounts: MountTable) -> Self {
        Self { mounts }
    }

    /// The mount table this filesystem resolves against.
    #[must_use]
    pub const fn mounts(&self) -> &MountTable {
        &self.mounts
    }

    /// Locates a path without following a symlink in its final component.
    fn locate_nofollow(&self, path: &VirtualPath) -> std::io::Result<Located<'_>> {
        self.locate(path, false)
    }

    /// Locates a path, following symlinks throughout.
    fn locate_follow(&self, path: &VirtualPath) -> std::io::Result<Located<'_>> {
        self.locate(path, true)
    }

    /// Walks `path` component by component, resolving symlinks as it goes.
    ///
    /// Resolution is virtual-path-level rather than descriptor-level because a
    /// symlink may cross a mount boundary: following one has to re-enter the
    /// mount table, not merely descend from wherever the walk had reached.
    fn locate(&self, path: &VirtualPath, follow_final: bool) -> std::io::Result<Located<'_>> {
        let mut resolved = VirtualPath::root();
        let mut pending: Vec<String> = path.components().rev().map(str::to_owned).collect();
        let mut hops = 0usize;

        while let Some(component) = pending.pop() {
            let candidate = resolved.resolve(&component).map_err(|e| {
                std::io::Error::new(std::io::ErrorKind::InvalidInput, e.to_string())
            })?;

            let is_final = pending.is_empty();
            let located = self.locate_exact(&candidate)?;

            // `symlink_metadata` rather than `metadata`: the question is whether
            // this component *is* a link, not what it points at.
            let is_symlink = located
                .mount
                .dir()
                .symlink_metadata(&located.relative)
                .is_ok_and(|m| m.is_symlink());

            if is_symlink && (!is_final || follow_final) {
                hops += 1;
                if hops > MAX_SYMLINK_HOPS {
                    // `ErrorKind::FilesystemLoop` is still unstable, so the
                    // condition is reported by message. Callers that need to
                    // distinguish it should use `is_symlink_loop`.
                    return Err(std::io::Error::other(format!(
                        "{SYMLINK_LOOP_MESSAGE}: {path}"
                    )));
                }

                let target = read_link_contents(located.mount, &located.relative)?;
                let target = target.to_str().ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("symlink target is not valid UTF-8: {path}"),
                    )
                })?;

                // The RESOLVE_IN_ROOT rule. An absolute target restarts at the
                // virtual root; a relative one continues from this component's
                // parent, which is where the link itself lives.
                if target.starts_with('/') {
                    resolved = VirtualPath::root();
                } else {
                    resolved = candidate.parent().unwrap_or_else(VirtualPath::root);
                }

                pending.extend(
                    target
                        .split('/')
                        .filter(|c| !c.is_empty())
                        .rev()
                        .map(str::to_owned),
                );
                continue;
            }

            resolved = candidate;
        }

        self.locate_exact(&resolved)
    }

    /// Maps a virtual path to a mount without touching symlinks.
    fn locate_exact(&self, path: &VirtualPath) -> std::io::Result<Located<'_>> {
        let (mount, rest) = self.mounts.resolve(path).ok_or_else(|| not_found(path))?;
        let mut relative = PathBuf::new();
        for component in rest {
            relative.push(component);
        }
        Ok(Located {
            mount,
            relative,
            virtual_path: path.clone(),
        })
    }

    /// Opens a file with the given options.
    ///
    /// # Errors
    ///
    /// Returns [`std::io::Error`] if the path is unmounted, if the mount is
    /// read-only and the options request writing, or if the underlying open
    /// fails.
    pub fn open_with(&self, path: &VirtualPath, mode: OpenMode) -> std::io::Result<std::fs::File> {
        let located = self.locate_follow(path)?;
        // The write check is on the resolved location, not the requested one: a
        // symlink from a writable mount into a read-only one must be governed by
        // where it lands.
        if !located.mount.access().is_writable() && mode.is_write() {
            return Err(read_only(&located.virtual_path));
        }
        Ok(located
            .mount
            .dir()
            .open_with(&located.relative, &mode.to_cap_std())?
            .into_std())
    }

    /// Opens a file for reading.
    ///
    /// # Errors
    ///
    /// As [`Vfs::open_with`].
    pub fn open(&self, path: &VirtualPath) -> std::io::Result<std::fs::File> {
        self.open_with(path, OpenMode::read())
    }

    /// Creates or truncates a file for writing.
    ///
    /// # Errors
    ///
    /// As [`Vfs::open_with`].
    pub fn create(&self, path: &VirtualPath) -> std::io::Result<std::fs::File> {
        self.open_with(path, OpenMode::write())
    }

    /// Metadata for `path`, following symlinks.
    ///
    /// # Errors
    ///
    /// Returns [`std::io::Error`] if the path is unmounted or the query fails.
    pub fn metadata(&self, path: &VirtualPath) -> std::io::Result<std::fs::Metadata> {
        let located = self.locate_follow(path)?;
        // Going through an opened descriptor keeps the returned type `std`'s,
        // so callers keep their platform extension traits.
        located
            .mount
            .dir()
            .open_with(
                &located.relative,
                cap_std::fs::OpenOptions::new().read(true),
            )
            .and_then(|f| f.into_std().metadata())
    }

    /// Whether `path` exists, following symlinks.
    #[must_use]
    pub fn exists(&self, path: &VirtualPath) -> bool {
        self.locate_follow(path).is_ok_and(|located| {
            located
                .mount
                .dir()
                .symlink_metadata(&located.relative)
                .is_ok()
        })
    }

    /// Whether `path` is a symlink, without following it.
    #[must_use]
    pub fn is_symlink(&self, path: &VirtualPath) -> bool {
        self.locate_nofollow(path).is_ok_and(|located| {
            located
                .mount
                .dir()
                .symlink_metadata(&located.relative)
                .is_ok_and(|m| m.is_symlink())
        })
    }

    /// Reads a symlink's target verbatim, without resolving it.
    ///
    /// # Errors
    ///
    /// Returns [`std::io::Error`] if the path is unmounted or is not a symlink.
    pub fn read_link(&self, path: &VirtualPath) -> std::io::Result<PathBuf> {
        let located = self.locate_nofollow(path)?;
        read_link_contents(located.mount, &located.relative)
    }

    /// Lists a directory's entry names.
    ///
    /// Names rather than paths: a `DirEntry`'s path is only meaningful relative
    /// to the directory it came from, and handing back a host path would defeat
    /// the point of the namespace.
    ///
    /// # Errors
    ///
    /// Returns [`std::io::Error`] if the path is unmounted or is not a
    /// directory.
    pub fn read_dir_names(&self, path: &VirtualPath) -> std::io::Result<Vec<String>> {
        let located = self.locate_follow(path)?;
        let dir = if located.relative.as_os_str().is_empty() {
            located.mount.dir().try_clone()?
        } else {
            located.mount.dir().open_dir(&located.relative)?
        };

        let mut names = Vec::new();
        for entry in dir.entries()? {
            let entry = entry?;
            names.push(entry.file_name().to_string_lossy().into_owned());
        }
        Ok(names)
    }

    /// Creates a directory, and any missing parents.
    ///
    /// # Errors
    ///
    /// Returns [`std::io::Error`] if the path is unmounted, the mount is
    /// read-only, or creation fails.
    pub fn create_dir_all(&self, path: &VirtualPath) -> std::io::Result<()> {
        let located = self.locate_nofollow(path)?;
        Self::require_writable(&located)?;
        located.mount.dir().create_dir_all(&located.relative)
    }

    /// Removes a file.
    ///
    /// # Errors
    ///
    /// Returns [`std::io::Error`] if the path is unmounted, the mount is
    /// read-only, or removal fails.
    pub fn remove_file(&self, path: &VirtualPath) -> std::io::Result<()> {
        let located = self.locate_nofollow(path)?;
        Self::require_writable(&located)?;
        located.mount.dir().remove_file(&located.relative)
    }

    /// Removes an empty directory.
    ///
    /// # Errors
    ///
    /// As [`Vfs::remove_file`].
    pub fn remove_dir(&self, path: &VirtualPath) -> std::io::Result<()> {
        let located = self.locate_nofollow(path)?;
        Self::require_writable(&located)?;
        located.mount.dir().remove_dir(&located.relative)
    }

    fn require_writable(located: &Located<'_>) -> std::io::Result<()> {
        if located.mount.access().is_writable() {
            Ok(())
        } else {
            Err(read_only(&located.virtual_path))
        }
    }
}

/// Reads a symlink's target exactly as stored.
///
/// `cap_std::fs::Dir::read_link` refuses a target that is absolute — it has no
/// root to interpret one against, so it reports "a path led outside of the
/// filesystem" even for a link this namespace can resolve perfectly well. The
/// underlying primitive still validates the path *to* the link against the
/// mount root; only the returned contents are raw, which is what D42 needs.
fn read_link_contents(mount: &Mount, relative: &std::path::Path) -> std::io::Result<PathBuf> {
    let dir = mount.dir().try_clone()?.into_std_file();
    cap_primitives::fs::read_link_contents(&dir, relative)
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;

    use super::*;
    use crate::mount::{Access, MountTable};

    struct Fixture {
        _root: tempfile::TempDir,
        vfs: Vfs,
    }

    /// A namespace with a writable `/work` and a read-only `/ro`, on disjoint
    /// host directories.
    fn fixture() -> Fixture {
        let root = tempfile::tempdir().expect("temp dir");
        let work = root.path().join("work");
        let ro = root.path().join("ro");
        std::fs::create_dir(&work).expect("mkdir work");
        std::fs::create_dir(&ro).expect("mkdir ro");
        std::fs::write(work.join("hello.txt"), b"hello").expect("write");
        std::fs::write(ro.join("readme.txt"), b"readme").expect("write");
        std::fs::create_dir(work.join("sub")).expect("mkdir sub");
        std::fs::write(work.join("sub").join("deep.txt"), b"deep").expect("write");

        let mounts = MountTable::builder()
            .mount("/work", &work, Access::ReadWrite)
            .unwrap()
            .mount("/ro", &ro, Access::ReadOnly)
            .unwrap()
            .build()
            .expect("fixture mounts build");

        Fixture {
            _root: root,
            vfs: Vfs::new(mounts),
        }
    }

    fn vp(s: &str) -> VirtualPath {
        VirtualPath::new(s).expect("valid virtual path")
    }

    /// The host directory behind `/work`, for planting links the vfs must then
    /// interpret. Tests reach around the vfs deliberately: a link it cannot
    /// create is still a link it has to resolve safely.
    #[cfg(unix)]
    fn work_host(f: &Fixture) -> std::path::PathBuf {
        f.vfs
            .mounts()
            .mounts()
            .find(|m| m.mount_point().as_str() == "/work")
            .expect("/work is mounted")
            .host_path()
            .to_path_buf()
    }

    fn read(vfs: &Vfs, path: &str) -> std::io::Result<String> {
        use std::io::Read as _;
        let mut s = String::new();
        vfs.open(&vp(path))?.read_to_string(&mut s)?;
        Ok(s)
    }

    #[test]
    fn reads_through_a_mount() {
        let f = fixture();
        assert_eq!(read(&f.vfs, "/work/hello.txt").unwrap(), "hello");
        assert_eq!(read(&f.vfs, "/work/sub/deep.txt").unwrap(), "deep");
        assert_eq!(read(&f.vfs, "/ro/readme.txt").unwrap(), "readme");
    }

    #[test]
    fn unmounted_paths_are_not_found() {
        let f = fixture();
        let err = read(&f.vfs, "/etc/passwd").unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    }

    #[test]
    fn writes_are_refused_on_a_read_only_mount() {
        let f = fixture();
        let err = f.vfs.create(&vp("/ro/new.txt")).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);

        let err = f.vfs.remove_file(&vp("/ro/readme.txt")).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);

        // And the file is still there.
        assert_eq!(read(&f.vfs, "/ro/readme.txt").unwrap(), "readme");
    }

    #[test]
    fn writes_succeed_on_a_writable_mount() {
        let f = fixture();
        f.vfs
            .create(&vp("/work/new.txt"))
            .unwrap()
            .write_all(b"written")
            .unwrap();
        assert_eq!(read(&f.vfs, "/work/new.txt").unwrap(), "written");

        f.vfs.remove_file(&vp("/work/new.txt")).unwrap();
        assert!(!f.vfs.exists(&vp("/work/new.txt")));
    }

    #[test]
    fn dotdot_inside_the_namespace_resolves() {
        let f = fixture();
        assert_eq!(read(&f.vfs, "/work/sub/../hello.txt").unwrap(), "hello");
    }

    #[cfg(unix)]
    #[test]
    fn a_relative_symlink_inside_the_mount_is_followed() {
        let f = fixture();
        std::os::unix::fs::symlink("hello.txt", work_host(&f).join("link.txt")).unwrap();
        assert_eq!(read(&f.vfs, "/work/link.txt").unwrap(), "hello");
    }

    #[cfg(unix)]
    #[test]
    fn an_absolute_symlink_resolves_against_the_virtual_root() {
        // This is the D42 behaviour: cap-std alone would reject the link outright,
        // which is what would break `$PATH` on a real host.
        let f = fixture();
        std::os::unix::fs::symlink("/ro/readme.txt", work_host(&f).join("abs.txt")).unwrap();
        assert_eq!(read(&f.vfs, "/work/abs.txt").unwrap(), "readme");
    }

    #[cfg(unix)]
    #[test]
    fn an_absolute_symlink_to_a_host_path_does_not_escape() {
        // The target is absolute *in the virtual namespace*, so /etc names
        // nothing rather than the host's /etc.
        let f = fixture();
        std::os::unix::fs::symlink("/etc/passwd", work_host(&f).join("escape.txt")).unwrap();
        let err = read(&f.vfs, "/work/escape.txt").unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    }

    #[cfg(unix)]
    #[test]
    fn a_relative_symlink_climbing_out_of_the_namespace_does_not_escape() {
        let f = fixture();
        std::os::unix::fs::symlink("../../../../etc/passwd", work_host(&f).join("up.txt")).unwrap();
        let err = read(&f.vfs, "/work/up.txt").unwrap_err();
        assert!(matches!(
            err.kind(),
            std::io::ErrorKind::NotFound | std::io::ErrorKind::InvalidInput
        ));
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_into_a_read_only_mount_is_still_read_only() {
        // The access check is on where the link lands, not where it lives.
        let f = fixture();
        std::os::unix::fs::symlink("/ro/readme.txt", work_host(&f).join("ro-link.txt")).unwrap();
        let err = f.vfs.create(&vp("/work/ro-link.txt")).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_cycle_terminates() {
        let f = fixture();
        std::os::unix::fs::symlink("b", work_host(&f).join("a")).unwrap();
        std::os::unix::fs::symlink("a", work_host(&f).join("b")).unwrap();
        let err = read(&f.vfs, "/work/a").unwrap_err();
        assert!(is_symlink_loop(&err), "expected a loop error, got {err}");
    }

    #[cfg(unix)]
    #[test]
    fn read_link_reports_the_target_verbatim() {
        let f = fixture();
        std::os::unix::fs::symlink("/ro/readme.txt", work_host(&f).join("v.txt")).unwrap();
        assert!(f.vfs.is_symlink(&vp("/work/v.txt")));
        assert_eq!(
            f.vfs.read_link(&vp("/work/v.txt")).unwrap(),
            PathBuf::from("/ro/readme.txt")
        );
    }

    #[test]
    fn directories_list_their_names() {
        let f = fixture();
        let mut names = f.vfs.read_dir_names(&vp("/work")).unwrap();
        names.sort();
        assert_eq!(names, vec!["hello.txt", "sub"]);

        let names = f.vfs.read_dir_names(&vp("/work/sub")).unwrap();
        assert_eq!(names, vec!["deep.txt"]);
    }

    #[test]
    fn every_write_intent_is_refused_on_a_read_only_mount() {
        // One case per flag, because the previous implementation inferred intent
        // from a Debug string and would have failed open on any it did not name.
        let f = fixture();
        for mode in [
            OpenMode::write(),
            OpenMode::append(),
            OpenMode::read().with_write(true),
            OpenMode::read().with_create(true),
            OpenMode::read().with_create_new(true),
            OpenMode::read().with_truncate(true),
        ] {
            assert!(mode.is_write(), "{mode:?} should count as a write");
            let err = f.vfs.open_with(&vp("/ro/readme.txt"), mode).unwrap_err();
            assert_eq!(
                err.kind(),
                std::io::ErrorKind::PermissionDenied,
                "{mode:?} was allowed on a read-only mount"
            );
        }

        // And a pure read is still permitted.
        assert!(!OpenMode::read().is_write());
        assert!(
            f.vfs
                .open_with(&vp("/ro/readme.txt"), OpenMode::read())
                .is_ok()
        );
    }

    #[test]
    fn create_dir_all_respects_access() {
        let f = fixture();
        f.vfs.create_dir_all(&vp("/work/a/b/c")).unwrap();
        assert!(f.vfs.exists(&vp("/work/a/b/c")));

        let err = f.vfs.create_dir_all(&vp("/ro/a")).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
    }
}
