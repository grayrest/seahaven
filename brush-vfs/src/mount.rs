//! The mount table: how virtual paths become host directories.
//!
//! A mount binds a virtual path to a host directory and an access mode. Their
//! union *is* the sandbox's `/`. Nothing outside a mount is nameable, which is
//! the property that makes an escape unrepresentable rather than merely denied.
//!
//! Loading is where a policy is checked, and it is deliberately strict. Two
//! layouts that would behave differently on different hosts are refused here
//! rather than discovered at open time, when the caller has no way to tell an
//! ambiguity from a missing file.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::path::{VirtualPath, fold_for_collision};

/// What a mount permits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// Reads only.
    ///
    /// Enforced by this layer, not by the kernel: a directory descriptor
    /// carries no write bit that propagates to the files opened through it. Any
    /// access that reaches the host without passing through here is unaffected,
    /// which is why an OS-level layer is the only thing that makes `ReadOnly`
    /// a guarantee rather than a convention.
    ReadOnly,
    /// Reads and writes.
    ReadWrite,
}

impl Access {
    /// Whether writes are permitted.
    #[must_use]
    pub const fn is_writable(self) -> bool {
        matches!(self, Self::ReadWrite)
    }
}

/// Why a mount table could not be built.
#[derive(Debug, thiserror::Error)]
pub enum MountError {
    /// A mount point is not a valid virtual path.
    #[error("invalid mount point: {0}")]
    Path(#[from] crate::path::PathError),

    /// The host directory could not be opened.
    #[error("cannot open host directory {path}: {source}")]
    HostDir {
        /// The host directory that could not be opened.
        path: PathBuf,
        /// The underlying error.
        source: std::io::Error,
    },

    /// Two mounts share a mount point.
    #[error("duplicate mount point: {0}")]
    DuplicateMountPoint(String),

    /// Two mount points differ only by case or Unicode form.
    ///
    /// They would be one directory on a case-insensitive host and two
    /// elsewhere, so the layout has no single meaning.
    #[error("mount points {first} and {second} collide when case-folded")]
    CollidingMountPoints {
        /// The mount point loaded first.
        first: String,
        /// The mount point that collides with it.
        second: String,
    },

    /// Two mounts' host directories overlap.
    ///
    /// Overlap is what lets a hard link in a writable mount reach content a
    /// read-only mount was meant to protect: the two mounts are the same inode
    /// tree under different names, so the access mode is decided by which name
    /// the caller happened to use.
    #[error("host directory {inner} lies inside {outer}; mounts must be disjoint")]
    OverlappingHostDirs {
        /// The nested directory.
        inner: PathBuf,
        /// The directory containing it.
        outer: PathBuf,
    },
}

/// One binding of a virtual path to a host directory.
pub struct Mount {
    at: VirtualPath,
    dir: cap_std::fs::Dir,
    /// The host directory this mount was opened from, when there was one.
    ///
    /// `None` for a mount rebuilt from a handle received over the broker (D24):
    /// a child is handed capabilities, not paths, so it genuinely does not know
    /// where on the host its namespace lives. That is D3's contract expressed
    /// in the type rather than by convention -- a child cannot leak a host path
    /// it was never given.
    host_path: Option<PathBuf>,
    canonical_host_path: Option<PathBuf>,
    access: Access,
    /// Whether another mount point lies strictly beneath this one.
    ///
    /// Computed once at build time because it decides, per operation, whether
    /// this mount's host directory may be walked in one step. See
    /// [`Mount::shadows_a_nested_mount`].
    shadows_nested: bool,
}

impl Mount {
    /// Where this mount appears in the virtual namespace.
    #[must_use]
    pub const fn mount_point(&self) -> &VirtualPath {
        &self.at
    }

    /// The capability handle for the mount's root directory.
    ///
    /// Deliberately crate-private: a `Dir` is the authority itself, and the
    /// facade's whole contract is that callers receive paths and descriptors
    /// rather than capabilities they could re-root from.
    pub(crate) const fn dir(&self) -> &cap_std::fs::Dir {
        &self.dir
    }

    /// What this mount permits.
    #[must_use]
    pub const fn access(&self) -> Access {
        self.access
    }

