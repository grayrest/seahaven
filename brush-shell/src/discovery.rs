//! The bounded upward search a launcher performs before any policy (D44).
//!
//! Finding the file to run cannot happen inside the sandbox, because the grant
//! is derived from where that file *is* and finding it means probing ancestors.
//! So it happens here, before there is a namespace to ask — which is why this
//! module reaches the host directly and carries the ban exemptions to say so.
//!
//! # The bound is the point
//!
//! An unbounded upward search for a file that does not exist ends at the
//! filesystem root, and a root that happens to contain the sought file grants
//! everything below it. So the search stops, and separately, the *answer* is
//! checked.
//!
//! **Two searches, not one.** D44's correction: first-marker-wins deletes the
//! monorepo root, because a vendored subtree, a submodule or a linked worktree
//! carries `.git` and a walk from inside one stops there. So the file is
//! searched for independently of the ceiling, and the ceiling is the
//! *outermost* marker below the first stop.
//!
//! **And the bound is a predicate on the answer, not on the walk.** Three
//! routes reach a root without searching at all — a path named outright, a
//! working directory supplied after the fact, an initialisation that runs
//! before the search. All four are held to the same rule: the resulting
//! directory must lie strictly below a marker that is itself strictly below
//! every stop.

use std::path::{Path, PathBuf};

/// Source-control markers, as a file *or* a directory.
///
/// Both, because git writes `.git` as a *file* in worktrees and submodules, and
/// a directory-only check silently fails in both.
pub const DEFAULT_MARKERS: &[&str] = &[".git", ".hg", ".svn", ".jj"];

/// Why a directory cannot be the root of a grant.
#[derive(Debug, thiserror::Error)]
pub enum BoundError {
    /// The search reached a stop without finding a marker.
    #[error("no source-control root below {stop}: {start} is not inside a project")]
    NoMarker {
        /// Where the search began.
        start: PathBuf,
        /// The stop it reached.
        stop: PathBuf,
    },
    /// A marker was found, but at or above a stop, so it does not bound anything.
    #[error(
        "the only source-control root for {start} is {marker}, which is at or above {stop}; \
         name a ceiling explicitly"
    )]
    MarkerAtStop {
        /// Where the search began.
        start: PathBuf,
        /// The marker that cannot be used.
        marker: PathBuf,
        /// The stop it sits at or above.
        stop: PathBuf,
    },
    /// The directory lies outside the ceiling its own search produced.
    #[error("{root} is not inside {ceiling}")]
    OutsideCeiling {
        /// The directory being checked.
        root: PathBuf,
        /// The ceiling it must lie within.
        ceiling: PathBuf,
    },
    /// The host refused to canonicalize a path.
    #[error("cannot resolve {path}: {source}")]
    Host {
        /// The path that could not be resolved.
        path: PathBuf,
        /// What the host said.
        source: std::io::Error,
    },
}

/// The outermost source-control root below the first stop, and its marker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ceiling {
    /// The directory holding the marker. A grant may not reach above it.
    pub dir: PathBuf,
    /// The marker itself, which is carved *out* of any grant: `.git/config`
    /// carries `core.pager`, `core.fsmonitor` and `!sh -c` aliases, and git
    /// runs what it finds there at the user's next command.
    pub marker: PathBuf,
}

/// A configured bound: what counts as a marker, and where the walk stops.
#[derive(Debug, Clone)]
pub struct Bound {
    markers: Vec<String>,
    stops: Vec<PathBuf>,
}

impl Bound {
    /// The platform's stop set, plus the user's home.
    ///
    /// The set is **enumerated rather than described**, and D44 is emphatic
    /// about why: `/tmp` canonicalizes to `/private/tmp` on macOS and `$TMPDIR`
    /// to `/private/var/...`, so a list containing `/private` or `/var` kills
    /// discovery for every temp-directory checkout — including this project's
    /// own scratch worktrees.
    ///
    /// Home is resolved through the platform's own notion of it, never through
    /// `HOME`. On Windows `HOME` is normally unset — the shell synthesizes it
    /// from `USERPROFILE` *inside* the boundary — so a launcher reading `HOME`
    /// would find nothing, the stop would silently vanish, and a `git init`'d
    /// user profile would become a valid ceiling.
    #[must_use]
    pub fn platform_default() -> Self {
        let mut stops: Vec<PathBuf> = platform_stops()
            .iter()
            .map(PathBuf::from)
            .filter_map(|p| canonical_if_present(&p))
            .collect();

        if let Ok(home) = etcetera::home_dir()
            && let Some(home) = canonical_if_present(&home)
        {
            stops.push(home);
        }

        Self {
            markers: DEFAULT_MARKERS.iter().map(|m| (*m).to_owned()).collect(),
            stops,
        }
    }

