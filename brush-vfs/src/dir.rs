//! A directory capability, for callers that are already descriptor-shaped.
//!
//! [`crate::fs`] is path-based and hands back `std` types, and that stays the
//! default: it is what the D4 codemod emits and what every rewritten utility
//! uses. This module is the deliberate exception, and D3's amendment records
//! why.
//!
//! `uucore::safe_traversal::DirFd` is the motivating caller — 1,464 lines of
//! `openat`/`fstatat`/`unlinkat`/`mkdirat` written so that a recursive walk
//! cannot be redirected between the check and the use, and what `chmod -R` and
//! `chown -R` descend with. Rooting it at a path and re-resolving per operation
//! would confine it while destroying the property it exists for: every
//! re-resolution is a fresh chance for a component to become a symlink. So it
//! gets a descriptor, anchored beneath a mount.
//!
//! # What keeps this from being a hole
//!
//! Two properties, both asserted by test rather than left to review:
//!
//! 1. **Nothing here yields a host path.** No method returns a `PathBuf` or
//!    otherwise lets a holder learn where in the host filesystem it is
//!    standing. `cap_std::fs::Dir` has three ways to do that — `canonicalize`,
//!    `open_parent_dir`, and the `AsFd`/`into_std_file` conversions — and none
//!    is re-exported.
//! 2. **Every operation names a single component, never a path.** `..` and `/`
//!    are rejected before `cap-std` is asked, so a holder cannot walk upward
//!    even to a location still inside the namespace, let alone out of it. This
//!    is belt-and-braces: `cap-std`'s beneath-the-root resolution would refuse
//!    an escape anyway, and refusing at the API keeps the *reason* legible.
//!
//! What a holder can do is descend, and read or modify what it finds — which is
//! exactly the authority the mount it came from already grants by path.
//!
//! # The exception to that, stated plainly
//!
//! [`Dir::into_owned_fd_for_at_traversal`] surrenders the raw descriptor, and a
//! raw descriptor *can* be walked upward with `openat(fd, "..")`. So a caller
//! holding one is **rooted** in the namespace, not sealed inside it. It exists
//! for `DirFd`, which is a raw fd with its own syscalls and whose alternative
//! today is accepting any host path at all. The gate that enforces property 1
//! names this method explicitly, so a second such method fails the build rather
//! than joining it quietly.

use crate::fs::{OpenMode, Vfs};
use crate::path::VirtualPath;

/// A directory inside the namespace, usable for `*at`-style operations.
///
/// Obtained from [`Vfs::open_dir`]. Cloning is a `dup(2)` of the descriptor via
/// [`Dir::try_clone`]; there is deliberately no `Clone` impl, since a silent
/// per-clone `dup` is how a deep pipeline exhausts its fd table.
#[derive(Debug)]
pub struct Dir {
    inner: cap_std::fs::Dir,
    /// Whether the mount this descends from allows writes. Carried in userspace
    /// for the same reason [`OpenMode`] carries write intent: a `Dir` fd does
    /// not remember that its mount was read-only.
    writable: bool,
}

/// Rejects anything that is not a single, ordinary path component.
///
/// The empty string, `.`, `..` and anything containing a separator are refused.
/// A holder of a `Dir` therefore has no vocabulary for "upward" at all.
fn component(name: &str) -> std::io::Result<&str> {
    if name.is_empty() || name == "." || name == ".." || name.contains('/') || name.contains('\\') {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("not a single path component: {name:?}"),
        ));
    }
    Ok(name)
}

/// What makes two directory entries the same file, and two files the same
/// filesystem.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileIdentity {
    /// The filesystem the file lives on.
    pub device: u64,
    /// The file within that filesystem.
    pub file: u64,
}

#[cfg(unix)]
fn identity_of(file: &std::fs::File) -> std::io::Result<FileIdentity> {
    use std::os::unix::fs::MetadataExt as _;

    let md = file.metadata()?;
    Ok(FileIdentity {
        device: md.dev(),
        file: md.ino(),
    })
}