    /// The host directory this mount was opened from.
    ///
    /// For diagnostics and policy loading only. It is never handed to sandboxed
    /// code, which has no way to name a host path.
    ///
    /// `None` when the mount came from a broker handshake rather than from a
    /// host directory this process opened.
    #[must_use]
    pub fn host_path(&self) -> Option<&Path> {
        self.host_path.as_deref()
    }

    /// The mount's host directory with every symlink resolved.
    ///
    /// Used only to build a host path for the operating system when there is no
    /// descriptor-based alternative -- today, executing a program. Canonical
    /// rather than as-written, because a path joined onto a symlinked mount root
    /// resolves somewhere the mount table never approved.
    ///
    /// `None` for a broker-received mount, which is why translating a virtual
    /// path for `exec` fails there rather than guessing.
    pub(crate) fn canonical_host_path(&self) -> Option<&Path> {
        self.canonical_host_path.as_deref()
    }

    /// Whether another mount point lies strictly beneath this one.
    ///
    /// When it does, this mount's host directory cannot be walked in one step,
    /// because a virtual mount boundary is not a host directory boundary. A
    /// relative symlink inside this mount can cross the boundary without ever
    /// leaving the host directory, so the host's own resolution follows it into
    /// the directory the nested mount *shadows* -- reaching content the
    /// namespace cannot name, and answering to this mount's access mode rather
    /// than the nested mount's. Only the component-by-component walk re-enters
    /// the mount table after each symlink hop.
    #[must_use]
    pub const fn shadows_a_nested_mount(&self) -> bool {
        self.shadows_nested
    }
}

impl std::fmt::Debug for Mount {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Mount")
            .field("at", &self.at)
            .field("host_path", &self.host_path)
            .field("access", &self.access)
            .finish_non_exhaustive()
    }
}

/// The set of mounts forming one virtual namespace.
#[derive(Debug, Default)]
pub struct MountTable {
    /// Sorted by descending component count, so the first match in iteration
    /// order is the longest one.
    mounts: Vec<Mount>,
}

impl MountTable {
    /// Starts building a table.
    #[must_use]
    pub fn builder() -> MountTableBuilder {
        MountTableBuilder::default()
    }

    /// Finds the mount governing `path`, with the path's components relative to
    /// that mount's root.
    ///
    /// Longest match wins, so a mount at `/work/target` takes precedence over
    /// one at `/work` for anything beneath it.
    #[must_use]
    pub fn resolve<'s, 'p>(&'s self, path: &'p VirtualPath) -> Option<(&'s Mount, Vec<&'p str>)> {
        self.mounts
            .iter()
            .find_map(|m| path.strip_prefix(&m.at).map(|rest| (m, rest)))
    }

    /// Whether some mount point lies strictly beneath `path`.
    ///
    /// Such a path is a directory of the namespace's own making: `/a` exists
    /// when `/a/b/work` is mounted even though nothing on any host backs it.
    /// A walk has to step through it without probing, because there is no
    /// directory to probe.
    #[must_use]
    pub(crate) fn has_mount_below(&self, path: &VirtualPath) -> bool {
        self.mounts
            .iter()
            .any(|m| m.at != *path && m.at.starts_with(path))
    }

    /// The mounts, longest mount point first.
    pub fn mounts(&self) -> impl Iterator<Item = &Mount> {
        self.mounts.iter()
    }

