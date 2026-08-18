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

pub use cap_primitives::fs::AccessModes;

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

/// `ELOOP`, which the kernel reports when it gives up following links itself.
///
/// Spelled numerically per platform rather than via `libc`, which this crate
/// does not depend on, and matched by number rather than by `ErrorKind`,
/// because `ErrorKind::FilesystemLoop` is still unstable.
#[cfg(any(target_os = "linux", target_os = "android"))]
const ELOOP: i32 = 40;
#[cfg(all(unix, not(any(target_os = "linux", target_os = "android"))))]
const ELOOP: i32 = 62;

/// Whether an error reports a symlink loop.
///
/// Two sources produce one: the walk's own hop counter, which sees a cycle that
/// crosses a mount or an absolute target, and the kernel, which sees the
/// ordinary same-directory kind before the walk is ever reached.
#[must_use]
pub fn is_symlink_loop(error: &std::io::Error) -> bool {
    #[cfg(unix)]
    if error.raw_os_error() == Some(ELOOP) {
        return true;
    }

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

/// What a `test` predicate can ask about a path.
///
/// Gathered in one probe rather than exposed as a `Metadata`, because the
/// no-follow case cannot be answered by opening the file and so has no `std`
/// metadata to hand back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileFacts {
    /// Whether it is a directory.
    pub is_dir: bool,
    /// Whether it is a regular file.
    pub is_file: bool,
    /// Whether it is a symbolic link. Only ever true for a no-follow probe.
    pub is_symlink: bool,
    /// Size in bytes.
    pub len: u64,
    /// Whether it is a block device.
    pub is_block_device: bool,
    /// Whether it is a character device.
    pub is_char_device: bool,
    /// Whether it is a FIFO.
    pub is_fifo: bool,
    /// Whether it is a socket.
    pub is_socket: bool,
    /// Whether the set-user-ID bit is set.
    pub is_setuid: bool,
    /// Whether the set-group-ID bit is set.
    pub is_setgid: bool,
    /// Whether the sticky bit is set.
    pub is_sticky: bool,
    /// Owning user and group, which `-O` and `-G` compare.
    pub uid_gid: (u32, u32),
    /// Device and inode, which `-ef` compares, or `None` where the platform
    /// has no such pair.
    ///
    /// `None` rather than a placeholder: a placeholder makes every file
    /// identical to every other one, so `-ef` would answer *true* for any two
    /// files rather than losing the ability to answer at all.
    ///
    /// These are host values, so they leak the host's mount layout into a
    /// namespace that otherwise hides it. Synthesising stable per-session
    /// identifiers is an open question.
    pub dev_ino: Option<(u64, u64)>,
}

impl FileFacts {
    fn from_metadata(metadata: &cap_std::fs::Metadata) -> Self {
        let file_type = metadata.file_type();

        #[cfg(unix)]
        let (mode, dev, ino, uid, gid) = {
            use cap_std::fs::MetadataExt as _;
            (
                metadata.mode(),
                Some(metadata.dev()),
                Some(metadata.ino()),
                metadata.uid(),
                metadata.gid(),
            )
        };
        // No device and inode numbers off Unix. `None` rather than a placeholder
        // pair, because a placeholder makes every file identical to every other
        // one -- which is what `-ef` would then report.
        #[cfg(not(unix))]
        let (mode, dev, ino, uid, gid) = (0u32, None, None, 0u32, 0u32);

        #[cfg(unix)]
        let (is_block_device, is_char_device, is_fifo, is_socket) = {
            use cap_std::fs::FileTypeExt as _;
            (
                file_type.is_block_device(),
                file_type.is_char_device(),
                file_type.is_fifo(),
                file_type.is_socket(),
            )
        };
        #[cfg(not(unix))]
        let (is_block_device, is_char_device, is_fifo, is_socket) = (false, false, false, false);

        Self {
            is_dir: file_type.is_dir(),
            is_file: file_type.is_file(),
            is_symlink: file_type.is_symlink(),
            len: metadata.len(),
            is_block_device,
            is_char_device,
            is_fifo,
            is_socket,
            is_setuid: mode & 0o4000 != 0,
            is_setgid: mode & 0o2000 != 0,
            is_sticky: mode & 0o1000 != 0,
            uid_gid: (uid, gid),
            dev_ino: dev.zip(ino),
        }
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

    /// Locates a path, following symlinks throughout.
    fn locate_follow(&self, path: &VirtualPath) -> std::io::Result<Located<'_>> {
        self.locate(path, true)
    }

