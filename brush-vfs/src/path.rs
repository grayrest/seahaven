//! The virtual path grammar.
//!
//! A virtual path is `/`-separated, always absolute once resolved, and carries
//! no host dialect: no drive letters, no backslash separators, no Windows
//! reserved device names, no NTFS alternate data streams, no trailing dots or
//! spaces. Those are rejected on *every* platform, not only the one where they
//! are dangerous, because a namespace that behaves differently per host is not
//! a namespace — it is three of them.
//!
//! `..` is resolved lexically, before any host path exists. That is deliberate:
//! resolving it against the real filesystem is what makes symlink races
//! exploitable, and a path that cannot escape textually cannot escape at all.

use std::fmt;

use unicode_normalization::{IsNormalized, UnicodeNormalization, is_nfc_quick};

/// Why a path could not be interpreted as a virtual path.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PathError {
    /// The path climbed above the virtual root.
    ///
    /// `..` is resolved lexically, so this is decided without consulting the
    /// filesystem and cannot be raced.
    #[error("path escapes the virtual root")]
    Escape,

    /// The path contains a backslash.
    ///
    /// Windows accepts `\` as a separator, so allowing it would make
    /// `a\b` one component on Linux and two on Windows.
    #[error("path contains a backslash: only '/' separates components")]
    Backslash,

    /// A component contains a colon.
    ///
    /// This covers both drive letters (`C:`) and NTFS alternate data streams
    /// (`file.txt:hidden`), which are the same character doing two different
    /// jobs — and both name something outside the component's apparent identity.
    #[error("path component contains a colon: {component:?}")]
    Colon {
        /// The offending component.
        component: String,
    },

    /// A component is a Windows reserved device name.
    ///
    /// `CON`, `NUL`, `COM1` and friends resolve to devices rather than files on
    /// Windows, regardless of the directory they appear in, and regardless of
    /// any extension (`CON.txt` is still `CON`).
    #[error("path component is a reserved device name: {component:?}")]
    ReservedName {
        /// The offending component.
        component: String,
    },

    /// A component ends with a dot or a space.
    ///
    /// Windows silently strips these, so `foo.` and `foo` would be the same
    /// file there and different files elsewhere.
    #[error("path component ends with a dot or space: {component:?}")]
    TrailingDotOrSpace {
        /// The offending component.
        component: String,
    },

    /// The path contains a NUL byte, which no filesystem accepts.
    #[error("path contains an interior NUL byte")]
    InteriorNul,

    /// A relative path was resolved against something that is not a directory
    /// path, or an empty path was given where one was required.
    #[error("path is empty")]
    Empty,
}

/// Windows device names, which are reserved in every directory.
const RESERVED_NAMES: &[&str] = &[
    "con", "prn", "aux", "nul", "com0", "com1", "com2", "com3", "com4", "com5", "com6", "com7",
    "com8", "com9", "lpt0", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
];

/// An absolute, normalized path in the virtual namespace.
///
/// Construction is the only way to obtain one, so holding a `VirtualPath` is
/// evidence that the grammar accepted it and that `..` has already been
/// resolved. It is always absolute and never contains `.` or `..`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct VirtualPath {
    /// Always begins with `/`; never ends with `/` unless it is the root.
    inner: String,
}

impl VirtualPath {
    /// The virtual root, `/`.
    #[must_use]
    pub fn root() -> Self {
        Self {
            inner: String::from("/"),
        }
    }

    /// Interprets `path` as absolute, rejecting anything the grammar forbids.
    ///
    /// # Errors
    ///
    /// Returns [`PathError`] if the path is empty, is not absolute, escapes the
    /// root, or contains a construct with no single cross-platform meaning.
    pub fn new(path: &str) -> Result<Self, PathError> {
        Self::root().resolve(path)
    }

    /// Resolves `path` against this path, which is treated as a directory.
    ///
    /// An absolute `path` replaces this one entirely; a relative one extends it.
    /// Either way the result is normalized and validated.
    ///
    /// # Errors
    ///
    /// Returns [`PathError`] as [`VirtualPath::new`] does.
    pub fn resolve(&self, path: &str) -> Result<Self, PathError> {
        if path.is_empty() {
            return Err(PathError::Empty);
        }
        if path.contains('\0') {
            return Err(PathError::InteriorNul);
        }
        if path.contains('\\') {
            return Err(PathError::Backslash);
        }

        // An absolute path discards the base; a relative one extends it. Doing
        // this before validation means a relative path can never be smuggled in
        // as an absolute one by a component that only looks like a separator.
        let mut components: Vec<String> = if path.starts_with('/') {
            Vec::new()
        } else {
            self.components().map(str::to_owned).collect()
        };

        for raw in path.split('/') {
            // `//` and a trailing `/` both yield empty components; both are
            // conventionally no-ops rather than errors.
            match raw {
                "" | "." => {}
                ".." => {
                    // Popping past the root is the escape, and it is decided
                    // here rather than by the kernel.
                    if components.pop().is_none() {
                        return Err(PathError::Escape);
                    }
                }
                _ => components.push(validate_component(raw)?),
            }
        }

        Ok(Self {
            inner: if components.is_empty() {
                String::from("/")
            } else {
                let mut s = String::new();
                for c in &components {
                    s.push('/');
                    s.push_str(c);
                }
                s
            },
        })
    }