    /// Whether the table has no mounts, in which case nothing is reachable.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.mounts.is_empty()
    }

    /// Orders mounts and records nesting, the two invariants `resolve` and the
    /// walk depend on.
    ///
    /// Factored out because a table can now be built two ways -- from host
    /// directories, and from handles received over the broker (D24) -- and an
    /// invariant established in only one of them is the kind that holds until
    /// the day it does not.
    fn from_mounts(mut mounts: Vec<Mount>) -> Self {
        // Longest mount point first, so `resolve` can take the first match.
        mounts.sort_by_key(|m| std::cmp::Reverse(m.at.components().count()));

        // Record which mounts have another mount point beneath them. Done here
        // rather than asked per operation because the answer never changes and
        // the question is on the hot path.
        let nested: Vec<bool> = mounts
            .iter()
            .map(|outer| {
                mounts
                    .iter()
                    .any(|inner| inner.at != outer.at && inner.at.starts_with(&outer.at))
            })
            .collect();
        for (mount, shadows_nested) in mounts.iter_mut().zip(nested) {
            mount.shadows_nested = shadows_nested;
        }

        Self { mounts }
    }

    /// Lends each mount's capability handle to a child of this process (D24).
    ///
    /// **The second amendment to D3**, after `Dir::into_owned_fd_for_at_traversal`,
    /// and scoped the same way: it exists for one caller, the broker's parent
    /// half, and it lends to a process this shell is about to spawn under this
    /// shell's own policy. It is not a way for sandboxed code to obtain a
    /// capability -- sandboxed code cannot spawn anything under a closed world,
    /// which is what makes that distinction enforceable rather than merely
    /// stated.
    ///
    /// The loan borrows; nothing is duplicated or closed here. What the caller
    /// does with the raw handle -- `SCM_RIGHTS` on Unix, `DuplicateHandle` into
    /// the child on Windows -- is the platform's business, not the namespace's.
    pub fn lend_to_child(&self) -> impl Iterator<Item = MountLoan<'_>> {
        self.mounts.iter().map(|m| MountLoan { mount: m })
    }

    /// Rebuilds a table from handles received over the broker (D24).
    ///
    /// The child half of [`lend_to_child`](Self::lend_to_child). Each entry is
    /// a mount point, what it permits, and an owned directory handle; there are
    /// no host paths, which is why [`Mount::host_path`] answers `None` for
    /// everything built this way.
    ///
    /// # Errors
    ///
    /// Returns [`MountError::Path`] if a mount point is not a valid virtual
    /// path, and the same duplicate/collision errors
    /// [`MountTableBuilder::build`] returns. It cannot check for overlapping
    /// host directories, because it has no host paths to compare -- that
    /// validation happened in the parent, when the table was first built.
    pub fn from_child_handles(
        entries: impl IntoIterator<Item = (String, Access, MountHandle)>,
    ) -> Result<Self, MountError> {
        let mut parsed = Vec::new();
        for (at, access, handle) in entries {
            parsed.push((VirtualPath::new(&at)?, access, handle));
        }
        check_mount_points(parsed.iter().map(|(at, _, _)| at))?;

        let mounts = parsed
            .into_iter()
            .map(|(at, access, handle)| Mount {
                at,
                dir: cap_std::fs::Dir::from_std_file(std::fs::File::from(handle)),
                host_path: None,
                canonical_host_path: None,
                access,
                shadows_nested: false,
            })
            .collect();
        Ok(Self::from_mounts(mounts))
    }
}

/// One mount's capability, borrowed for handing to a child process (D24).
///
/// Deliberately not `Clone` and deliberately borrowing: a loan that outlived
/// the table it came from would be a capability with no owner, which is the
/// shape D3 exists to prevent.
pub struct MountLoan<'a> {
    mount: &'a Mount,
}

impl MountLoan<'_> {
    /// Where this mount appears in the virtual namespace.
    #[must_use]
    pub const fn mount_point(&self) -> &VirtualPath {
        &self.mount.at
    }

    /// What this mount permits.
    #[must_use]
    pub const fn access(&self) -> Access {
        self.mount.access
    }

    /// The borrowed directory handle, for the platform call that sends it.
    #[cfg(unix)]
    #[must_use]
    pub fn as_raw_fd(&self) -> std::os::fd::RawFd {
        use std::os::fd::AsRawFd as _;
        self.mount.dir.as_raw_fd()
    }

    /// The borrowed directory handle, for the platform call that sends it.
    #[cfg(windows)]
    #[must_use]
    pub fn as_raw_handle(&self) -> std::os::windows::io::RawHandle {
        use std::os::windows::io::AsRawHandle as _;
        self.mount.dir.as_raw_handle()
    }
}

/// An owned directory handle received from a parent process (D24).
#[cfg(unix)]
pub type MountHandle = std::os::fd::OwnedFd;

/// An owned directory handle received from a parent process (D24).
#[cfg(windows)]
pub type MountHandle = std::os::windows::io::OwnedHandle;