#[cfg(windows)]
fn identity_of(file: &std::fs::File) -> std::io::Result<FileIdentity> {
    use std::os::windows::io::AsRawHandle as _;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };

    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: `file` is a live open handle for the duration of the call, and
    // `info` is a properly sized, writable `BY_HANDLE_FILE_INFORMATION`.
    let ok = unsafe { GetFileInformationByHandle(file.as_raw_handle() as _, &raw mut info) };
    if ok == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(FileIdentity {
        device: u64::from(info.dwVolumeSerialNumber),
        file: (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
    })
}

fn read_only() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::PermissionDenied,
        "read-only mount".to_string(),
    )
}

impl Dir {
    /// Wraps an already-confined `cap-std` handle. Crate-internal: the only way
    /// to obtain one from outside is [`Vfs::open_dir`], which resolves through
    /// the mount table first.
    pub(crate) const fn new(inner: cap_std::fs::Dir, writable: bool) -> Self {
        Self { inner, writable }
    }

    /// Whether the mount this descends from allows writes.
    #[must_use]
    pub const fn is_writable(&self) -> bool {
        self.writable
    }

    /// Surrenders the descriptor for `*at` traversal by a caller that does its
    /// own syscalls.
    ///
    /// **This is the one documented hole in this module, and it is narrower
    /// than "no confinement" but wider than the rest of this type.** The
    /// descriptor is confined *at the moment it is produced*: it names a
    /// directory that resolution already proved is inside the namespace, and
    /// nothing about the path that produced it survives. What it does not carry
    /// is the refusal of `..`. A holder doing raw `openat(fd, "..")` walks to
    /// the real parent and can keep going to the host root, because a directory
    /// descriptor is a position in the host tree and the kernel will happily
    /// move upward from it.
    ///
    /// It exists for `uucore::safe_traversal::DirFd`, which is a raw `OwnedFd`
    /// with `openat`/`fstatat`/`unlinkat`/`fchownat` written directly against
    /// it, plus public `AsFd`/`AsRawFd` impls its callers use. Porting it onto
    /// this type's sealed API is the sound alternative and a milestone in its
    /// own right — it needs `chown`, which `cap-std` does not provide at all.
    /// Against that, `DirFd::open` today takes *any host path outright*, so
    /// rooting it here is a large gain for small work, and the residual risk is
    /// that a future upstream change starts passing `".."` where it passes
    /// directory entry names today.
    ///
    /// Do not reach for this to avoid a missing method. Add the method.
    ///
    /// Unix only, because a `DirFd` is: `uucore::safe_traversal` is itself
    /// `#[cfg(unix)]`. Nothing else on this type is platform-specific.
    #[cfg(unix)]
    #[must_use]
    pub fn into_owned_fd_for_at_traversal(self) -> std::os::fd::OwnedFd {
        use std::os::fd::OwnedFd;

        OwnedFd::from(self.inner.into_std_file())
    }

    /// Duplicates the descriptor.
    ///
    /// # Errors
    ///
    /// If the underlying `dup` fails.
    pub fn try_clone(&self) -> std::io::Result<Self> {
        Ok(Self {
            inner: self.inner.try_clone()?,
            writable: self.writable,
        })
    }

    /// Opens a subdirectory by name.
    ///
    /// # Errors
    ///
    /// If `name` is not a single component, or the directory cannot be opened.
    pub fn open_dir(&self, name: &str) -> std::io::Result<Self> {
        let name = component(name)?;
        Ok(Self {
            inner: self.inner.open_dir(name)?,
            writable: self.writable,
        })
    }

    /// Opens a file in this directory.
    ///
    /// # Errors
    ///
    /// If `name` is not a single component, the mount is read-only and `mode`
    /// writes, or the open fails.
    pub fn open_file(&self, name: &str, mode: OpenMode) -> std::io::Result<std::fs::File> {
        let name = component(name)?;
        if mode.is_write() && !self.writable {
            return Err(read_only());
        }
        Ok(self.inner.open_with(name, &mode.to_cap_std())?.into_std())
    }

