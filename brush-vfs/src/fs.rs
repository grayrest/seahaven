//! Filesystem operations over the virtual namespace.
//!
//! The API is **path-based and `std`-typed**: it takes [`VirtualPath`] and hands
//! back `std::fs::File` and `std::fs::Metadata`. That is not a stylistic choice
//! — the utilities this eventually has to accommodate carry owned paths and
//! re-open them repeatedly, so an API *demanding* a `Dir` would force them to be
//! restructured rather than rewritten.
//!
//! It is sound because confinement comes from *resolution*, not from the handle
//! type. Once a descriptor has been opened beneath a mount it carries no ambient
//! authority, so returning a plain `std::fs::File` gives nothing away.
//!
//! # The one exception
//!
//! A caller that is *already* descriptor-shaped may ask for a directory
//! capability instead: see [`crate::dir`]. `uucore::safe_traversal::DirFd` is
//! the motivating case — an `openat`-anchored walk written so a recursive
//! `chmod -R` cannot be redirected between the check and the use. Handing it
//! paths to re-resolve would confine it while destroying the property it exists
//! for.
//!
//! This narrows D3 rather than reversing it. Path-based access stays the default
//! and is the only thing the codemod emits; the capability is opt-in, and the
//! type it hands back can perform `*at` operations but cannot name a host path.
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
    /// Refuse a final symlink (`O_NOFOLLOW`).
    nofollow: bool,
    /// Permission bits for a newly created inode.
    mode: Option<u32>,
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
            nofollow: false,
            mode: None,
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
            nofollow: false,
            mode: None,
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
            nofollow: false,
            mode: None,
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

    /// Refuses to follow a final symlink, failing with `ELOOP` instead.
    ///
    /// What `uucore::safe_copy` opens its source and destination with, and the
    /// reason it does is worth keeping: an attacker who plants a symlink at the
    /// path between the caller's `lstat` and this open would otherwise redirect
    /// the read — or, for a destination, redirect a truncate onto any file the
    /// caller can write. `cap-std` resolution already refuses a link *out* of
    /// the namespace; this refuses one inside it too, which is the caller's
    /// intent rather than the namespace's.
    #[must_use]
    pub const fn with_nofollow(mut self, yes: bool) -> Self {
        self.nofollow = yes;
        self
    }

    /// Sets the permission bits a *newly created* file is given.
    ///
    /// Only applies when the open creates the inode; an existing file keeps its
    /// mode, which is why `safe_copy` widens permissions after the content copy
    /// rather than at open time.
    #[must_use]
    pub const fn with_mode(mut self, mode: u32) -> Self {
        self.mode = Some(mode);
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
    pub(crate) fn to_cap_std(self) -> cap_std::fs::OpenOptions {
        let mut options = cap_std::fs::OpenOptions::new();
        options
            .read(self.read)
            .write(self.write)
            .append(self.append)
            .create(self.create)
            .create_new(self.create_new)
            .truncate(self.truncate);

        #[cfg(unix)]
        {
            use cap_std::fs::OpenOptionsExt as _;

            // `nofollow` is deliberately *not* forwarded as a flag: see
            // `Vfs::open_with`, which answers it during resolution because a
            // final symlink is followed before any descriptor exists.
            if let Some(mode) = self.mode {
                options.mode(mode);
            }
        }

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

    /// Resolves `path` and opens it as a confined `cap-std` directory handle,
    /// reporting whether its mount permits writes.
    ///
    /// The single place a directory capability is minted. Everything a
    /// [`crate::dir::Dir`] can do afterwards is `*at`-relative to what this
    /// returns, so confinement is established here exactly once.
    pub(crate) fn locate_dir(
        &self,
        path: &VirtualPath,
    ) -> std::io::Result<(cap_std::fs::Dir, bool)> {
        let located = self.locate_follow(path)?;
        let writable = located.mount.access().is_writable();
        let dir = if located.relative.as_os_str().is_empty() {
            // The mount point itself has no relative path to descend.
            located.mount.dir().try_clone()?
        } else {
            located.mount.dir().open_dir(&located.relative)?
        };
        Ok((dir, writable))
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

            let located = match self.locate_exact(&candidate) {
                Ok(located) => located,
                // A prefix with a mount beneath it is part of the namespace's
                // own shape rather than any host's: `/a` exists because
                // `/a/b/work` is mounted, and nothing backs it. It cannot be a
                // symlink, so step through it. Without this the walk fails for
                // every mount point deeper than one component, which took
                // `rename` and `symlink` -- the two that call it directly --
                // out entirely.
                // ...but never the virtual root itself. `has_mount_below` is
                // true for `/` whenever any non-root mount exists, and a `..`
                // in a symlink target reaches `/` as a candidate. Stepping
                // through it and re-descending by *mount point* name is not
                // what the host does, because a mount point is not its host
                // directory's name: `../work/x` from a mount `/work` on
                // `<root>/project` reads `<root>/project/x` here and
                // `<root>/work/x` there.
                Err(e)
                    if !is_final
                        && !candidate.is_root()
                        && self.mounts.has_mount_below(&candidate) =>
                {
                    let _ = e;
                    resolved = candidate;
                    cursor = None;
                    continue;
                }
                Err(e) => return Err(e),
            };

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
        // `O_NOFOLLOW` has to be answered here rather than passed down as a
        // flag. Resolution walks the path itself and *follows* a final symlink
        // before any open happens, so the descriptor cap-std would apply the
        // flag to is already the target -- the flag would be set on something
        // that is by construction not a link, and silently never fire. Asking
        // the namespace whether the final component is a link is the same
        // question `O_NOFOLLOW` asks the kernel.
        //
        // The race this leaves is narrower than it looks: if the final
        // component becomes a symlink between this check and the open,
        // cap-std's own beneath-the-root re-check still refuses an escape. What
        // is lost is only the caller's stricter intent, not confinement.
        if mode.nofollow && self.is_symlink(path) {
            return Err(std::io::Error::from_raw_os_error(ELOOP));
        }

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

    /// Metadata for `path`, **not** following a final symlink. The vfs's
    /// `symlink_metadata`.
    ///
    /// cap-std has no `std::fs::Metadata`-returning form: its own `Metadata`
    /// cannot be turned back into `std`'s, which has no public constructor. So
    /// on Unix this opens the link *as itself* -- `O_PATH`/`O_SYMLINK` with
    /// `O_NOFOLLOW`, relative to the link's parent directory opened through
    /// cap-std so the traversal stays confined and `RESOLVE_BENEATH` still
    /// governs it -- and `fstat`s that descriptor. A non-symlink final component
    /// is opened normally, so the answer matches `metadata` there, as
    /// `symlink_metadata` requires.
    ///
    /// On non-Unix it falls back to [`metadata`](Self::metadata), which
    /// *follows* the final link. That is wrong for a symlink and is a documented
    /// Windows limitation, pending the deferred Windows symlink work.
    ///
    /// # Errors
    ///
    /// Returns [`std::io::Error`] if the path is unmounted or the query fails.
    pub fn symlink_metadata(&self, path: &VirtualPath) -> std::io::Result<std::fs::Metadata> {
        #[cfg(not(unix))]
        {
            return self.metadata(path);
        }

        #[cfg(unix)]
        {
            let located = self.locate(path, false)?;

            // The mount point itself is a directory, never a symlink.
            if located.relative.as_os_str().is_empty() {
                return located.mount.dir().try_clone()?.into_std_file().metadata();
            }

            let name = located.relative.file_name().ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "path has no final component",
                )
            })?;
            // The parent is opened through cap-std, so intermediate components
            // are resolved with the same confinement as every other access; only
            // the single final component is opened by name below.
            let parent = match located.relative.parent() {
                Some(p) if !p.as_os_str().is_empty() => located.mount.dir().open_dir(p)?,
                _ => located.mount.dir().try_clone()?,
            };
            symlink_metadata_at(&parent, name)
        }
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

        // Moving a symlink moves its stored bytes, and the same bytes mean
        // something else from a different directory. `ln -s ..` is valid one
        // level down and hands out the mount's parent at the root, so a link
        // whose target does not survive the move is refused rather than
        // silently relocated.
        if from
            .mount
            .dir()
            .symlink_metadata(&from.relative)
            .is_ok_and(|m| m.is_symlink())
        {
            let stored = read_link_contents(from.mount, &from.relative)?;
            let stored = stored.to_string_lossy();
            if !stored_target_stays_in_mount(&to.relative, &stored) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("moving this link would point {stored} out of the mount"),
                ));
            }
        }

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

        // Resolve the target the same way whichever way it was spelled. The
        // first version of this checked a relative target with
        // `parent.resolve(target)` alone and stored it verbatim, on the
        // reasoning that a relative target is "already host-meaningful". It is
        // not. `resolve` asks only whether a path stays inside the *virtual
        // root*, and the virtual root sits above every mount point that is not
        // `/` -- so `../secret.txt` from a link in `/work` named a valid,
        // merely unmounted virtual path, passed, and on the host climbed out of
        // the mount directory. `ln -s .. up` handed out the mount's parent.
        let parent = link.virtual_path.parent().unwrap_or_else(VirtualPath::root);
        let landing = parent
            .resolve(target)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e.to_string()))?;

        // The target must name a place in this namespace -- being merely inside
        // the virtual root is not enough -- and that place must be in the same
        // mount as the link, because a virtual `..` out of a mount is a host
        // `..` into an unrelated directory.
        let lexical = self.locate_exact(&landing)?;
        if !std::ptr::eq(lexical.mount, link.mount) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("symlink target {landing} is in another mount"),
            ));
        }

        // And following it must stay here too, so that a new link cannot be
        // chained onto an escaping one already on disk. A target that does not
        // exist yet is fine: the walk does not require the final component to
        // be there.
        let followed = self.locate(&landing, true)?;
        if !std::ptr::eq(followed.mount, link.mount) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("symlink target {landing} leads out of the mount"),
            ));
        }

        // Stored relative to the link, from the *lexical* landing rather than
        // the followed one, so `read_link` reports what was asked for wherever
        // that is expressible rather than a resolved form.
        let stored = relative_from(&link.relative, &lexical.relative);
        if !stored_target_stays_in_mount(&link.relative, &stored) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("symlink target {landing} leaves the mount when followed"),
            ));
        }
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