    /// Walks `path` component by component, resolving symlinks as it goes.
    ///
    /// Resolution is virtual-path-level rather than descriptor-level because a
    /// symlink may cross a mount boundary: following one has to re-enter the
    /// mount table, not merely descend from wherever the walk had reached.
    ///
    /// The walk carries a handle on the directory it has reached so far, so
    /// that each component costs one probe against that handle. Without it,
    /// every component is probed by its whole path from the mount root and
    /// cap-std re-walks the prefix each time: quadratic in the depth of the
    /// path, which cost 70-230 microseconds per PATH entry on a real `$PATH`.
    /// The handle is dropped whenever the walk stops being a plain descent --
    /// on a symlink restart, on crossing into another mount, or if the
    /// directory cannot be opened -- and the walk falls back to the whole path,
    /// so dropping it costs speed and never correctness.
    fn locate(&self, path: &VirtualPath, follow_final: bool) -> std::io::Result<Located<'_>> {
        let mut resolved = VirtualPath::root();
        let mut pending: Vec<String> = path.components().rev().map(str::to_owned).collect();
        let mut hops = 0usize;

        // The directory `resolved` names, and the mount it was opened from.
        let mut cursor: Option<(&Mount, cap_std::fs::Dir)> = None;

        while let Some(component) = pending.pop() {
            let candidate = resolved.resolve(&component).map_err(|e| {
                std::io::Error::new(std::io::ErrorKind::InvalidInput, e.to_string())
            })?;

            let is_final = pending.is_empty();
            let located = self.locate_exact(&candidate)?;

            // Usable only while the walk stays inside one mount: descending one
            // component from the cursor would otherwise land in the parent
            // mount's directory rather than the nested mount's.
            let descent = cursor
                .as_ref()
                .filter(|(mount, _)| std::ptr::eq(*mount, located.mount))
                .map(|(_, dir)| dir);

            // `symlink_metadata` rather than `metadata`: the question is whether
            // this component *is* a link, not what it points at.
            let is_symlink = match descent {
                Some(dir) => dir.symlink_metadata(&component),
                None => located.mount.dir().symlink_metadata(&located.relative),
            }
            .is_ok_and(|m| m.is_symlink());

            if is_symlink && (!is_final || follow_final) {
                cursor = None;
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

            // Advance the cursor for the next component. A failure here is not
            // an error: the component may not be a directory at all, in which
            // case the next probe reports it by its whole path exactly as it
            // would have without the cursor.
            cursor = if is_final {
                None
            } else {
                let opened = match cursor
                    .as_ref()
                    .filter(|(mount, _)| std::ptr::eq(*mount, located.mount))
                {
                    Some((_, dir)) => dir.open_dir(&component),
                    None => located.mount.dir().open_dir(&located.relative),
                };
                opened.ok().map(|dir| (located.mount, dir))
            };
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

    /// Runs `op` at the location `path` names.
    ///
    /// The whole path goes to cap-std first, in one walk. Its resolution
    /// follows relative symlinks and refuses anything that leaves the mount, so
    /// a success means the path held nothing this namespace would have resolved
    /// differently -- in particular no absolute symlink target, which is the
    /// only case D42 treats specially. It also means the location reached is
    /// inside the mount it started from, since mounts never share a host
    /// directory, so the mount's own access rules still govern.
    ///
    /// cap-std reports leaving the root as `PermissionDenied`, distinct from
    /// the `NotFound` of a path that simply is not there, so the careful
    /// component-by-component walk is only paid for when there is something to
    /// reinterpret. Two other things also report `PermissionDenied` -- a write
    /// refused by a read-only mount, and `access(2)` answering no -- and both
    /// pay for a second walk that reaches the same answer. They are error
    /// paths, and the alternative is matching on cap-std's wording.
    ///
    /// **All of that holds only while a mount's host directory and its virtual
    /// subtree are the same thing.** They stop being the same as soon as
    /// another mount point lies beneath this one: a relative symlink can then
    /// cross the virtual boundary without leaving the host directory, and
    /// cap-std, which knows nothing of the mount table, follows it into the
    /// directory the nested mount shadows. So a mount that shadows a nested one
    /// never takes the fast path.
    ///
    /// One divergence remains and is deliberate. The grammar's *containment*
    /// rules survive here, because cap-std enforces them independently: a
    /// symlink target that climbs past the virtual root, or that is absolute,
    /// is refused either way. Its *portability* rules do not -- a target naming
    /// a reserved device name, a colon or a trailing dot is refused to a caller
    /// but followed here, because a symlink target inside a mount is host data
    /// and this path never inspects it. The reachable set is therefore slightly
    /// larger than the nameable one, with no authority gained; closing it means
    /// walking every path component by component, which is the cost this exists
    /// to avoid.
    fn at<T>(
        &self,
        path: &VirtualPath,
        follow_final: bool,
        op: impl Fn(&Located<'_>) -> std::io::Result<T>,
    ) -> std::io::Result<T> {
        let direct = self.locate_exact(path)?;
        if !direct.mount.shadows_a_nested_mount() {
            match op(&direct) {
                Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {}
                result => return result,
            }
        }

        let located = self.locate(path, follow_final)?;
        op(&located)
    }

    /// Opens a file with the given options.
    ///
    /// # Errors
    ///
    /// Returns [`std::io::Error`] if the path is unmounted, if the mount is
    /// read-only and the options request writing, or if the underlying open
    /// fails.
    pub fn open_with(&self, path: &VirtualPath, mode: OpenMode) -> std::io::Result<std::fs::File> {
        self.at(path, true, |located| {
            // The write check is on the resolved location, not the requested
            // one: a symlink from a writable mount into a read-only one must be
            // governed by where it lands.
            if !located.mount.access().is_writable() && mode.is_write() {
                return Err(read_only(&located.virtual_path));
            }
            Ok(located
                .mount
                .dir()
                .open_with(&located.relative, &mode.to_cap_std())?
                .into_std())
        })
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
        self.at(path, true, |located| {
            // Every branch goes through a descriptor so the result is `std`'s
            // type and callers keep their platform extension traits. A
            // directory cannot be opened as a file, and the mount point has no
            // relative path to open at all -- both were silently broken until
            // `cd /work` failed.
            let file = if located.relative.as_os_str().is_empty() {
                located.mount.dir().try_clone()?.into_std_file()
            } else if located.mount.dir().metadata(&located.relative)?.is_dir() {
                located
                    .mount
                    .dir()
                    .open_dir(&located.relative)?
                    .into_std_file()
            } else {
                located
                    .mount
                    .dir()
                    .open_with(&located.relative, &OpenMode::read().to_cap_std())?
                    .into_std()
            };

            file.metadata()
        })
    }

    /// Whether `path` exists, following symlinks.
    #[must_use]
    pub fn exists(&self, path: &VirtualPath) -> bool {
        self.facts(path, true).is_some()
    }

    /// Kernel-evaluated accessibility, as `access(2)` reports it.
    ///
    /// Mode bits are deliberately not consulted: they give the wrong answer for
    /// root and under ACLs, and `test -w` has to agree with what a write would
    /// actually do. A read-only mount is also unwritable regardless of what the
    /// host permits, since the mount is the narrower authority.
    #[must_use]
    pub fn access(&self, path: &VirtualPath, modes: AccessModes) -> bool {
        self.at(path, true, |located| {
            if modes.writable && !located.mount.access().is_writable() {
                return Err(read_only(&located.virtual_path));
            }

            let dir = located
                .mount
                .dir()
                .try_clone()
                .map(cap_std::fs::Dir::into_std_file)?;

            cap_primitives::fs::access(
                &dir,
                &located.relative,
                cap_primitives::fs::AccessType::Access(modes),
                cap_primitives::fs::FollowSymlinks::Yes,
            )
        })
        .is_ok()
    }

    /// Everything a `test` predicate needs about a path, or `None` if it names
    /// nothing in this namespace.
    ///
    /// `None` covers unmounted paths and paths the grammar rejects as well as
    /// missing ones, because a predicate must answer *false* for all three.
    /// bash reports a missing file as false rather than as an error, and an
    /// unmounted path is missing as far as the sandbox is concerned.
    #[must_use]
    pub fn facts(&self, path: &VirtualPath, follow: bool) -> Option<FileFacts> {
        self.at(path, follow, |located| {
            // An empty relative path is the mount point itself, which the `Dir`
            // handle already names -- `metadata("")` would simply fail, which is
            // why `[[ -d /work ]]` was false for a mounted directory.
            let metadata = if located.relative.as_os_str().is_empty() {
                located.mount.dir().dir_metadata()?
            } else if follow {
                located.mount.dir().metadata(&located.relative)?
            } else {
                located.mount.dir().symlink_metadata(&located.relative)?
            };

            Ok(FileFacts::from_metadata(&metadata))
        })
        .ok()
    }

    /// Whether `path` is a symlink, without following it.
    #[must_use]
    pub fn is_symlink(&self, path: &VirtualPath) -> bool {
        self.facts(path, false).is_some_and(|f| f.is_symlink)
    }

    /// The path with every symlink resolved, still expressed virtually.
    ///
    /// # Errors
    ///
    /// Returns [`std::io::Error`] if the path does not resolve in this
    /// namespace.
    pub fn canonicalize(&self, path: &VirtualPath) -> std::io::Result<VirtualPath> {
        Ok(self.locate_follow(path)?.virtual_path)
    }

    /// The host path of the file `path` names, with every symlink resolved.
    ///
    /// **The one place a host path leaves this crate.** It exists because
    /// executing a program needs a name the operating system understands and
    /// there is no descriptor-based alternative in portable Rust -- `fexecve`
    /// has no `std` equivalent. It is not for sandboxed code, which has no way
    /// to use a host path and must never be handed one.
    ///
    /// Symlinks are resolved before the join, so the answer is where the file
    /// actually is rather than a name that might point elsewhere once the
    /// kernel follows it. Without that, translating a virtual path for `exec`
    /// would hand the kernel a link the namespace had approved and let it
    /// resolve the target against the host.
    ///
    /// # Errors
    ///
    /// Returns [`std::io::Error`] if the path does not resolve in this
    /// namespace.
    pub fn host_path(&self, path: &VirtualPath) -> std::io::Result<PathBuf> {
        let located = self.locate_follow(path)?;
        Ok(located.mount.canonical_host_path().join(&located.relative))
    }

    /// Reads a symlink's target verbatim, without resolving it.
    ///
    /// # Errors
    ///
    /// Returns [`std::io::Error`] if the path is unmounted or is not a symlink.
    pub fn read_link(&self, path: &VirtualPath) -> std::io::Result<PathBuf> {
        self.at(path, false, |located| {
            read_link_contents(located.mount, &located.relative)
        })
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
        self.at(path, true, |located| {
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
        })
    }

    /// Creates a directory, and any missing parents.
    ///
    /// # Errors
    ///
    /// Returns [`std::io::Error`] if the path is unmounted, the mount is
    /// read-only, or creation fails.
    pub fn create_dir_all(&self, path: &VirtualPath) -> std::io::Result<()> {
        self.at(path, false, |located| {
            Self::require_writable(located)?;
            located.mount.dir().create_dir_all(&located.relative)
        })
    }

    /// Removes a file.
    ///
    /// # Errors
    ///
    /// Returns [`std::io::Error`] if the path is unmounted, the mount is
    /// read-only, or removal fails.
    pub fn remove_file(&self, path: &VirtualPath) -> std::io::Result<()> {
        self.at(path, false, |located| {
            Self::require_writable(located)?;
            located.mount.dir().remove_file(&located.relative)
        })
    }

    /// Removes an empty directory.
    ///
    /// # Errors
    ///
    /// As [`Vfs::remove_file`].
    pub fn remove_dir(&self, path: &VirtualPath) -> std::io::Result<()> {
        self.at(path, false, |located| {
            Self::require_writable(located)?;
            located.mount.dir().remove_dir(&located.relative)
        })
    }

    /// Removes a directory and everything beneath it.
    ///
    /// # Errors
    ///
    /// Returns [`std::io::Error`] if the path is unmounted, the mount is
    /// read-only, or removal fails.
    pub fn remove_dir_all(&self, path: &VirtualPath) -> std::io::Result<()> {
        self.at(path, false, |located| {
            Self::require_writable(located)?;
            located.mount.dir().remove_dir_all(&located.relative)
        })
    }

    /// Renames a file or directory.
    ///
    /// # Errors
    ///
    /// Returns [`std::io::Error`] if either path is unmounted, if either mount
    /// is read-only, or if the rename fails. Renaming across a mount boundary
    /// returns `CrossesDevices`: a mount boundary *is* a filesystem boundary as
    /// far as `renameat` is concerned, and reporting it as one lets callers use
    /// the copy-then-delete fallback they already have for `EXDEV` rather than
    /// learn a new failure. Emulating the move here would silently turn an
    /// atomic operation into one that is not.
    pub fn rename(&self, from: &VirtualPath, to: &VirtualPath) -> std::io::Result<()> {
        let from = self.locate(from, false)?;
        let to = self.locate(to, false)?;

        if !std::ptr::eq(from.mount, to.mount) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::CrossesDevices,
                format!(
                    "rename across a mount boundary: {from} to {to}",
                    from = from.virtual_path,
                    to = to.virtual_path
                ),
            ));
        }

        Self::require_writable(&from)?;
        Self::require_writable(&to)?;
        from.mount
            .dir()
            .rename(&from.relative, to.mount.dir(), &to.relative)
    }