    /// Metadata for an entry, following a final symlink.
    ///
    /// # Errors
    ///
    /// If `name` is not a single component, or the entry cannot be stat'd.
    pub fn metadata(&self, name: &str) -> std::io::Result<std::fs::Metadata> {
        let name = component(name)?;
        // cap-std's `Metadata` cannot be converted into `std`'s, which has no
        // public constructor, so the entry is opened and the descriptor stat'd
        // -- the same approach `Vfs::metadata` takes, including opening a
        // directory as a directory rather than as a file.
        let file = if self.inner.metadata(name)?.is_dir() {
            self.inner.open_dir(name)?.into_std_file()
        } else {
            self.inner
                // Opened only to `fstat`; without O_NONBLOCK a FIFO entry
                // would wait for a writer that never comes.
                .open_with(name, &OpenMode::read().with_nonblock(true).to_cap_std())?
                .into_std()
        };
        file.metadata()
    }

    /// Metadata for an entry, **not** following a final symlink.
    ///
    /// What a walk needs under `follow_links(false)`: a symlink to a directory
    /// must report as a symlink, or the walk descends into it. Shares its
    /// implementation with [`Vfs::symlink_metadata`], so the two cannot drift.
    ///
    /// # Errors
    ///
    /// If `name` is not a single component, or the entry cannot be stat'd.
    pub fn symlink_metadata(&self, name: &str) -> std::io::Result<std::fs::Metadata> {
        let name = component(name)?;
        crate::fs::symlink_metadata_at(&self.inner, std::ffi::OsStr::new(name))
    }

    /// This directory's identity, for loop detection and same-filesystem tests.
    ///
    /// `(device, file)` — `(dev, ino)` on Unix, and volume serial plus file
    /// index on Windows, where `std::fs::Metadata`'s accessors for those are
    /// still nightly-only, so it goes to `GetFileInformationByHandle` directly.
    /// Taken from the open handle rather than a path, so it cannot describe
    /// something other than the directory this capability names.
    ///
    /// # Errors
    ///
    /// If the handle cannot be duplicated or interrogated.
    pub fn identity(&self) -> std::io::Result<FileIdentity> {
        identity_of(&self.inner.try_clone()?.into_std_file())
    }

    /// Metadata for this directory itself.
    ///
    /// # Errors
    ///
    /// If the descriptor cannot be stat'd.
    pub fn self_metadata(&self) -> std::io::Result<std::fs::Metadata> {
        self.inner.try_clone()?.into_std_file().metadata()
    }

    /// Whether an entry exists, following symlinks.
    #[must_use]
    pub fn exists(&self, name: &str) -> bool {
        component(name).is_ok_and(|n| self.inner.exists(n))
    }

    /// The names of this directory's entries, excluding `.` and `..`, in the
    /// order the filesystem reported them.
    ///
    /// Deliberately unsorted: a recursive walk has to yield what `readdir`
    /// yielded, because that is what the utilities built on one expect, and
    /// sorting here would hide the real order from a caller that wants it.
    /// Callers wanting a stable order sort themselves.
    ///
    /// Names rather than `DirEntry`s: a `cap-std` entry can be asked for its
    /// path, and handing one out would defeat the point.
    ///
    /// # Errors
    ///
    /// If the directory cannot be read.
    pub fn entry_names(&self) -> std::io::Result<Vec<String>> {
        let mut names = Vec::new();
        for entry in self.inner.entries()? {
            let entry = entry?;
            names.push(entry.file_name().to_string_lossy().into_owned());
        }
        Ok(names)
    }

    /// Creates a subdirectory.
    ///
    /// # Errors
    ///
    /// If `name` is not a single component, the mount is read-only, or the
    /// create fails.
    pub fn create_dir(&self, name: &str) -> std::io::Result<()> {
        let name = component(name)?;
        if !self.writable {
            return Err(read_only());
        }
        self.inner.create_dir(name)
    }

    /// Removes a file.
    ///
    /// # Errors
    ///
    /// If `name` is not a single component, the mount is read-only, or the
    /// removal fails.
    pub fn remove_file(&self, name: &str) -> std::io::Result<()> {
        let name = component(name)?;
        if !self.writable {
            return Err(read_only());
        }
        self.inner.remove_file(name)
    }

