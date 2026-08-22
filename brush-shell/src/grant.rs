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

/// Where consent is recorded (D29).
#[must_use]
pub fn trust_store_path() -> Option<PathBuf> {
    launcher_state().ok().map(|d| d.join("trust.toml"))
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
    use std::io::ErrorKind::{self, CrossesDevices, PermissionDenied};

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

    #[test]
    fn no_write_route_reaches_the_marker() {
        // Gate 6, as worded. The case above asserts the marker's *contents* are
        // unreachable, which is the stronger property for reading and says
        // nothing at all about writing -- and writing is what D44's reasoning
        // is about. Git runs what it finds in `.git` at the user's next
        // command, so a grant that cannot read `config` but can drop a
        // `hooks/pre-commit` has closed nothing.
        //
        // Every route is enumerated rather than sampled, because they are
        // separate calls in the facade and a read-only mount is enforced per
        // call. `hard_link` and `copy` are the two the shadow exists for:
        // `Grant::derive` explains that a read-only mount *of the real marker*
        // would let a hard link give a read-write name to a read-only inode,
        // and the shadow is what makes that unreachable rather than merely
        // refused.
        let (_root, project, ceiling) = repo();
        std::fs::write(project.join(".git").join("config"), b"[core]\n").expect("write");
        std::fs::write(project.join("payload"), b"p").expect("write");

        let grant = Grant::derive(&project, &ceiling).expect("derives");
        let vfs = brush_vfs::Vfs::new(grant.mount_table().expect("builds"));
        let vp = |p: &str| brush_vfs::VirtualPath::new(p).expect("valid");

        // The indirect route, closed one step earlier than expected: a symlink
        // inside the writable tree pointing *at* the mount point is refused at
        // creation, because the facade will not make a link that crosses a
        // mount boundary. So there is no name to write through, rather than a
        // name that is written through and refused.
        let err = vfs
            .symlink(&vp("/work/link"), ".git")
            .expect_err("a link across a mount boundary must be refused");
        assert_eq!(err.kind(), ErrorKind::InvalidInput, "{err}");

        let routes: Vec<(&str, ErrorKind, std::io::Result<()>)> = vec![
            (
                "create a file",
                PermissionDenied,
                vfs.create(&vp("/work/.git/hooks")).map(drop),
            ),
            (
                "create a directory",
                PermissionDenied,
                vfs.create_dir(&vp("/work/.git/x")),
            ),
            (
                "create a directory tree",
                PermissionDenied,
                vfs.create_dir_all(&vp("/work/.git/hooks/x")),
            ),
            (
                "symlink into it",
                PermissionDenied,
                vfs.symlink(&vp("/work/.git/payload"), "../payload"),
            ),
            (
                "copy into it",
                PermissionDenied,
                vfs.copy(&vp("/work/payload"), &vp("/work/.git/payload"))
                    .map(drop),
            ),
            (
                "overwrite what is there",
                PermissionDenied,
                vfs.create(&vp("/work/.git/config")).map(drop),
            ),
            (
                "remove what is there",
                PermissionDenied,
                vfs.remove_file(&vp("/work/.git/config")),
            ),
            // These two are refused a step earlier, by the mount boundary
            // rather than by the access mode -- which is the stronger of the
            // two refusals: it does not depend on the shadow having been
            // mounted read-only, only on it being a mount at all.
            (
                "rename into it",
                CrossesDevices,
                vfs.rename(&vp("/work/payload"), &vp("/work/.git/payload")),
            ),
            (
                "hard-link into it",
                CrossesDevices,
                vfs.hard_link(&vp("/work/payload"), &vp("/work/.git/payload")),
            ),
        ];
        for (what, expected, result) in routes {
            let err = result.expect_err(&format!("`{what}` must be refused"));
            assert_eq!(
                err.kind(),
                expected,
                "`{what}` was refused for the wrong reason: {err}"
            );
        }

        // The half an error-only assertion cannot make. Every call above
        // failing is consistent with one of them having succeeded somewhere
        // else first, and there are two somewhere-elses that matter: the host's
        // real marker, and the shadow -- which is one empty directory shared by
        // every project on the machine, so a write landing there is a channel
        // between projects rather than only an escape from one.
        let entries = |dir: &Path| {
            let mut names: Vec<String> = std::fs::read_dir(dir)
                .expect("read_dir")
                .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
                .collect();
            names.sort();
            names
        };
        assert_eq!(
            entries(&project.join(".git")),
            ["config"],
            "a write reached the host's real marker"
        );
        assert!(
            entries(&grant.shadow).is_empty(),
            "a write landed in the shadow, which every project on this machine shares"
        );
    }

    #[test]
    fn a_derived_grant_is_the_namespace_the_fixture_asks_for() {
        // Gate 8. The cases above assert that a grant *resolves* the way it
        // should, which a grant derived from the wrong directory passes just as
        // well so long as it is internally consistent. This runs the launcher's
        // whole pipeline -- discovery upward from a nested directory, then
        // derivation -- and compares the mounts to an expectation built here
        // from the fixture's own layout.
        //
        // The expectation duplicates `home_key`'s hash and `launcher_state`'s
        // path on purpose. Composing the helpers under test would make this
        // assert only that the code agrees with itself, which is the failure
        // mode the gate is named after.
        let (root, project, nested) = nested_repo();
        let ceiling = crate::discovery::Bound::platform_default()
            .ceiling(&nested)
            .expect("the fixture is a project below every stop");

        assert_eq!(
            ceiling.dir, project,
            "discovery must answer with the repository, not the directory it started in"
        );
        assert_eq!(ceiling.marker, project.join(".git"));

        let grant = Grant::derive(&ceiling.dir, &ceiling).expect("derives");
        let table = grant.mount_table().expect("builds");

        let mut observed: Vec<(String, Option<PathBuf>, Access)> = table
            .mounts()
            .map(|m| {
                (
                    m.mount_point().as_str().to_owned(),
                    m.host_path().map(Path::to_path_buf),
                    m.access(),
                )
            })
            .collect();
        observed.sort_by(|a, b| a.0.cmp(&b.0));

        let state = expected_state_dir();
        let expected = vec![
            (
                "/home/user".to_owned(),
                Some(state.join("homes").join(expected_home_key(&project))),
                Access::ReadWrite,
            ),
            ("/work".to_owned(), Some(project.clone()), Access::ReadWrite),
            (
                "/work/.git".to_owned(),
                Some(state.join("empty")),
                Access::ReadOnly,
            ),
        ];
        assert_eq!(observed, expected);

        // And it is not a constant. A pipeline that ignored its input would
        // satisfy every assertion above.
        let (_other_root, other_project, other_nested) = nested_repo();
        assert_ne!(other_project, project, "the fixtures must differ");
        let other_ceiling = crate::discovery::Bound::platform_default()
            .ceiling(&other_nested)
            .expect("the second fixture is a project too");
        let other = Grant::derive(&other_ceiling.dir, &other_ceiling).expect("derives");
        assert_ne!(other.root, grant.root);
        assert_ne!(
            other.home, grant.home,
            "two projects must not share a home (D31)"
        );
        assert_eq!(
            other.shadow, grant.shadow,
            "the shadow is empty and read-only, so one per user is the design"
        );
        drop(root);
    }

    /// A repository with a subdirectory to start a search from, canonicalized
    /// so it can be compared to what discovery returns.
    fn nested_repo() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let root = tempfile::tempdir().expect("temp dir");
        let project = root.path().join("proj");
        std::fs::create_dir(&project).expect("mkdir");
        std::fs::create_dir(project.join(".git")).expect("mkdir .git");
        let nested = project.join("src").join("deep");
        std::fs::create_dir_all(&nested).expect("mkdir -p");
        let project = project.canonicalize().expect("canonicalize");
        let nested = nested.canonicalize().expect("canonicalize");
        (root, project, nested)
    }

    /// `launcher_state`, rewritten rather than called. See the gate 8 case.
    fn expected_state_dir() -> PathBuf {
        let strategy = etcetera::choose_base_strategy().expect("a base strategy");
        etcetera::BaseStrategy::state_dir(&strategy)
            .unwrap_or_else(|| etcetera::BaseStrategy::data_dir(&strategy))
            .join("brush")
    }

    /// `home_key`, rewritten rather than called. See the gate 8 case.
    fn expected_home_key(root: &Path) -> String {
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for byte in root.as_os_str().as_encoded_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x100_0000_01b3);
        }
        format!("proj-{hash:016x}")
    }
}