/// Rejects duplicated and case-folding-collision mount points.
///
/// Shared by both ways of building a table so that a namespace assembled from
/// handles is held to the same grammar as one assembled from directories.
fn check_mount_points<'a>(points: impl Iterator<Item = &'a VirtualPath>) -> Result<(), MountError> {
    let mut seen: HashMap<String, String> = HashMap::new();
    for at in points {
        let key = at
            .components()
            .map(fold_for_collision)
            .collect::<Vec<_>>()
            .join("/");

        if let Some(first) = seen.get(&key) {
            return if first == at.as_str() {
                Err(MountError::DuplicateMountPoint(at.as_str().to_owned()))
            } else {
                Err(MountError::CollidingMountPoints {
                    first: first.clone(),
                    second: at.as_str().to_owned(),
                })
            };
        }
        seen.insert(key, at.as_str().to_owned());
    }
    Ok(())
}

/// Accumulates and validates mounts.
#[derive(Debug, Default)]
pub struct MountTableBuilder {
    entries: Vec<(VirtualPath, PathBuf, Access)>,
}

impl MountTableBuilder {
    /// Adds a mount, deferring validation to [`MountTableBuilder::build`].
    ///
    /// # Errors
    ///
    /// Returns [`MountError::Path`] if the mount point is not a valid virtual
    /// path.
    pub fn mount(
        mut self,
        at: &str,
        host_dir: impl Into<PathBuf>,
        access: Access,
    ) -> Result<Self, MountError> {
        self.entries
            .push((VirtualPath::new(at)?, host_dir.into(), access));
        Ok(self)
    }

    /// Validates the accumulated mounts and opens their host directories.
    ///
    /// This is the last point at which host paths are consulted by name. After
    /// it, every operation goes through a directory capability instead.
    ///
    /// # Errors
    ///
    /// Returns [`MountError`] if a mount point is duplicated, if two mount
    /// points collide when case-folded, if two host directories overlap, or if
    /// a host directory cannot be opened.
    pub fn build(self) -> Result<MountTable, MountError> {
        check_mount_points(self.entries.iter().map(|(at, _, _)| at))?;

        // Canonicalize before comparing: two host paths can name the same
        // directory through different symlinks, and an overlap that is invisible
        // textually is still an overlap on disk.
        let canonical: Vec<PathBuf> = self
            .entries
            .iter()
            .map(|(_, host, _)| {
                // The one place a host path is legitimately resolved against
                // the host: mount construction is where the namespace is
                // built, before there is a namespace to ask.
                #[expect(
                    clippy::disallowed_methods,
                    reason = "building the namespace out of host directories is this crate's job"
                )]
                host.canonicalize().map_err(|source| MountError::HostDir {
                    path: host.clone(),
                    source,
                })
            })
            .collect::<Result<_, _>>()?;

        for (i, outer) in canonical.iter().enumerate() {
            for (j, inner) in canonical.iter().enumerate() {
                if i != j && inner.starts_with(outer) {
                    return Err(MountError::OverlappingHostDirs {
                        inner: inner.clone(),
                        outer: outer.clone(),
                    });
                }
            }
        }

        let mut mounts = Vec::with_capacity(self.entries.len());
        for ((at, host_path, access), canonical) in self.entries.into_iter().zip(canonical) {
            let dir = cap_std::fs::Dir::open_ambient_dir(&canonical, cap_std::ambient_authority())
                .map_err(|source| MountError::HostDir {
                    path: canonical.clone(),
                    source,
                })?;
            mounts.push(Mount {
                at,
                dir,
                host_path: Some(host_path),
                canonical_host_path: Some(canonical),
                access,
                shadows_nested: false,
            });
        }

        Ok(MountTable::from_mounts(mounts))
    }
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "test fixtures are built on the host, which is the one place that is the point"
)]
mod tests {
    use super::*;

    /// Two sibling directories under one temp root, for tests that need
    /// genuinely disjoint host paths.
    fn scratch() -> tempfile::TempDir {
        let d = tempfile::tempdir().expect("temp dir");
        std::fs::create_dir(d.path().join("a")).expect("mkdir a");
        std::fs::create_dir(d.path().join("b")).expect("mkdir b");
        std::fs::create_dir(d.path().join("a").join("nested")).expect("mkdir a/nested");
        d
    }