    /// Replaces the marker set, for a project that is not a repository.
    ///
    /// D44 makes the strategy configurable because this platform is meant to
    /// carry more than one consumer. What is *not* configurable is that some
    /// bound applies, which is why there is no way to empty the stop list.
    #[must_use]
    pub fn with_markers(mut self, markers: impl IntoIterator<Item = String>) -> Self {
        self.markers = markers.into_iter().collect();
        self
    }

    /// The directories the walk will not pass, canonicalized.
    #[must_use]
    pub fn stops(&self) -> &[PathBuf] {
        &self.stops
    }

    /// Whether `dir` is at or above a stop.
    fn is_stop(&self, dir: &Path) -> Option<&Path> {
        self.stops
            .iter()
            .find(|stop| dir == stop.as_path())
            .map(PathBuf::as_path)
    }

    /// The outermost marker strictly below the first stop, searching up from
    /// `start`.
    ///
    /// Outermost rather than first, so a vendored subtree, submodule or linked
    /// worktree does not shadow the repository that contains it.
    ///
    /// # Errors
    ///
    /// Returns [`BoundError`] if no marker lies below the first stop, or if the
    /// only one found is at or above it.
    pub fn ceiling(&self, start: &Path) -> Result<Ceiling, BoundError> {
        let start = canonical(start)?;
        let mut outermost: Option<Ceiling> = None;
        let mut cursor = start.as_path();

        loop {
            // A stop is checked *before* the directory is considered, so a
            // marker sitting at a stop -- a dotfiles repository making `$HOME` a
            // source-control root -- never becomes a ceiling. D44 is explicit:
            // a marker found at or above a stop does not rescue the walk.
            if let Some(stop) = self.is_stop(cursor) {
                return outermost.ok_or_else(|| match self.marker_in(cursor) {
                    Some(marker) => BoundError::MarkerAtStop {
                        start: start.clone(),
                        marker,
                        stop: stop.to_path_buf(),
                    },
                    None => BoundError::NoMarker {
                        start: start.clone(),
                        stop: stop.to_path_buf(),
                    },
                });
            }

            if let Some(marker) = self.marker_in(cursor) {
                outermost = Some(Ceiling {
                    dir: cursor.to_path_buf(),
                    marker,
                });
            }

            match cursor.parent() {
                Some(parent) => cursor = parent,
                // The filesystem root is a stop whether or not it is listed.
                None => {
                    return outermost.ok_or_else(|| BoundError::NoMarker {
                        start: start.clone(),
                        stop: cursor.to_path_buf(),
                    });
                }
            }
        }
    }

    /// The marker this directory holds, if any.
    fn marker_in(&self, dir: &Path) -> Option<PathBuf> {
        self.markers.iter().find_map(|name| {
            let candidate = dir.join(name);
            // File *or* directory: git writes `.git` as a file in worktrees and
            // submodules, and `is_dir` alone silently fails in both.
            #[expect(
                clippy::disallowed_methods,
                reason = "discovery runs before any namespace exists; see the module docs"
            )]
            candidate.exists().then_some(candidate)
        })
    }

    /// Whether `root` may be the root of a grant.
    ///
    /// **This is the rule, and it is a predicate on the answer.** As first
    /// written D44 constrained only the search, and three routes reach a root
    /// without searching. Every route goes through here.
    ///
    /// # Errors
    ///
    /// Returns [`BoundError`] if no ceiling bounds `root`, or if `root` lies
    /// outside the one its own search produced.
    pub fn permits(&self, root: &Path) -> Result<Ceiling, BoundError> {
        let root = canonical(root)?;
        let ceiling = self.ceiling(&root)?;
        if !root.starts_with(&ceiling.dir) {
            return Err(BoundError::OutsideCeiling {
                root,
                ceiling: ceiling.dir,
            });
        }
        Ok(ceiling)
    }

    /// Searches upward from `start` for a file named `name`, within the bound.
    ///
    /// Independent of [`ceiling`](Self::ceiling), which is the whole point of
    /// D44's correction: one search finds the file, the other finds the
    /// ceiling, and the caller then requires the first to lie within the
    /// second.
    ///
    /// # Errors
    ///
    /// Returns [`BoundError::NoMarker`] if the search reaches a stop having
    /// found nothing.
    pub fn find_upward(&self, start: &Path, name: &str) -> Result<PathBuf, BoundError> {
        let start = canonical(start)?;
        let mut cursor = start.as_path();
        loop {
            let candidate = cursor.join(name);
            #[expect(
                clippy::disallowed_methods,
                reason = "discovery runs before any namespace exists; see the module docs"
            )]
            let found = candidate.is_file();
            if found {
                return Ok(candidate);
            }
            if let Some(stop) = self.is_stop(cursor) {
                let stop = stop.to_path_buf();
                return Err(BoundError::NoMarker { start, stop });
            }
            let Some(parent) = cursor.parent() else {
                let stop = cursor.to_path_buf();
                return Err(BoundError::NoMarker { start, stop });
            };
            cursor = parent;
        }
    }
}