    /// Removes an empty subdirectory.
    ///
    /// # Errors
    ///
    /// If `name` is not a single component, the mount is read-only, or the
    /// removal fails.
    pub fn remove_dir(&self, name: &str) -> std::io::Result<()> {
        let name = component(name)?;
        if !self.writable {
            return Err(read_only());
        }
        self.inner.remove_dir(name)
    }

    /// Sets an entry's permission bits.
    ///
    /// # Errors
    ///
    /// If `name` is not a single component, the mount is read-only, or the
    /// change fails.
    #[cfg(unix)]
    pub fn set_permissions(&self, name: &str, mode: u32) -> std::io::Result<()> {
        use cap_std::fs::{Permissions, PermissionsExt as _};

        let name = component(name)?;
        if !self.writable {
            return Err(read_only());
        }
        self.inner
            .set_permissions(name, Permissions::from_mode(mode))
    }
}

impl Vfs {
    /// Opens a directory in the namespace as a capability.
    ///
    /// The path is resolved through the mount table exactly as any other
    /// operation is — symlinks followed, escapes refused — and only then is the
    /// descriptor handed out. Confinement is established here, once; everything
    /// the returned [`Dir`] can do is `*at`-relative to it.
    ///
    /// # Errors
    ///
    /// If the path does not resolve, escapes the namespace, or is not a
    /// directory.
    pub fn open_dir(&self, path: &VirtualPath) -> std::io::Result<Dir> {
        let (dir, writable) = self.locate_dir(path)?;
        Ok(Dir::new(dir, writable))
    }
}