    /// Duplicates each lent handle, as `SCM_RIGHTS` and `DuplicateHandle` both
    /// do, so the rebuilt table owns handles the original still holds.
    fn relend(table: &MountTable) -> Vec<(String, Access, MountHandle)> {
        table
            .lend_to_child()
            .map(|loan| {
                #[cfg(unix)]
                let handle = {
                    use std::os::fd::{BorrowedFd, OwnedFd};
                    // SAFETY: the loan borrows a live `Dir` owned by `table`,
                    // which outlives this borrow, and `try_clone_to_owned`
                    // duplicates rather than taking ownership.
                    let borrowed = unsafe { BorrowedFd::borrow_raw(loan.as_raw_fd()) };
                    let owned: OwnedFd = borrowed.try_clone_to_owned().expect("dup");
                    owned
                };
                #[cfg(windows)]
                let handle = {
                    use std::os::windows::io::{BorrowedHandle, OwnedHandle};
                    // SAFETY: as above; the loan borrows a live `Dir`.
                    let borrowed = unsafe { BorrowedHandle::borrow_raw(loan.as_raw_handle()) };
                    let owned: OwnedHandle = borrowed.try_clone_to_owned().expect("dup");
                    owned
                };
                (
                    loan.mount_point().as_str().to_owned(),
                    loan.access(),
                    handle,
                )
            })
            .collect()
    }

    #[test]
    fn a_table_survives_being_lent_and_rebuilt() {
        // Step 0 of the broker (D24), proven without any IPC: what crosses is a
        // mount point, an access mode and a handle, and a table rebuilt from
        // those three resolves the same way the original does.
        let s = scratch();
        let table = MountTable::builder()
            .mount("/work", s.path().join("a"), Access::ReadWrite)
            .unwrap()
            .mount("/work/vendor", s.path().join("b"), Access::ReadOnly)
            .unwrap()
            .build()
            .expect("disjoint mounts build");

        let rebuilt = MountTable::from_child_handles(relend(&table)).expect("rebuilds");

        let p = VirtualPath::new("/work/vendor/lib.rs").unwrap();
        let (mount, rest) = rebuilt.resolve(&p).expect("resolves");
        assert_eq!(mount.mount_point().as_str(), "/work/vendor");
        assert_eq!(rest, vec!["lib.rs"]);
        assert_eq!(
            mount.access(),
            Access::ReadOnly,
            "access must cross, or a read-only mount silently becomes writable"
        );

        // The ordering invariant `resolve` depends on is re-established rather
        // than inherited from the order the entries happened to arrive in.
        let outer = rebuilt
            .mounts()
            .find(|m| m.mount_point().as_str() == "/work")
            .expect("/work is mounted");
        assert!(
            outer.shadows_a_nested_mount(),
            "nesting must be recomputed on the rebuilt table"
        );
    }

    #[test]
    fn a_rebuilt_mount_has_no_host_path() {
        // D3 expressed in the type: a child is handed capabilities, so it has
        // no host path to leak even by accident.
        let s = scratch();
        let table = MountTable::builder()
            .mount("/work", s.path().join("a"), Access::ReadWrite)
            .unwrap()
            .build()
            .expect("builds");
        assert!(
            table
                .mounts()
                .next()
                .expect("one mount")
                .host_path()
                .is_some(),
            "the control: a mount built from a directory knows its path"
        );

        let rebuilt = MountTable::from_child_handles(relend(&table)).expect("rebuilds");
        assert!(
            rebuilt
                .mounts()
                .next()
                .expect("one mount")
                .host_path()
                .is_none()
        );
    }

    #[test]
    fn rebuilding_rejects_a_duplicated_mount_point() {
        // The grammar checks apply to a namespace assembled from handles too.
        // Host-directory overlap cannot be checked here -- there are no paths
        // -- so the checks that *can* run have to actually run.
        let s = scratch();
        let table = MountTable::builder()
            .mount("/work", s.path().join("a"), Access::ReadWrite)
            .unwrap()
            .mount("/other", s.path().join("b"), Access::ReadOnly)
            .unwrap()
            .build()
            .expect("builds");

        let mut entries = relend(&table);
        entries[1].0 = "/work".to_owned();
        assert!(matches!(
            MountTable::from_child_handles(entries),
            Err(MountError::DuplicateMountPoint(_))
        ));
    }

