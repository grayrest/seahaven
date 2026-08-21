//! Turning a discovered project root into a namespace (D16, D31, D44).
//!
//! [`discovery`](crate::discovery) answers *where* the project is and what
//! bounds it. This turns that answer into the mount table a shell is confined
//! to — which is the launcher's whole job, and the reason a repo-local manifest
//! can never widen anything: it is read inside the boundary this builds.
//!
//! # What a project gets
//!
//! Its own tree read-write, its source-control marker read-only, and a
//! persistent home of its own. Nothing else. That is D16's default ceiling —
//! "the justfile's own directory tree rw and nothing else" — with D44's carve-
//! out and D31's home.
//!
//! **This is not D16's grant derivation.** That resolves a justfile's
//! `import`/`mod` graph up front and unions the trees it reaches, which needs a
//! parser this repository does not have. What is here is the default a project
//! gets before any manifest is consulted.

use std::path::{Path, PathBuf};

use brush_vfs::{Access, MountError, MountTable};

use crate::discovery::Ceiling;

/// Where the project's own tree appears.
pub const PROJECT_MOUNT: &str = "/work";

/// Where the project's persistent home appears (D31).
pub const HOME_MOUNT: &str = "/home/user";

/// Why a grant could not be derived.
#[derive(Debug, thiserror::Error)]
pub enum GrantError {
    /// The platform has no state directory to keep per-project homes in.
    #[error("no state directory on this platform, so there is nowhere to keep a project's home")]
    NoStateDir,
    /// A directory the grant needs could not be created.
    #[error("cannot create {path}: {source}")]
    Host {
        /// The directory that could not be created.
        path: PathBuf,
        /// What the host said.
        source: std::io::Error,
    },
    /// The marker does not lie inside the project tree, so there is no mount
    /// point to shadow it at.
    ///
    /// Reachable in a worktree or submodule, where `.git` is a file pointing at
    /// a git directory elsewhere -- and where D44 notes the real git directory
    /// already lies outside the ceiling, so there is nothing to carve out.
    #[error("{marker} is not inside {root}, so there is nothing to shadow")]
    MarkerOutsideRoot {
        /// The marker that could not be placed.
        marker: PathBuf,
        /// The project tree it was expected to be inside.
        root: PathBuf,
    },
    /// The mounts do not form a namespace.
    #[error("cannot build the namespace: {0}")]
    Mounts(#[from] MountError),
}

/// The host directories a project is granted, before they become mounts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    /// The project tree, read-write.
    pub root: PathBuf,
    /// The source-control marker, read-only. See [`Grant::derive`].
    pub marker: PathBuf,
    /// This project's persistent home, read-write (D31).
    pub home: PathBuf,
    /// An empty read-only directory, mounted over the marker so its contents
    /// have no name in the namespace. See [`Grant::derive`].
    pub shadow: PathBuf,
}

impl Grant {
    /// Derives the default grant for a project root.
    ///
    /// # The marker is shadowed, not mounted read-only
    ///
    /// The obvious move — mount the marker read-only inside the read-write
    /// project — **does not build**, and the refusal is right.
    /// `MountTable::build` rejects overlapping *host* directories because "the
    /// two mounts are the same inode tree under different names, so the access
    /// mode is decided by which name the caller happened to use": with both
    /// mounted, `ln /work/.git/config /work/x` gives a read-write name to a
    /// read-only inode. D16 names this same hazard when it discusses per-path
    /// access rules.
    ///
    /// So the marker is **shadowed** by an empty read-only directory instead,
    /// which is ordinary mount semantics rather than a special case — the same
    /// thing a Unix mount over a non-empty directory does. The host directories
    /// are disjoint, so the table builds and the hard-link hazard does not
    /// arise; `/work/.git` resolves to the empty shadow by longest match, and
    /// the real marker has no name in the namespace at all.
    ///
    /// That satisfies D44's actual words — "the grant must not contain the
    /// marker's own directory" — rather than only the writability half of its
    /// reasoning.
    ///
    /// # Errors
    ///
    /// Returns [`GrantError`] if the platform has no state directory or the
    /// project's home cannot be created.
    pub fn derive(root: &Path, ceiling: &Ceiling) -> Result<Self, GrantError> {
        Ok(Self {
            root: root.to_path_buf(),
            marker: ceiling.marker.clone(),
            home: project_home(root)?,
            shadow: shadow_dir()?,
        })
    }

    /// Builds the namespace this grant describes.
    ///
    /// # Errors
    ///
    /// Returns [`GrantError::MarkerOutsideRoot`] if the marker is not inside
    /// the project tree, or [`GrantError::Mounts`] if the mounts do not form a
    /// namespace.
    pub fn mount_table(&self) -> Result<MountTable, GrantError> {
        let Some(marker_at) = self.marker_mount_point() else {
            return Err(GrantError::MarkerOutsideRoot {
                marker: self.marker.clone(),
                root: self.root.clone(),
            });
        };

        Ok(MountTable::builder()
            .mount(PROJECT_MOUNT, &self.root, Access::ReadWrite)?
            .mount(&marker_at, &self.shadow, Access::ReadOnly)?
            .mount(HOME_MOUNT, &self.home, Access::ReadWrite)?
            .build()?)
    }

    /// Where the marker appears in the namespace, given where it is on the host.
    fn marker_mount_point(&self) -> Option<String> {
        let rest = self.marker.strip_prefix(&self.root).ok()?;
        let mut at = String::from(PROJECT_MOUNT);
        for component in rest.components() {
            at.push('/');
            at.push_str(component.as_os_str().to_str()?);
        }
        (at != PROJECT_MOUNT).then_some(at)
    }
}