#[cfg(test)]
#[expect(
    clippy::disallowed_methods,
    reason = "test fixtures are built on the host, which is the one place that is the point"
)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::mount::{Access, MountTable};

    fn vfs_at(root: &std::path::Path, access: Access) -> Arc<Vfs> {
        let mounts = MountTable::builder()
            .mount("/work", root, access)
            .expect("mount")
            .build()
            .expect("mount table");
        Arc::new(Vfs::new(mounts))
    }

    fn vp(s: &str) -> VirtualPath {
        VirtualPath::root().resolve(s).expect("virtual path")
    }

    #[test]
    fn a_capability_descends_and_reads() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("sub")).unwrap();
        std::fs::write(tmp.path().join("sub/f.txt"), b"hello").unwrap();

        let vfs = vfs_at(tmp.path(), Access::ReadWrite);
        let root = vfs.open_dir(&vp("/work")).unwrap();
        let sub = root.open_dir("sub").unwrap();

        let mut names = sub.entry_names().unwrap();
        names.sort();
        assert_eq!(names, vec!["f.txt".to_string()]);
        assert_eq!(sub.metadata("f.txt").unwrap().len(), 5);
        assert!(sub.self_metadata().unwrap().is_dir());
    }

    #[test]
    fn the_capability_no_follow_stat_reports_the_link_not_the_target() {
        // Gate 1. A walk under `follow_links(false)` descends into anything the
        // stat calls a directory, so a symlink-to-directory reported as its
        // target is how a walk leaves the tree it was told to walk.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("real")).unwrap();
        std::fs::write(tmp.path().join("real/f.txt"), b"x").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("real", tmp.path().join("link")).unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_dir("real", tmp.path().join("link")).unwrap();

        let vfs = vfs_at(tmp.path(), Access::ReadWrite);
        let root = vfs.open_dir(&vp("/work")).unwrap();

        assert!(root.symlink_metadata("link").unwrap().is_symlink());
        assert!(!root.symlink_metadata("link").unwrap().is_dir());
        // Following, which is what `metadata` does, still sees the directory.
        assert!(root.metadata("link").unwrap().is_dir());
        // And a plain directory is a directory either way.
        assert!(root.symlink_metadata("real").unwrap().is_dir());
    }

    #[test]
    fn a_capability_cannot_name_a_parent() {
        // The property D3's amendment rests on. `..` is refused before cap-std
        // is asked, so a holder has no vocabulary for "upward" -- not even to a
        // location still inside the namespace.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("sub")).unwrap();

        let vfs = vfs_at(tmp.path(), Access::ReadWrite);
        let sub = vfs.open_dir(&vp("/work/sub")).unwrap();

        for bad in ["..", ".", "", "../..", "sub/nested", "/etc", "a\\b"] {
            let err = sub.open_dir(bad).unwrap_err();
            assert_eq!(
                err.kind(),
                std::io::ErrorKind::InvalidInput,
                "{bad:?} must be refused as a non-component"
            );
        }
    }

    #[test]
    fn a_capability_cannot_follow_a_symlink_out_of_the_namespace() {
        // safe_traversal is what `chmod -R` descends with, so a root that leaks
        // on traversal confines nothing.
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret"), b"nope").unwrap();
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("sub")).unwrap();

        #[cfg(unix)]
        std::os::unix::fs::symlink(outside.path(), tmp.path().join("sub/escape")).unwrap();

        let vfs = vfs_at(tmp.path(), Access::ReadWrite);
        let sub = vfs.open_dir(&vp("/work/sub")).unwrap();

        #[cfg(unix)]
        {
            assert!(
                sub.open_dir("escape").is_err(),
                "a symlink to outside the mount must not open"
            );
            assert!(!sub.exists("escape"));
        }
    }

    #[test]
    fn a_read_only_mount_refuses_every_mutation() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("f.txt"), b"x").unwrap();

        let vfs = vfs_at(tmp.path(), Access::ReadOnly);
        let root = vfs.open_dir(&vp("/work")).unwrap();

        assert!(!root.is_writable());
        assert!(root.metadata("f.txt").is_ok(), "reads still work");
        for kind in [
            root.create_dir("new").err(),
            root.remove_file("f.txt").err(),
            root.remove_dir("nope").err(),
            root.open_file("f.txt", OpenMode::write()).err(),
        ] {
            assert_eq!(
                kind.map(|e| e.kind()),
                Some(std::io::ErrorKind::PermissionDenied)
            );
        }
    }

    #[test]
    fn no_public_method_can_yield_a_host_path() {
        // Gate 9, and the single property D3's amendment rests on: a holder may
        // descend and act, but must never learn where in the host filesystem it
        // is standing. `cap_std::fs::Dir` offers three routes out --
        // `canonicalize`, `open_parent_dir`, and the `AsFd`/`into_std_file`
        // conversions -- so "we did not re-export those" needs to stay true as
        // the type grows.
        //
        // Asserted against the source text because the thing being checked is
        // the *shape of the public API*, which no runtime call can observe. It
        // fails loudly the moment someone adds a returning method that leaks,
        // which is what a mechanism whose failure mode is silence requires.
        // The single documented carve-out, named so that a *second* one still
        // fails this test. Its own caveats are on the method; the point here is
        // that surrendering a descriptor stays a deliberate, reviewed act
        // rather than something that accretes.
        const ALLOWED: &str = "into_owned_fd_for_at_traversal";

        let source = include_str!("dir.rs");
        let banned = [
            "PathBuf",
            "&Path",
            "as_fd",
            "as_raw_fd",
            "OwnedFd",
            "RawFd",
            "into_std_file",
            "cap_std::fs::Dir",
        ];

        for line in source.lines() {
            let line = line.trim();
            let Some(rest) = line
                .strip_prefix("pub fn ")
                .or_else(|| line.strip_prefix("pub const fn "))
            else {
                continue;
            };
            if rest.starts_with(ALLOWED) {
                continue;
            }
            // Only the return type matters: taking a `&str` name is the point.
            let Some((_, ret)) = rest.split_once("->") else {
                continue;
            };
            for bad in banned {
                assert!(
                    !ret.contains(bad),
                    "public method `{}` returns `{}`, which can name a host \
                     location -- see D3's amendment",
                    rest.split('(').next().unwrap_or(rest),
                    ret.trim()
                );
                let _ = bad;
            }
        }
    }

    #[test]
    fn opening_a_directory_outside_the_namespace_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let vfs = vfs_at(tmp.path(), Access::ReadWrite);
        assert!(vfs.open_dir(&vp("/etc")).is_err());
    }
}
