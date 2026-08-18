//! A session: one namespace, one working directory.
//!
//! Everything sandboxed code does happens in a session. The working directory
//! lives here rather than in the process because a process has exactly one and
//! a shell needs one per concurrent job — a recipe with its own working
//! directory cannot be expressed by `chdir`, and two running at once cannot be
//! expressed by it at all.
//!
//! The process's real working directory is never consulted and never changed.

use std::sync::Arc;

use crate::fs::Vfs;
use crate::mount::{Access, MountError, MountTable};
use crate::path::{PathError, VirtualPath};

/// One sandboxed execution context.
///
/// Cloning a session shares its namespace and copies its working directory,
/// which is what a subshell wants: the same filesystem, its own `cd`.
#[derive(Debug, Clone)]
pub struct Session {
    vfs: Arc<Vfs>,
    cwd: VirtualPath,
}

impl Default for Session {
    /// A session with no mounts, in which nothing is reachable.
    ///
    /// Failing closed matters because this is what a `Shell` built without an
    /// explicit policy gets: an empty namespace denies everything, where a
    /// default of "the host" would silently grant everything.
    fn default() -> Self {
        Self::new(Arc::new(Vfs::new(MountTable::default())))
    }
}

impl Session {
    /// Creates a session rooted at `/` in the given namespace.
    #[must_use]
    pub fn new(vfs: Arc<Vfs>) -> Self {
        Self {
            vfs,
            cwd: VirtualPath::root(),
        }
    }

    /// The namespace this session resolves against.
    #[must_use]
    pub fn vfs(&self) -> &Vfs {
        &self.vfs
    }

    /// The session's working directory.
    #[must_use]
    pub const fn cwd(&self) -> &VirtualPath {
        &self.cwd
    }

    /// Interprets `path` relative to the session's working directory.
    ///
    /// This is the single entry point by which a string written in a script
    /// becomes something the filesystem will act on, which is what makes the
    /// grammar unavoidable rather than merely available.
    ///
    /// # Errors
    ///
    /// Returns [`PathError`] if the grammar rejects the path.
    pub fn resolve(&self, path: &str) -> Result<VirtualPath, PathError> {
        self.cwd.resolve(path)
    }

    /// Changes the working directory.
    ///
    /// The target must exist and be a directory, matching what `cd` promises.
    ///
    /// # Errors
    ///
    /// Returns [`std::io::Error`] if the path is invalid, unmounted, missing, or
    /// not a directory.
    pub fn set_cwd(&mut self, path: &str) -> std::io::Result<()> {
        let target = self
            .resolve(path)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e.to_string()))?;

        // A probe rather than an open: a directory cannot be opened as a file,
        // and an unmounted path must report NotFound rather than surfacing
        // whatever the underlying open happened to fail with.
        match self.vfs.facts(&target, true) {
            Some(facts) if facts.is_dir => {}
            Some(_) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::NotADirectory,
                    format!("not a directory: {target}"),
                ));
            }
            None => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("no such file or directory: {target}"),
                ));
            }
        }

        self.cwd = target;
        Ok(())
    }
}

/// Builds the namespace policies a launcher can install.
pub struct Policy;

impl Policy {
    /// The identity policy: the host filesystem, mounted whole and writable.
    ///
    /// This exists so the compatibility suite can run the production binary
    /// rather than a build configured for testing. Under it every rejection
    /// branch in the vfs is unreachable — nothing is above the root and nothing
    /// is unmounted — which is precisely why it proves the absence of
    /// regressions and nothing about confinement.
    ///
    /// # Errors
    ///
    /// Returns [`MountError`] if the host root cannot be opened.
    pub fn identity() -> Result<MountTable, MountError> {
        MountTable::builder()
            .mount("/", host_root(), Access::ReadWrite)?
            .build()
    }

    /// A single writable directory, and nothing else.
    ///
    /// The default ceiling: a project gets its own tree and no more.
    ///
    /// # Errors
    ///
    /// Returns [`MountError`] if the directory cannot be opened.
    pub fn single_tree(
        host_dir: impl Into<std::path::PathBuf>,
        at: &str,
    ) -> Result<MountTable, MountError> {
        MountTable::builder()
            .mount(at, host_dir, Access::ReadWrite)?
            .build()
    }
}

