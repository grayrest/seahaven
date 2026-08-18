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
    host_path: PathBuf,
    access: Access,
}

impl Mount {
    /// Where this mount appears in the virtual namespace.
    #[must_use]
    pub const fn mount_point(&self) -> &VirtualPath {
        &self.at
    }

    /// The capability handle for the mount's root directory.
    #[must_use]
    pub const fn dir(&self) -> &cap_std::fs::Dir {
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
    #[must_use]
    pub fn host_path(&self) -> &Path {
        &self.host_path
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
    pub fn resolve<'a>(&'a self, path: &'a VirtualPath) -> Option<(&'a Mount, Vec<&'a str>)> {
        self.mounts
            .iter()
            .find_map(|m| path.strip_prefix(&m.at).map(|rest| (m, rest)))
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
        let mut seen: HashMap<String, String> = HashMap::new();
        for (at, _, _) in &self.entries {
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

        // Canonicalize before comparing: two host paths can name the same
        // directory through different symlinks, and an overlap that is invisible
        // textually is still an overlap on disk.
        let canonical: Vec<PathBuf> = self
            .entries
            .iter()
            .map(|(_, host, _)| {
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
                host_path,
                access,
            });
        }

        // Longest mount point first, so `resolve` can take the first match.
        mounts.sort_by_key(|m| std::cmp::Reverse(m.at.components().count()));

        Ok(MountTable { mounts })
    }
}

#[cfg(test)]
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