    /// The path as a string, always beginning with `/`.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.inner
    }

    /// Iterates the path's components, which are never empty, `.` or `..`.
    pub fn components(&self) -> impl DoubleEndedIterator<Item = &str> {
        self.inner.split('/').filter(|c| !c.is_empty())
    }

    /// Whether this is the virtual root.
    #[must_use]
    pub fn is_root(&self) -> bool {
        self.inner == "/"
    }

    /// The parent path, or `None` at the root.
    #[must_use]
    pub fn parent(&self) -> Option<Self> {
        if self.is_root() {
            return None;
        }
        // `get` rather than indexing: slicing a `str` by byte offset panics on a
        // char boundary, and the workspace bans it for exactly that reason.
        let idx = self.inner.rfind('/')?;
        Some(Self {
            inner: match self.inner.get(..idx) {
                None | Some("") => String::from("/"),
                Some(parent) => parent.to_owned(),
            },
        })
    }

    /// The final component, or `None` at the root.
    #[must_use]
    pub fn file_name(&self) -> Option<&str> {
        self.components().next_back()
    }

    /// Whether this path is `prefix` or lies beneath it.
    ///
    /// Compares whole components, so `/ab` is not beneath `/a` — the mistake
    /// that a string-prefix comparison makes and that this exists to avoid.
    #[must_use]
    pub fn starts_with(&self, prefix: &Self) -> bool {
        if prefix.is_root() {
            return true;
        }
        let mut ours = self.components();
        prefix.components().all(|p| ours.next() == Some(p))
    }

    /// The components of this path relative to `prefix`, if it lies beneath it.
    #[must_use]
    pub fn strip_prefix(&self, prefix: &Self) -> Option<Vec<&str>> {
        if !self.starts_with(prefix) {
            return None;
        }
        Some(
            self.components()
                .skip(prefix.components().count())
                .collect(),
        )
    }
}

impl fmt::Display for VirtualPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.inner)
    }
}

/// Validates one path component and returns it in NFC.
///
/// Normalizing here rather than at comparison time means two spellings of the
/// same name cannot denote two different files: macOS historically stored NFD,
/// so a name typed as NFC and one read back as NFD would otherwise be distinct
/// paths for identical bytes on disk.
fn validate_component(component: &str) -> Result<String, PathError> {
    if component.contains(':') {
        return Err(PathError::Colon {
            component: component.to_owned(),
        });
    }

    if component.ends_with('.') || component.ends_with(' ') {
        return Err(PathError::TrailingDotOrSpace {
            component: component.to_owned(),
        });
    }

    // A reserved name is reserved with or without an extension, so compare the
    // stem rather than the whole component.
    let stem = component.split_once('.').map_or(component, |(s, _)| s);
    if RESERVED_NAMES.iter().any(|r| stem.eq_ignore_ascii_case(r)) {
        return Err(PathError::ReservedName {
            component: component.to_owned(),
        });
    }

    Ok(match is_nfc_quick(component.chars()) {
        IsNormalized::Yes => component.to_owned(),
        _ => component.nfc().collect(),
    })
}