    /// Creates a symbolic link at `path` pointing at `target`.
    ///
    /// The target is validated, and rewritten when it has to be. An absolute
    /// target names a place in *this* namespace, but the link is a host artifact
    /// that outlives the run -- so it is stored relative to the link, and then
    /// means the same thing whether it is followed inside the sandbox or by
    /// whatever copies the workspace afterwards. A target in another mount
    /// cannot be expressed that way and is refused. `read_link` reports what was
    /// stored, so it reports the rewritten form.
    ///
    /// # Errors
    ///
    /// Returns [`std::io::Error`] if the link's path is unmounted or read-only,
    /// if the target does not name anywhere in this namespace, if it lies in
    /// another mount, or if creation fails.
    pub fn symlink(&self, path: &VirtualPath, target: &str) -> std::io::Result<()> {
        let link = self.locate(path, false)?;
        Self::require_writable(&link)?;

        let stored = if target.starts_with('/') {
            let resolved = VirtualPath::new(target).map_err(|e| {
                std::io::Error::new(std::io::ErrorKind::InvalidInput, e.to_string())
            })?;
            let landing = self.locate_exact(&resolved)?;
            if !std::ptr::eq(landing.mount, link.mount) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("symlink target {resolved} is in another mount"),
                ));
            }
            relative_from(&link.relative, &landing.relative)
        } else {
            // A relative target is already host-meaningful. It still has to stay
            // inside the namespace once followed, which is the same check the
            // walk would apply, made here so the link is never written at all.
            let parent = link.virtual_path.parent().unwrap_or_else(VirtualPath::root);
            parent.resolve(target).map_err(|e| {
                std::io::Error::new(std::io::ErrorKind::InvalidInput, e.to_string())
            })?;
            target.to_owned()
        };

        cap_fs_ext::DirExt::symlink(link.mount.dir(), &stored, &link.relative)
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
/// Expresses `target` relative to the directory holding `link`, both given
/// relative to one mount's root.
fn relative_from(link: &std::path::Path, target: &std::path::Path) -> String {
    let link_dir: Vec<_> = link
        .parent()
        .map(|p| p.components().collect())
        .unwrap_or_default();
    let target: Vec<_> = target.components().collect();

    let shared = link_dir
        .iter()
        .zip(&target)
        .take_while(|(a, b)| a == b)
        .count();

    let ups = std::iter::repeat_n("..", link_dir.len() - shared);
    let downs = target[shared..]
        .iter()
        .map(|c| c.as_os_str().to_string_lossy().into_owned());

    let parts: Vec<String> = ups.map(ToOwned::to_owned).chain(downs).collect();
    if parts.is_empty() {
        ".".to_owned()
    } else {
        parts.join("/")
    }
}