/// The stop set for this platform, as literal paths.
///
/// Enumerated, never derived. See [`Bound::platform_default`].
const fn platform_stops() -> &'static [&'static str] {
    #[cfg(target_vendor = "apple")]
    {
        // `/private/tmp` is deliberately absent: it is what `/tmp`
        // canonicalizes to, and listing it stops discovery for every
        // temp-directory checkout.
        &[
            "/System",
            "/Library",
            "/usr",
            "/bin",
            "/sbin",
            "/opt",
            "/Applications",
            "/Volumes",
            "/private/etc",
        ]
    }
    #[cfg(all(unix, not(target_vendor = "apple")))]
    {
        &["/usr", "/etc", "/boot", "/proc", "/sys", "/dev"]
    }
    #[cfg(windows)]
    {
        // Resolved from the environment at use, since their locations are not
        // fixed. `SystemRoot` is `C:\Windows` on an ordinary install.
        &[]
    }
    #[cfg(not(any(unix, windows)))]
    {
        &[]
    }
}

/// Canonicalizes, reporting which path failed.
fn canonical(path: &Path) -> Result<PathBuf, BoundError> {
    #[expect(
        clippy::disallowed_methods,
        reason = "discovery runs before any namespace exists; see the module docs"
    )]
    path.canonicalize().map_err(|source| BoundError::Host {
        path: path.to_path_buf(),
        source,
    })
}

/// Canonicalizes a stop, dropping it if it does not exist on this machine.
///
/// A stop naming a directory that is not there bounds nothing, and keeping the
/// uncanonicalized string would compare against a path no walk ever produces.
fn canonical_if_present(path: &Path) -> Option<PathBuf> {
    #[expect(
        clippy::disallowed_methods,
        reason = "discovery runs before any namespace exists; see the module docs"
    )]
    path.canonicalize().ok()
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "test fixtures are built on the host, which is the one place that is the point"
)]
mod tests {
    use super::*;

    /// A tree under a temp directory, which is the case D44 warns a
    /// carelessly-written stop list breaks.
    struct Tree {
        root: tempfile::TempDir,
        repo: PathBuf,
        nested: PathBuf,
    }

    /// `<tmp>/repo/.git`, with `<tmp>/repo/sub/deep` beneath it.
    fn tree() -> Tree {
        let root = tempfile::tempdir().expect("temp dir");
        let repo = root.path().join("repo");
        let nested = repo.join("sub").join("deep");
        std::fs::create_dir_all(&nested).expect("mkdir");
        std::fs::create_dir(repo.join(".git")).expect("mkdir .git");
        Tree { root, repo, nested }
    }

    fn canonical_of(p: &Path) -> PathBuf {
        p.canonicalize().expect("canonicalize")
    }

    #[test]
    fn a_checkout_in_a_temp_directory_is_discoverable() {
        // **The case D44 predicts a stop list gets wrong.** On macOS `/tmp`
        // canonicalizes to `/private/tmp` and `$TMPDIR` to `/private/var/...`,
        // so a list containing `/private` or `/var` -- both of which read as
        // obvious system trees -- stops discovery for every temp checkout,
        // including this project's own scratch worktrees.
        let t = tree();
        let ceiling = Bound::platform_default()
            .ceiling(&t.nested)
            .expect("a temp-directory checkout must be discoverable");
        assert_eq!(ceiling.dir, canonical_of(&t.repo));
        assert_eq!(ceiling.marker, canonical_of(&t.repo).join(".git"));
    }