    #[test]
    fn resolves_to_the_longest_matching_mount() {
        let s = scratch();
        let table = MountTable::builder()
            .mount("/work", s.path().join("a"), Access::ReadWrite)
            .unwrap()
            .mount("/work/vendor", s.path().join("b"), Access::ReadOnly)
            .unwrap()
            .build()
            .expect("disjoint mounts build");

        let p = VirtualPath::new("/work/vendor/lib.rs").unwrap();
        let (mount, rest) = table.resolve(&p).expect("resolves");
        assert_eq!(mount.mount_point().as_str(), "/work/vendor");
        assert_eq!(rest, vec!["lib.rs"]);
        assert_eq!(mount.access(), Access::ReadOnly);

        let p = VirtualPath::new("/work/src/lib.rs").unwrap();
        let (mount, rest) = table.resolve(&p).expect("resolves");
        assert_eq!(mount.mount_point().as_str(), "/work");
        assert_eq!(rest, vec!["src", "lib.rs"]);
    }

    #[test]
    fn unmounted_paths_do_not_resolve() {
        let s = scratch();
        let table = MountTable::builder()
            .mount("/work", s.path().join("a"), Access::ReadWrite)
            .unwrap()
            .build()
            .unwrap();

        assert!(
            table
                .resolve(&VirtualPath::new("/etc/passwd").unwrap())
                .is_none()
        );
        assert!(table.resolve(&VirtualPath::root()).is_none());
    }

    #[test]
    fn overlapping_host_dirs_are_refused() {
        // The hard-link-through-a-ro-mount hazard: two mounts over one inode
        // tree mean the access mode depends on which name was used.
        let s = scratch();
        let err = MountTable::builder()
            .mount("/outer", s.path().join("a"), Access::ReadOnly)
            .unwrap()
            .mount(
                "/inner",
                s.path().join("a").join("nested"),
                Access::ReadWrite,
            )
            .unwrap()
            .build()
            .expect_err("nesting must be refused");
        assert!(matches!(err, MountError::OverlappingHostDirs { .. }));
    }

    #[test]
    fn identical_host_dirs_are_refused() {
        let s = scratch();
        let err = MountTable::builder()
            .mount("/one", s.path().join("a"), Access::ReadOnly)
            .unwrap()
            .mount("/two", s.path().join("a"), Access::ReadWrite)
            .unwrap()
            .build()
            .expect_err("aliasing must be refused");
        assert!(matches!(err, MountError::OverlappingHostDirs { .. }));
    }

    #[test]
    fn case_folded_mount_points_are_refused() {
        let s = scratch();
        let err = MountTable::builder()
            .mount("/Work", s.path().join("a"), Access::ReadWrite)
            .unwrap()
            .mount("/work", s.path().join("b"), Access::ReadWrite)
            .unwrap()
            .build()
            .expect_err("case collision must be refused");
        assert!(matches!(err, MountError::CollidingMountPoints { .. }));
    }

    #[test]
    fn duplicate_mount_points_are_refused() {
        let s = scratch();
        let err = MountTable::builder()
            .mount("/work", s.path().join("a"), Access::ReadWrite)
            .unwrap()
            .mount("/work", s.path().join("b"), Access::ReadWrite)
            .unwrap()
            .build()
            .expect_err("duplicate must be refused");
        assert!(matches!(err, MountError::DuplicateMountPoint(_)));
    }

    #[test]
    fn a_missing_host_dir_is_reported_with_its_path() {
        let s = scratch();
        let err = MountTable::builder()
            .mount("/work", s.path().join("does-not-exist"), Access::ReadWrite)
            .unwrap()
            .build()
            .expect_err("missing dir must be refused");
        assert!(matches!(err, MountError::HostDir { .. }));
    }

    #[test]
    fn an_invalid_mount_point_is_refused_at_add_time() {
        let err = MountTable::builder()
            .mount("/C:/work", "/tmp", Access::ReadWrite)
            .expect_err("drive letter must be refused");
        assert!(matches!(err, MountError::Path(_)));
    }
}