/// This project's persistent home directory on the host (D31).
///
/// Under the user's state directory, so caches survive between runs, and keyed
/// so one repository's home is invisible to another. XDG paths derive as
/// subdirectories of it.
fn project_home(root: &Path) -> Result<PathBuf, GrantError> {
    let home = launcher_state()?.join("homes").join(home_key(root));

    // The launcher's own directory, created before there is a namespace that
    // could contain it -- the same exemption discovery and config loading carry.
    #[expect(
        clippy::disallowed_methods,
        reason = "the launcher creates the project's home before any namespace exists"
    )]
    std::fs::create_dir_all(&home).map_err(|source| GrantError::Host {
        path: home.clone(),
        source,
    })?;
    Ok(home)
}

/// The empty directory mounted over a project's source-control marker.
///
/// One per user rather than one per project: it is empty and read-only, so
/// there is nothing for two projects to share.
fn shadow_dir() -> Result<PathBuf, GrantError> {
    let dir = launcher_state()?.join("empty");
    #[expect(
        clippy::disallowed_methods,
        reason = "the launcher creates its own directories before any namespace exists"
    )]
    std::fs::create_dir_all(&dir).map_err(|source| GrantError::Host {
        path: dir.clone(),
        source,
    })?;
    Ok(dir)
}

/// The launcher's own state directory.
fn launcher_state() -> Result<PathBuf, GrantError> {
    let strategy = etcetera::choose_base_strategy().map_err(|_| GrantError::NoStateDir)?;
    let base = etcetera::BaseStrategy::state_dir(&strategy)
        .unwrap_or_else(|| etcetera::BaseStrategy::data_dir(&strategy));
    Ok(base.join("brush"))
}

/// A stable, readable directory name for a project's home.
///
/// The project's own directory name, so the state directory can be read by a
/// human, plus a hash of the full path, so two checkouts of the same repository
/// do not share a home. Hashed with FNV-1a rather than the standard library's
/// hasher because that one makes no stability guarantee across releases, and a
/// key that changes on a toolchain upgrade orphans every existing home.
fn home_key(root: &Path) -> String {
    let name = root
        .file_name()
        .and_then(|n| n.to_str())
        .filter(|n| {
            n.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        })
        .unwrap_or("project");

    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in root.as_os_str().as_encoded_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    format!("{name}-{hash:016x}")
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "test fixtures are built on the host, which is the one place that is the point"
)]
mod tests {
    use super::*;

    fn repo() -> (tempfile::TempDir, PathBuf, Ceiling) {
        let root = tempfile::tempdir().expect("temp dir");
        let project = root.path().join("proj");
        std::fs::create_dir(&project).expect("mkdir");
        std::fs::create_dir(project.join(".git")).expect("mkdir .git");
        std::fs::write(project.join("f.txt"), b"x").expect("write");
        let ceiling = Ceiling {
            dir: project.clone(),
            marker: project.join(".git"),
        };
        (root, project, ceiling)
    }

    #[test]
    fn the_project_tree_is_writable_and_the_marker_is_not() {
        let (_root, project, ceiling) = repo();
        let grant = Grant::derive(&project, &ceiling).expect("derives");
        let table = grant.mount_table().expect("builds");

        let at = |p: &str| {
            let vp = brush_vfs::VirtualPath::new(p).expect("valid");
            table
                .resolve(&vp)
                .map(|(m, _)| m.access())
                .expect("resolves")
        };
        assert_eq!(at("/work/f.txt"), Access::ReadWrite);
        assert_eq!(at("/home/user/cache"), Access::ReadWrite);

        // The marker's mount point exists and is read-only, but it resolves to
        // the empty shadow rather than to the real `.git`.
        assert_eq!(at("/work/.git"), Access::ReadOnly);

        let vfs = brush_vfs::Vfs::new(grant.mount_table().expect("builds"));
        std::fs::write(project.join(".git").join("config"), b"[core]\n").expect("write");
        assert!(
            !vfs.exists(&brush_vfs::VirtualPath::new("/work/.git/config").expect("valid")),
            "git runs what it finds in .git at the user's next command, so the \
             grant must not contain it"
        );
    }

    #[test]
    fn nothing_above_the_project_is_reachable() {
        // The default ceiling is the project tree and nothing else, so the
        // directory containing it has no name at all.
        let (root, project, ceiling) = repo();
        std::fs::write(root.path().join("secret.txt"), b"s").expect("write");
        let grant = Grant::derive(&project, &ceiling).expect("derives");
        let table = grant.mount_table().expect("builds");

        let vfs = brush_vfs::Vfs::new(table);
        assert!(vfs.exists(&brush_vfs::VirtualPath::new("/work/f.txt").expect("valid")));
        assert!(
            !vfs.exists(&brush_vfs::VirtualPath::new("/work/../secret.txt").expect("valid")),
            "the project's parent must not be nameable"
        );
    }

    #[test]
    fn two_checkouts_of_one_repository_do_not_share_a_home() {
        // Keyed by the full path, so a second clone gets its own caches rather
        // than inheriting the first's.
        let a = home_key(Path::new("/a/proj"));
        let b = home_key(Path::new("/b/proj"));
        assert_ne!(a, b);
        assert!(a.starts_with("proj-"), "the key stays readable: {a}");
    }

    #[test]
    fn the_home_key_is_stable() {
        // Hard-coded rather than compared to a fresh computation: the point is
        // that the value does not move between builds, and a self-comparison
        // would pass however much it moved.
        assert_eq!(home_key(Path::new("/a/proj")), "proj-e6a8e6d8398d46e5");
    }

    #[test]
    fn a_project_name_that_is_not_a_safe_directory_name_is_replaced() {
        let key = home_key(Path::new("/a/pro ject!"));
        assert!(key.starts_with("project-"), "{key}");
    }
}