    #[test]
    fn no_stop_swallows_a_temp_path() {
        // The same hazard stated directly against the list, so a future edit
        // that adds `/private` or `/var` fails here rather than in a user's
        // checkout.
        let bound = Bound::platform_default();
        let scratch = canonical_of(tempfile::tempdir().expect("temp dir").path());
        for stop in bound.stops() {
            assert!(
                !scratch.starts_with(stop),
                "stop {} swallows temp directories like {}",
                stop.display(),
                scratch.display()
            );
        }
    }

    #[test]
    fn the_outermost_marker_wins_not_the_first() {
        // First-marker-wins deletes the monorepo root: a vendored subtree, a
        // submodule or a linked worktree carries `.git`, so a walk from inside
        // one stops there and discovery fails where upstream succeeds.
        let t = tree();
        let vendored = t.repo.join("vendor").join("inner");
        std::fs::create_dir_all(&vendored).expect("mkdir");
        // A worktree's `.git` is a *file*, which a directory-only check misses.
        std::fs::write(vendored.join(".git"), b"gitdir: elsewhere\n").expect("write .git");

        let ceiling = Bound::platform_default()
            .ceiling(&vendored)
            .expect("discovers a ceiling");
        assert_eq!(
            ceiling.dir,
            canonical_of(&t.repo),
            "the enclosing repository must win over the vendored one"
        );
    }

    #[test]
    fn a_marker_at_a_stop_does_not_rescue_the_walk() {
        // The dotfiles case, which is not hypothetical: a dotfiles repository
        // makes `$HOME` a source-control root, and `$HOME` as a grant is nearly
        // as bad as `/`.
        let root = tempfile::tempdir().expect("temp dir");
        let home = root.path().join("home");
        let project = home.join("project");
        std::fs::create_dir_all(&project).expect("mkdir");
        std::fs::create_dir(home.join(".git")).expect("mkdir .git");

        let bound = Bound {
            markers: DEFAULT_MARKERS.iter().map(|m| (*m).to_owned()).collect(),
            stops: vec![canonical_of(&home)],
        };
        assert!(matches!(
            bound.ceiling(&project),
            Err(BoundError::MarkerAtStop { .. })
        ));
    }

    #[test]
    fn a_directory_outside_its_ceiling_is_refused() {
        // The predicate applies to a root that was *named* rather than found,
        // which is the correction: `--justfile ~/justfile` reaches a root
        // without searching at all.
        let t = tree();
        let outside = canonical_of(t.root.path());
        let bound = Bound::platform_default();
        assert!(
            bound.permits(&t.nested).is_ok(),
            "a directory inside the repository is permitted"
        );
        assert!(
            matches!(
                bound.permits(&outside),
                Err(BoundError::NoMarker { .. } | BoundError::OutsideCeiling { .. })
            ),
            "the repository's parent is not inside any ceiling"
        );
    }

    #[test]
    fn the_marker_is_reported_so_a_grant_can_carve_it_out() {
        // `.git/config` carries `core.pager`, `core.fsmonitor` and `!sh -c`
        // aliases, and git runs what it finds there at the user's next command.
        // The walk knows where the marker is, so the caller never has to guess.
        let t = tree();
        let ceiling = Bound::platform_default()
            .ceiling(&t.nested)
            .expect("ceiling");
        assert!(ceiling.marker.starts_with(&ceiling.dir));
        assert_eq!(
            ceiling.marker.file_name().and_then(|n| n.to_str()),
            Some(".git")
        );
    }

    #[test]
    fn the_file_search_is_independent_of_the_ceiling() {
        // Two searches, not one. The file may sit anywhere at or below the
        // ceiling, and finding it says nothing about where the ceiling is.
        let t = tree();
        std::fs::write(t.repo.join("justfile"), b"default:\n").expect("write");
        let bound = Bound::platform_default();

        let found = bound
            .find_upward(&t.nested, "justfile")
            .expect("finds the file above");
        assert_eq!(found, canonical_of(&t.repo).join("justfile"));

        assert!(
            bound.find_upward(&t.nested, "no-such-file").is_err(),
            "a search that finds nothing must fail rather than walk to the root"
        );
    }
}