/// Whether `stored`, followed from the directory holding `link`, stays within
/// the mount -- as the *host* would follow it, with no mount table involved.
///
/// This is the invariant that actually matters for a link, and it is lexical:
/// the host resolves a stored target against the link's own directory and knows
/// nothing about virtual paths. Checking containment virtually is not the same
/// question, which is how `../secret.txt` came to be written once already.
fn stored_target_stays_in_mount(link_relative: &std::path::Path, stored: &str) -> bool {
    if stored.starts_with('/') {
        return false;
    }

    // How deep the link's own directory sits inside the mount.
    let mut depth = link_relative.parent().map_or(0, |p| p.components().count());

    for component in stored.split('/') {
        match component {
            "" | "." => {}
            ".." => match depth.checked_sub(1) {
                Some(shallower) => depth = shallower,
                None => return false,
            },
            _ => depth += 1,
        }
    }

    true
}

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

/// Reads a symlink's target exactly as stored.
///
/// `cap_std::fs::Dir::read_link` refuses a target that is absolute — it has no
/// root to interpret one against, so it reports "a path led outside of the
/// filesystem" even for a link this namespace can resolve perfectly well. The
/// underlying primitive still validates the path *to* the link against the
/// mount root; only the returned contents are raw, which is what D42 needs.
/// Expresses `target` relative to the directory holding `link`, both given
/// relative to one mount's root.
fn read_link_contents(mount: &Mount, relative: &std::path::Path) -> std::io::Result<PathBuf> {
    let dir = mount.dir().try_clone()?.into_std_file();
    cap_primitives::fs::read_link_contents(&dir, relative)
}