/// Folds a component for collision detection.
///
/// Two components that fold alike name the same file on a case-insensitive host
/// even though they are distinct strings. Mount loading uses this to refuse a
/// layout that would mean different things on different platforms, rather than
/// discovering the ambiguity at open time.
#[must_use]
pub fn fold_for_collision(component: &str) -> String {
    component.nfc().flat_map(char::to_lowercase).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(p: &str) -> String {
        VirtualPath::new(p).expect("should be accepted").inner
    }

    fn err(p: &str) -> PathError {
        VirtualPath::new(p).expect_err("should be rejected")
    }

    #[test]
    fn absolute_paths_normalize() {
        assert_eq!(ok("/"), "/");
        assert_eq!(ok("/a/b"), "/a/b");
        assert_eq!(ok("/a//b"), "/a/b");
        assert_eq!(ok("/a/b/"), "/a/b");
        assert_eq!(ok("/a/./b"), "/a/b");
        assert_eq!(ok("/a/b/.."), "/a");
        assert_eq!(ok("/a/../b"), "/b");
        assert_eq!(ok("/a/b/../.."), "/");
    }

    #[test]
    fn dotdot_that_stays_inside_is_allowed() {
        // Rejecting `..` outright would break ordinary relative navigation;
        // only leaving the root is an escape.
        assert_eq!(ok("/work/../work/x"), "/work/x");
    }

    #[test]
    fn dotdot_above_root_escapes() {
        assert_eq!(err("/.."), PathError::Escape);
        assert_eq!(err("/a/../.."), PathError::Escape);
        assert_eq!(err("/../etc/passwd"), PathError::Escape);
    }

    #[test]
    fn relative_paths_resolve_against_a_base() {
        let base = VirtualPath::new("/work/src").unwrap();
        assert_eq!(
            base.resolve("main.rs").unwrap().as_str(),
            "/work/src/main.rs"
        );
        assert_eq!(base.resolve("../lib.rs").unwrap().as_str(), "/work/lib.rs");
        assert_eq!(base.resolve("/etc").unwrap().as_str(), "/etc");
        assert_eq!(base.resolve("../../..").unwrap_err(), PathError::Escape);
    }

    #[test]
    fn host_dialects_are_refused() {
        assert_eq!(err(r"/a\b"), PathError::Backslash);
        assert!(matches!(err("/C:/Windows"), PathError::Colon { .. }));
        assert!(matches!(err("/a/file.txt:hidden"), PathError::Colon { .. }));
    }

    #[test]
    fn windows_reserved_names_are_refused_everywhere() {
        // Rejected on Linux too: the namespace has to mean one thing.
        assert!(matches!(err("/CON"), PathError::ReservedName { .. }));
        assert!(matches!(err("/a/nul"), PathError::ReservedName { .. }));
        assert!(matches!(err("/a/CoM1"), PathError::ReservedName { .. }));
        // Reserved with an extension too.
        assert!(matches!(err("/a/con.txt"), PathError::ReservedName { .. }));
        // But only as a whole stem.
        assert_eq!(ok("/a/console"), "/a/console");
        assert_eq!(ok("/a/context.txt"), "/a/context.txt");
    }

    #[test]
    fn trailing_dot_or_space_is_refused() {
        assert!(matches!(
            err("/a/foo."),
            PathError::TrailingDotOrSpace { .. }
        ));
        assert!(matches!(
            err("/a/foo "),
            PathError::TrailingDotOrSpace { .. }
        ));
        // A leading dot is an ordinary hidden file.
        assert_eq!(ok("/a/.hidden"), "/a/.hidden");
    }

    #[test]
    fn nul_and_empty_are_refused() {
        assert_eq!(err("/a/\0b"), PathError::InteriorNul);
        assert_eq!(err(""), PathError::Empty);
    }

    #[test]
    fn components_are_normalized_to_nfc() {
        // U+0065 U+0301 (e + combining acute) folds to U+00E9.
        let decomposed = "/caf\u{0065}\u{0301}";
        let composed = "/caf\u{00e9}";
        assert_eq!(ok(decomposed), composed);
        assert_eq!(VirtualPath::new(decomposed), VirtualPath::new(composed));
    }

    #[test]
    fn prefix_comparison_is_by_component() {
        let a = VirtualPath::new("/a").unwrap();
        assert!(VirtualPath::new("/a/b").unwrap().starts_with(&a));
        assert!(a.starts_with(&a));
        // The bug a string-prefix check would have.
        assert!(!VirtualPath::new("/ab").unwrap().starts_with(&a));
        assert!(
            VirtualPath::new("/anything")
                .unwrap()
                .starts_with(&VirtualPath::root())
        );
    }

    #[test]
    fn strip_prefix_yields_relative_components() {
        let root = VirtualPath::new("/work").unwrap();
        let p = VirtualPath::new("/work/src/main.rs").unwrap();
        assert_eq!(p.strip_prefix(&root), Some(vec!["src", "main.rs"]));
        assert_eq!(root.strip_prefix(&root), Some(vec![]));
        assert_eq!(
            VirtualPath::new("/other").unwrap().strip_prefix(&root),
            None
        );
    }

    #[test]
    fn parent_and_file_name() {
        let p = VirtualPath::new("/a/b/c").unwrap();
        assert_eq!(p.parent().unwrap().as_str(), "/a/b");
        assert_eq!(p.file_name(), Some("c"));
        let top = VirtualPath::new("/a").unwrap();
        assert_eq!(top.parent().unwrap().as_str(), "/");
        assert_eq!(VirtualPath::root().parent(), None);
        assert_eq!(VirtualPath::root().file_name(), None);
    }

    #[test]
    fn collision_folding_catches_case_and_form() {
        assert_eq!(fold_for_collision("README"), fold_for_collision("readme"));
        assert_eq!(
            fold_for_collision("caf\u{0065}\u{0301}"),
            fold_for_collision("caf\u{00e9}")
        );
        assert_ne!(fold_for_collision("a"), fold_for_collision("b"));
    }
}