fn read_link_contents(mount: &Mount, relative: &std::path::Path) -> std::io::Result<PathBuf> {
    let dir = mount.dir().try_clone()?.into_std_file();
    cap_primitives::fs::read_link_contents(&dir, relative)
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "test fixtures are built on the host, which is the one place that is the point"
)]
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

    /// A virtually nested pair of mounts on *disjoint host directories*, which
    /// the builder permits: it checks that host directories do not overlap, and
    /// these do not.
    ///
    ///   /work        -> <tmp>/outer   rw
    ///   /work/vendor -> <tmp>/inner   ro
    ///
    /// `<tmp>/outer` also holds a real `vendor` directory that the nested mount
    /// shadows, and a plain relative symlink `link -> vendor`. The symlink never
    /// leaves the outer mount's host directory, so nothing at the host level
    /// objects to it -- but virtually it crosses a mount boundary.
    #[cfg(unix)]
    fn nested_mounts() -> (tempfile::TempDir, Vfs) {
        let root = tempfile::tempdir().expect("temp dir");
        let outer = root.path().join("outer");
        let inner = root.path().join("inner");
        std::fs::create_dir(&outer).expect("mkdir outer");
        std::fs::create_dir(&inner).expect("mkdir inner");

        std::fs::write(inner.join("lib.rs"), b"real").expect("write inner");
        std::fs::create_dir(outer.join("vendor")).expect("mkdir shadow");
        std::fs::write(outer.join("vendor").join("lib.rs"), b"shadowed").expect("write shadow");
        std::fs::write(outer.join("vendor").join("hidden.txt"), b"hidden").expect("write hidden");
        std::os::unix::fs::symlink("vendor", outer.join("link")).expect("symlink");

        let mounts = MountTable::builder()
            .mount("/work", &outer, Access::ReadWrite)
            .expect("outer mount")
            .mount("/work/vendor", &inner, Access::ReadOnly)
            .expect("nested mount")
            .build()
            .expect("disjoint host dirs must build");

        (root, Vfs::new(mounts))
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_across_a_nested_mount_boundary_lands_in_the_nested_mount() {
        // A virtual mount boundary is not a host directory boundary. Resolving
        // the whole path in one step would follow `link` into the directory the
        // nested mount shadows, without ever leaving the outer mount's host
        // directory -- so nothing at the host level would object.
        let (_root, vfs) = nested_mounts();
        assert_eq!(read(&vfs, "/work/vendor/lib.rs").unwrap(), "real");
        assert_eq!(read(&vfs, "/work/link/lib.rs").unwrap(), "real");
    }

    #[cfg(unix)]
    #[test]
    fn a_shadowed_directory_is_not_reachable_through_a_symlink() {
        // `hidden.txt` exists only in the directory the nested mount shadows,
        // so no virtual path names it. A path that resolved in the outer
        // mount's host tree would reach it.
        let (_root, vfs) = nested_mounts();
        assert!(read(&vfs, "/work/vendor/hidden.txt").is_err());
        assert!(read(&vfs, "/work/link/hidden.txt").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn a_read_only_nested_mount_is_read_only_through_a_symlink_too() {
        // The access mode must come from where the path lands, not from which
        // mount the caller happened to name.
        let (_root, vfs) = nested_mounts();
        assert_eq!(
            vfs.create(&vp("/work/vendor/new.txt")).unwrap_err().kind(),
            std::io::ErrorKind::PermissionDenied
        );
        assert_eq!(
            vfs.create(&vp("/work/link/new.txt")).unwrap_err().kind(),
            std::io::ErrorKind::PermissionDenied
        );
        assert!(!vfs.access(
            &vp("/work/link/lib.rs"),
            AccessModes {
                readable: false,
                writable: true,
                executable: false
            }
        ));
    }

    #[cfg(unix)]
    #[test]
    fn every_answer_about_a_nested_mount_agrees_with_canonicalize() {
        // The failure this pins was not that one answer was wrong, but that two
        // answers about the same path disagreed: `canonicalize` named the
        // nested mount while `open` read the shadowed tree.
        let (_root, vfs) = nested_mounts();
        assert_eq!(
            vfs.canonicalize(&vp("/work/link/lib.rs")).unwrap(),
            vp("/work/vendor/lib.rs")
        );
        assert_eq!(vfs.read_dir_names(&vp("/work/link")).unwrap(), ["lib.rs"]);
        assert!(vfs.facts(&vp("/work/link/lib.rs"), true).is_some());
        assert!(vfs.facts(&vp("/work/link/hidden.txt"), true).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn a_nested_mount_is_reachable_through_a_symlink_with_nothing_to_shadow() {
        // The same crossing, with no directory in the outer mount for the
        // nested one to shadow. Resolving the whole path in one step reports
        // the file missing, which fails closed and is still wrong.
        let root = tempfile::tempdir().expect("temp dir");
        let outer = root.path().join("outer");
        let inner = root.path().join("inner");
        std::fs::create_dir(&outer).expect("mkdir outer");
        std::fs::create_dir(&inner).expect("mkdir inner");
        std::fs::write(inner.join("lib.rs"), b"real").expect("write inner");
        std::os::unix::fs::symlink("vendor", outer.join("link")).expect("symlink");

        let mounts = MountTable::builder()
            .mount("/work", &outer, Access::ReadWrite)
            .expect("outer mount")
            .mount("/work/vendor", &inner, Access::ReadOnly)
            .expect("nested mount")
            .build()
            .expect("disjoint host dirs must build");
        let vfs = Vfs::new(mounts);

        assert_eq!(read(&vfs, "/work/link/lib.rs").unwrap(), "real");
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_target_the_grammar_reserves_for_portability_is_followed() {
        // A documented divergence, pinned so it cannot drift further.
        //
        // The grammar's *containment* rules -- no escape past the virtual root,
        // no absolute target taken literally -- are enforced independently by
        // cap-std, so they hold on every path (see the two tests above this
        // one). Its *portability* rules are not: reserved device names, colons
        // and trailing dots are refused to a caller naming a path, but a
        // symlink target inside a mount is host data, and resolving a path in
        // one step follows it.
        //
        // No authority is gained -- the target is inside a mount the caller
        // already has -- but the reachable set is larger than the nameable one.
        // Closing it means resolving every path component by component, which
        // is what the fast path exists to avoid.
        let f = fixture();
        std::fs::write(work_host(&f).join("con.txt"), b"reserved").expect("write");
        std::os::unix::fs::symlink("con.txt", work_host(&f).join("res")).expect("symlink");

        assert!(
            VirtualPath::new("/work/con.txt").is_err(),
            "the grammar reserves this name"
        );
        assert_eq!(read(&f.vfs, "/work/res").unwrap(), "reserved");
    }

    #[cfg(unix)]
    #[test]
    fn an_absolute_symlink_target_is_stored_relative_to_the_link() {
        // The link outlives the run, so it has to mean the same thing to
        // whatever copies the workspace afterwards.
        let f = fixture();
        f.vfs
            .symlink(&vp("/work/sub/link.txt"), "/work/hello.txt")
            .expect("same mount");

        assert_eq!(
            f.vfs.read_link(&vp("/work/sub/link.txt")).unwrap(),
            std::path::PathBuf::from("../hello.txt"),
            "readlink reports what was stored, which is the rewritten form"
        );
        assert_eq!(read(&f.vfs, "/work/sub/link.txt").unwrap(), "hello");

        // And the host agrees, which is the whole point.
        let host = work_host(&f).join("sub").join("link.txt");
        assert_eq!(std::fs::read_to_string(host).unwrap(), "hello");
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_target_in_another_mount_is_refused() {
        // Expressible virtually, not expressible relatively on the host.
        let f = fixture();
        let err = f
            .vfs
            .symlink(&vp("/work/cross.txt"), "/ro/readme.txt")
            .unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_target_that_would_escape_is_refused_before_it_is_written() {
        let f = fixture();
        let err = f
            .vfs
            .symlink(&vp("/work/evil.txt"), "../../../../etc/passwd")
            .unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        assert!(
            !work_host(&f).join("evil.txt").exists(),
            "a refused link must not reach the host at all"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_cannot_be_created_in_a_read_only_mount() {
        let f = fixture();
        let err = f
            .vfs
            .symlink(&vp("/ro/link.txt"), "readme.txt")
            .unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn rename_within_a_mount_moves_the_file() {
        let f = fixture();
        f.vfs
            .rename(&vp("/work/hello.txt"), &vp("/work/sub/moved.txt"))
            .expect("same mount");
        assert_eq!(read(&f.vfs, "/work/sub/moved.txt").unwrap(), "hello");
        assert!(f.vfs.facts(&vp("/work/hello.txt"), false).is_none());
    }

    #[test]
    fn rename_across_a_mount_boundary_reports_crossing_devices() {
        // So that callers use the copy-then-delete fallback they already have
        // for EXDEV rather than learn a new failure.
        let f = fixture();
        let err = f
            .vfs
            .rename(&vp("/work/hello.txt"), &vp("/ro/hello.txt"))
            .unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::CrossesDevices);
    }

    #[test]
    fn rename_into_a_read_only_mount_is_refused() {
        let f = fixture();
        let mounts = MountTable::builder()
            .mount("/work", work_host(&f), Access::ReadOnly)
            .expect("mount")
            .build()
            .expect("build");
        let ro = Vfs::new(mounts);
        let err = ro
            .rename(&vp("/work/hello.txt"), &vp("/work/other.txt"))
            .unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn remove_dir_all_respects_the_mount_s_access() {
        let f = fixture();
        assert!(f.vfs.remove_dir_all(&vp("/work/sub")).is_ok());
        assert!(f.vfs.facts(&vp("/work/sub"), false).is_none());

        let err = f.vfs.remove_dir_all(&vp("/ro")).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
    }

    #[cfg(unix)]
    #[test]
    fn an_absolute_symlink_in_the_middle_of_a_path_still_resolves_in_the_root() {
        // The whole path goes to cap-std first, and cap-std refuses an absolute
        // symlink target wherever it appears. This is the case that has to fall
        // back to the careful walk, and it must fall back for an *interior*
        // component, not only a final one.
        let f = fixture();
        std::os::unix::fs::symlink("/ro", work_host(&f).join("elsewhere")).unwrap();

        assert_eq!(
            read(&f.vfs, "/work/elsewhere/readme.txt").unwrap(),
            "readme"
        );

        // And landing in a read-only mount by that route is still read-only.
        let err = f.vfs.create(&vp("/work/elsewhere/new.txt")).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);

        // The predicates agree with the open.
        assert!(
            f.vfs
                .facts(&vp("/work/elsewhere/readme.txt"), true)
                .is_some()
        );
        assert_eq!(
            f.vfs
                .canonicalize(&vp("/work/elsewhere/readme.txt"))
                .unwrap(),
            vp("/ro/readme.txt")
        );
    }

    #[cfg(unix)]
    #[test]
    fn an_interior_symlink_pointing_out_of_the_namespace_is_refused() {
        // A relative target that climbs past the virtual root must be refused
        // whether it is reached by the fast path or the careful walk.
        let f = fixture();
        std::os::unix::fs::symlink("../../../../etc", work_host(&f).join("out")).unwrap();
        assert!(read(&f.vfs, "/work/out/passwd").is_err());
        assert!(f.vfs.facts(&vp("/work/out/passwd"), true).is_none());
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
    fn the_mount_point_itself_has_facts() {
        // Regression: the mount root's relative path is empty, and
        // `metadata("")` fails -- so `[[ -d /work ]]` was false for a directory
        // that plainly existed.
        let f = fixture();
        let facts = f
            .vfs
            .facts(&vp("/work"), true)
            .expect("mount root has facts");
        assert!(facts.is_dir);
        assert!(f.vfs.exists(&vp("/work")));
    }

    #[test]
    fn facts_are_absent_for_unmounted_paths() {
        // The semantic every `test` predicate depends on: unmounted is not an
        // error, it is simply nothing, so predicates answer false as bash does
        // for a missing file.
        let f = fixture();
        assert!(f.vfs.facts(&vp("/etc/passwd"), true).is_none());
        assert!(f.vfs.facts(&vp("/etc"), true).is_none());
    }

    #[test]
    fn a_read_only_mount_is_never_writable() {
        let f = fixture();
        let w = AccessModes {
            readable: false,
            writable: true,
            executable: false,
        };
        assert!(!f.vfs.access(&vp("/ro/readme.txt"), w));
        assert!(f.vfs.access(&vp("/work/hello.txt"), w));
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

// ===========================================================================
// TEMPORARY: adversarial review of adeadc0's fast path in `Vfs::at`.
// Added by a security review; remove or fold in once triaged.
// ===========================================================================