/// The host's filesystem root.
///
/// On Windows there is no single root, so the identity policy covers the
/// current drive. A namespace spanning every drive would need one mount per
/// drive and a virtual layout to hang them from, which is a policy decision
/// rather than a default.
fn host_root() -> std::path::PathBuf {
    #[cfg(windows)]
    {
        use std::path::Component;
        if let Ok(cwd) = std::env::current_dir()
            && let Some(Component::Prefix(prefix)) = cwd.components().next()
        {
            let mut root = std::path::PathBuf::from(prefix.as_os_str());
            root.push(std::path::MAIN_SEPARATOR_STR);
            return root;
        }
        std::path::PathBuf::from("C:\\")
    }

    #[cfg(not(windows))]
    std::path::PathBuf::from("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session_over(dir: &std::path::Path) -> Session {
        let mounts = Policy::single_tree(dir, "/work").expect("mounts build");
        Session::new(Arc::new(Vfs::new(mounts)))
    }

    #[test]
    fn relative_paths_resolve_against_the_working_directory() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("src")).unwrap();
        std::fs::write(tmp.path().join("src").join("main.rs"), b"fn main() {}").unwrap();

        let mut s = session_over(tmp.path());
        assert_eq!(s.cwd().as_str(), "/");

        s.set_cwd("/work/src").unwrap();
        assert_eq!(s.cwd().as_str(), "/work/src");
        assert_eq!(s.resolve("main.rs").unwrap().as_str(), "/work/src/main.rs");
        assert_eq!(s.resolve("../src").unwrap().as_str(), "/work/src");
        assert_eq!(s.resolve("/work").unwrap().as_str(), "/work");
    }

    #[test]
    fn changing_to_a_missing_or_unmounted_directory_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let mut s = session_over(tmp.path());

        assert_eq!(
            s.set_cwd("/work/nope").unwrap_err().kind(),
            std::io::ErrorKind::NotFound
        );
        assert_eq!(
            s.set_cwd("/etc").unwrap_err().kind(),
            std::io::ErrorKind::NotFound
        );
        // And the working directory is unchanged after a failure.
        assert_eq!(s.cwd().as_str(), "/");
    }

    #[test]
    fn changing_to_a_file_fails() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("f.txt"), b"x").unwrap();
        let mut s = session_over(tmp.path());
        assert_eq!(
            s.set_cwd("/work/f.txt").unwrap_err().kind(),
            std::io::ErrorKind::NotADirectory
        );
    }

    #[test]
    fn escaping_the_root_is_refused_at_resolution() {
        let tmp = tempfile::tempdir().unwrap();
        let s = session_over(tmp.path());
        assert_eq!(s.resolve("/..").unwrap_err(), PathError::Escape);
    }

    #[test]
    fn clones_share_the_namespace_and_diverge_on_cwd() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("a")).unwrap();
        std::fs::create_dir(tmp.path().join("b")).unwrap();

        let mut parent = session_over(tmp.path());
        parent.set_cwd("/work/a").unwrap();

        let mut child = parent.clone();
        child.set_cwd("/work/b").unwrap();

        // A subshell's `cd` does not reach its parent.
        assert_eq!(parent.cwd().as_str(), "/work/a");
        assert_eq!(child.cwd().as_str(), "/work/b");
    }

    #[test]
    fn the_identity_policy_reaches_the_host() {
        // The point of identity: nothing is confined, so the compat suite sees
        // the same filesystem it always did.
        let mounts = Policy::identity().expect("host root opens");
        let vfs = Vfs::new(mounts);
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("probe"), b"visible").unwrap();

        let host = tmp.path().join("probe").canonicalize().unwrap();
        let as_virtual = host.to_str().unwrap().replace('\\', "/");
        let as_virtual = as_virtual.strip_prefix("C:").unwrap_or(&as_virtual);

        let p = VirtualPath::new(as_virtual).expect("host path is a valid virtual path");
        assert!(vfs.exists(&p), "identity policy should see {p}");
    }
}