/// `fstat`s a single directory entry `name` without following it if it is a
/// symlink, returning `std::fs::Metadata`.
///
/// Opens the entry relative to `parent`'s descriptor so no host path is used and
/// the operation cannot race a rename of an intermediate directory. `O_PATH`
/// (Linux/BSD) or `O_SYMLINK` (macOS) is what lets a symlink be opened as itself
/// rather than followed; a non-symlink is opened normally, so its metadata is
/// the same one `metadata` would return.
#[cfg(unix)]
fn symlink_metadata_at(
    parent: &cap_std::fs::Dir,
    name: &std::ffi::OsStr,
) -> std::io::Result<std::fs::Metadata> {
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::ffi::OsStrExt;

    let cname = std::ffi::CString::new(name.as_bytes()).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "directory entry name contains an interior NUL",
        )
    })?;

    #[cfg(any(target_os = "macos", target_os = "ios"))]
    let flags = libc::O_SYMLINK | libc::O_CLOEXEC;
    #[cfg(not(any(target_os = "macos", target_os = "ios")))]
    let flags = libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC;

    #[expect(
        clippy::disallowed_methods,
        reason = "brush-vfs is the facade; this is the confined primitive it exists to provide, \
                  opening a single named entry relative to a cap-std-resolved parent descriptor"
    )]
    // SAFETY: `parent` is a live directory descriptor for the duration of the
    // call, and `cname` is a valid NUL-terminated C string. `openat` returns a
    // fresh owned descriptor or -1.
    let fd = unsafe { libc::openat(parent.as_raw_fd(), cname.as_ptr(), flags) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `fd` is a fresh, valid, owned descriptor returned by `openat`;
    // wrapping it in a `File` transfers ownership so it is closed on drop.
    let file = unsafe { std::fs::File::from_raw_fd(fd) };
    file.metadata()
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
    fn nofollow_refuses_a_final_symlink_even_inside_the_namespace() {
        // `uucore::safe_copy` opens with `O_NOFOLLOW` so an attacker who plants
        // a symlink between the caller's lstat and the open cannot redirect the
        // read -- or, at a destination, redirect a truncate onto any file the
        // caller can write. Both ends of that link are *inside* the namespace
        // here, so confinement alone does not answer it.
        //
        // It has to be enforced during resolution: passing O_NOFOLLOW down as a
        // flag is inert, because the final link is followed before any
        // descriptor exists and the flag lands on the target.
        let fx = fixture();
        fx.vfs
            .symlink(&vp("/work/link.txt"), "hello.txt")
            .expect("symlink");

        // Following is the default and still works.
        assert!(fx.vfs.open(&vp("/work/link.txt")).is_ok());

        let err = fx
            .vfs
            .open_with(&vp("/work/link.txt"), OpenMode::read().with_nofollow(true))
            .expect_err("a final symlink must be refused under nofollow");
        assert_eq!(err.raw_os_error(), Some(ELOOP));

        // A plain file is unaffected -- nofollow is about links, not about
        // refusing everything.
        assert!(
            fx.vfs
                .open_with(&vp("/work/hello.txt"), OpenMode::read().with_nofollow(true))
                .is_ok()
        );
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
    fn symlink_metadata_sees_the_link_while_metadata_sees_the_target() {
        let f = fixture();
        std::os::unix::fs::symlink("hello.txt", work_host(&f).join("link.txt")).unwrap();

        // Following (metadata) lands on the 5-byte target file.
        let followed = f.vfs.metadata(&vp("/work/link.txt")).unwrap();
        assert!(followed.is_file());
        assert!(!followed.is_symlink());
        assert_eq!(followed.len(), 5);

        // Not following (symlink_metadata) sees the link itself.
        let link = f.vfs.symlink_metadata(&vp("/work/link.txt")).unwrap();
        assert!(link.is_symlink());
        assert!(!link.is_file());

        // For a non-symlink the two agree.
        let plain = f.vfs.symlink_metadata(&vp("/work/hello.txt")).unwrap();
        assert!(plain.is_file());
        assert!(!plain.is_symlink());
        assert_eq!(plain.len(), 5);
    }

    #[cfg(unix)]
    #[test]
    fn symlink_metadata_stays_confined() {
        let f = fixture();
        // A link pointing outside the namespace: symlink_metadata reports on the
        // link itself (it does not follow), and an unmounted path is not found.
        std::os::unix::fs::symlink("/etc/passwd", work_host(&f).join("escape.txt")).unwrap();
        assert!(f.vfs.symlink_metadata(&vp("/work/escape.txt")).unwrap().is_symlink());
        assert_eq!(
            f.vfs.symlink_metadata(&vp("/etc/passwd")).unwrap_err().kind(),
            std::io::ErrorKind::NotFound
        );
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

    /// A mount whose point is deeper than one component, on a host directory
    /// with a sibling outside it.
    #[cfg(unix)]
    fn deep_mount() -> (tempfile::TempDir, Vfs) {
        let root = tempfile::tempdir().expect("temp dir");
        let inside = root.path().join("inside");
        std::fs::create_dir(&inside).expect("mkdir");
        std::fs::write(inside.join("hello.txt"), b"hello").expect("write");
        std::fs::write(root.path().join("outside.txt"), b"outside").expect("write");

        let mounts = MountTable::builder()
            .mount("/a/b/work", &inside, Access::ReadWrite)
            .expect("mount")
            .build()
            .expect("build");
        (root, Vfs::new(mounts))
    }

    /// A mount whose *point* is spelled differently from its host directory,
    /// with a decoy on the host at the mount point's name.
    ///
    /// The ordinary fixture mounts `/work` on a directory called `work`, so a
    /// virtual `..`-and-back and a host `..`-and-back land in the same place
    /// and an entire class of divergence is invisible. Adversarial review found
    /// three defects with this shape after the ordinary fixture found none.
    #[cfg(unix)]
    fn divergent_mount() -> (tempfile::TempDir, Vfs) {
        let root = tempfile::tempdir().expect("temp dir");
        let project = root.path().join("project");
        std::fs::create_dir(&project).expect("mkdir project");
        std::fs::create_dir(project.join("sub")).expect("mkdir sub");
        std::fs::write(project.join("hello.txt"), b"hello").expect("write");

        // Outside every mount, named so that a virtual `/work/x` and a host
        // `../work/x` are different files.
        std::fs::create_dir(root.path().join("work")).expect("mkdir decoy");
        std::fs::write(
            root.path().join("work").join("secret.txt"),
            b"OUTSIDE EVERY MOUNT",
        )
        .expect("write");

        let mounts = MountTable::builder()
            .mount("/work", &project, Access::ReadWrite)
            .expect("mount")
            .build()
            .expect("build");
        (root, Vfs::new(mounts))
    }

    #[cfg(unix)]
    #[test]
    fn a_link_cannot_be_relocated_into_meaning_something_else() {
        // `ln -s ..` is valid one level down -- it names the mount root -- and
        // hands out the mount's *parent* from the root. Creation validated the
        // target against the directory the link was created in; moving it
        // moved the same bytes somewhere they mean something else.
        let (root, vfs) = divergent_mount();
        vfs.symlink(&vp("/work/sub/up"), "..")
            .expect("valid one level down");

        let err = vfs
            .rename(&vp("/work/sub/up"), &vp("/work/up"))
            .expect_err("moving it must be refused");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);

        assert!(
            !root.path().join("project").join("up").exists(),
            "nothing may be left at the destination"
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_walk_does_not_step_through_the_virtual_root() {
        // A `..` in a symlink target reaches `/` as a candidate. Stepping
        // through it and re-descending by *mount point* name is not what the
        // host does, because a mount point is not its host directory's name --
        // so the vfs read one file and the host another through one link.
        let (root, vfs) = divergent_mount();
        std::os::unix::fs::symlink(
            "../work/secret.txt",
            root.path().join("project").join("via"),
        )
        .expect("plant");

        assert!(
            read(&vfs, "/work/via").is_err(),
            "the namespace must not follow it"
        );
        assert!(vfs.facts(&vp("/work/via"), true).is_none());

        // The host still follows it -- the link was planted, not written here,
        // and no namespace can un-plant one. What matters is that the vfs does
        // not read through it, and does not report it as resolving inside.
        assert_eq!(
            std::fs::read_to_string(root.path().join("project").join("via")).ok(),
            Some("OUTSIDE EVERY MOUNT".to_owned())
        );
    }

    #[cfg(unix)]
    #[test]
    fn no_sequence_of_calls_leaves_a_link_pointing_out_of_the_mount() {
        // The property, stated as itself rather than as a list of blocked
        // routes: after any accepted sequence, every link in the mount must
        // resolve inside it *on the host*.
        let (root, vfs) = divergent_mount();
        // Canonicalized, because the comparison below is against canonicalized
        // link targets and macOS resolves /var to /private/var.
        let project = root
            .path()
            .join("project")
            .canonicalize()
            .expect("canonicalize the mount root");

        assert!(vfs.symlink(&vp("/work/ok"), "hello.txt").is_ok());
        assert!(vfs.symlink(&vp("/work/sub/back"), "../hello.txt").is_ok());
        let _ = vfs.symlink(&vp("/work/up"), "..");
        let _ = vfs.symlink(&vp("/work/out"), "../work/secret.txt");
        let _ = vfs.symlink(&vp("/work/sub/deep"), "../../work/secret.txt");
        let _ = vfs.rename(&vp("/work/sub/back"), &vp("/work/back"));

        for entry in std::fs::read_dir(&project)
            .into_iter()
            .flatten()
            .chain(std::fs::read_dir(project.join("sub")).into_iter().flatten())
            .flatten()
        {
            let path = entry.path();
            if !path.is_symlink() {
                continue;
            }
            let resolved = path
                .parent()
                .expect("a link has a parent")
                .join(std::fs::read_link(&path).expect("readlink"));
            let canonical = resolved
                .canonicalize()
                .unwrap_or_else(|_| resolved.components().collect());
            assert!(
                canonical.starts_with(&project),
                "{} resolves to {} on the host, outside {}",
                path.display(),
                canonical.display(),
                project.display()
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_mount_point_deeper_than_one_component_still_walks() {
        // The walk probes each prefix, and `/a` is backed by nothing: it exists
        // only because `/a/b/work` is mounted. Requiring it to resolve took
        // `rename` and `symlink` out entirely, since they walk with no fast
        // path in front of them.
        let (_root, vfs) = deep_mount();
        assert!(
            vfs.rename(&vp("/a/b/work/hello.txt"), &vp("/a/b/work/moved.txt"))
                .is_ok()
        );
        assert!(vfs.symlink(&vp("/a/b/work/link"), "moved.txt").is_ok());
        assert_eq!(
            vfs.canonicalize(&vp("/a/b/work/link")).unwrap(),
            vp("/a/b/work/moved.txt")
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_relative_symlink_target_cannot_escape_the_mount_on_the_host() {
        // The escape this whole check exists to prevent, and the one the first
        // version wrote to disk. `resolve` asks only whether a path stays inside
        // the *virtual root*, which sits above every mount point that is not
        // `/`, so `../outside.txt` was a valid virtual path that merely named
        // nothing -- and on the host it climbed out of the mount directory.
        let (root, vfs) = deep_mount();
        let err = vfs
            .symlink(&vp("/a/b/work/pwn.txt"), "../outside.txt")
            .unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
        assert!(
            !root.path().join("inside").join("pwn.txt").exists(),
            "a refused link must not reach the host at all"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_link_to_dotdot_does_not_hand_out_the_mount_s_parent() {
        // `ln -s ..` under `--mount /work:$HOME/project` left a live handle to
        // $HOME in the workspace -- inert in the sandbox and fully live to
        // whatever copies it afterwards, which is D26's threat model inverted.
        let f = fixture();
        let err = vfs_symlink_err(&f.vfs, "/work/up", "..");
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
        assert!(!work_host(&f).join("up").exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_cross_mount_target_is_refused_in_either_spelling() {
        // The mount check lived only in the absolute branch, so the relative
        // spelling of the same target walked straight past it.
        let f = fixture();
        assert!(f.vfs.symlink(&vp("/work/abs"), "/ro/readme.txt").is_err());
        assert!(f.vfs.symlink(&vp("/work/rel"), "../ro/readme.txt").is_err());
        assert!(!work_host(&f).join("abs").exists());
        assert!(!work_host(&f).join("rel").exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_link_cannot_be_chained_onto_an_escaping_link_already_on_disk() {
        // The vfs refuses to *follow* such a link, but happily pointed a new
        // one at it -- so the new link meant something different outside.
        let f = fixture();
        // Outside every mount: the fixture mounts `<root>/work` and `<root>/ro`,
        // so `<root>/escape.txt` is in neither.
        let outside = work_host(&f)
            .parent()
            .expect("work has a parent")
            .join("escape.txt");
        std::fs::write(&outside, b"outside").expect("write");
        std::os::unix::fs::symlink("../escape.txt", work_host(&f).join("out"))
            .expect("plant an escaping link");
        assert!(
            read(&f.vfs, "/work/out").is_err(),
            "following it is refused"
        );

        // Refused; the kind depends on *why* the target leaves the namespace
        // and is not the property under test.
        let _ = vfs_symlink_err(&f.vfs, "/work/chain", "out");
        assert!(!work_host(&f).join("chain").exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_dangling_target_is_still_allowed() {
        // `ln -s` to a file that does not exist yet is ordinary and must work;
        // the containment check must not become an existence check.
        let f = fixture();
        assert!(f.vfs.symlink(&vp("/work/later"), "not-yet.txt").is_ok());
        assert_eq!(
            f.vfs.read_link(&vp("/work/later")).unwrap(),
            std::path::PathBuf::from("not-yet.txt")
        );
    }

    #[cfg(unix)]
    fn vfs_symlink_err(vfs: &Vfs, link: &str, target: &str) -> std::io::Error {
        vfs.symlink(&vp(link), target)
            .expect_err("this target must be refused")
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

// ===========================================================================
// TEMPORARY: adversarial review of 8f3510b (`symlink`, `rename`,
// `remove_dir_all`, `relative_from`). Added by a security review; remove or
// fold in once triaged.
//
// Each test asserts the INVARIANT THE DESIGN CLAIMS. A test that fails is a
// broken claim, and its panic message is the transcript.
// ===========================================================================
