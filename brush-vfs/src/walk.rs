//! A recursive walk that stays inside the namespace.
//!
//! The last thing in the coreutils fork set that reached the filesystem without
//! asking: `find`, `grep -r`, `cp -r` and `chmod -R` all traversed with
//! `walkdir`, which opens directories by path and reads them itself. `deny.toml`
//! banned the crate for exactly that reason and then listed every consumer as an
//! exemption.
//!
//! # Why it mirrors `walkdir`
//!
//! The builder, the iterator, `skip_current_dir`, the entry and error shapes are
//! all `walkdir`'s. That keeps each consumer's change an identifier swap rather
//! than a restructure — the same bargain [`crate::ambient`] makes for
//! `std::fs` — and it means the two can be run side by side over one tree and
//! required to agree, which is how the semantics here are checked rather than
//! asserted.
//!
//! [`DirEntry::path`] returns a **virtual** path. Handing back a host path would
//! make every consumer half-virtual: the walk from one filesystem, the reads
//! that follow it from another.
//!
//! # Why it is anchored rather than path-based
//!
//! Each level is a [`crate::dir::Dir`], and a child is opened *from its parent's
//! handle by name*. `walkdir` re-opens by full path at every level, so a
//! directory renamed mid-walk redirects the rest of it. Here the only path
//! resolution is the root's, once. That is the property
//! `uucore::safe_traversal` exists to provide, and recursive `chmod` now has it.

use std::cmp::Ordering;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::dir::{Dir, FileIdentity};
use crate::fs::Vfs;
use crate::path::VirtualPath;

/// One entry yielded by a walk.
#[derive(Debug)]
pub struct DirEntry {
    path: VirtualPath,
    /// The path as the *caller* would spell it: the root exactly as given, with
    /// each component joined onto it.
    ///
    /// Separate from `path` because `find ./x` must print `./x/y`, not the
    /// absolute path `./x` resolved to. `walkdir` keeps the caller's spelling
    /// for the same reason, and `find`'s entire output is these strings.
    display: PathBuf,
    name: String,
    depth: usize,
    metadata: std::fs::Metadata,
    /// Whether the entry *itself* is a symlink. Distinct from
    /// `metadata.is_symlink()`, which describes the target once a link has been
    /// followed.
    is_symlink: bool,
    /// Whether a symlink was resolved to produce `metadata`. Only these are
    /// loop-checked, matching `walkdir`: a directory cannot be its own ancestor
    /// without one.
    followed: bool,
}

impl DirEntry {
    /// The entry's path, spelled as the caller spelled the root.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.display
    }

    /// The entry's path, consuming it. Mirrors `walkdir::DirEntry::into_path`.
    #[must_use]
    pub fn into_path(self) -> PathBuf {
        self.display
    }

    /// The entry's path as a [`VirtualPath`], for callers already inside the vfs.
    #[must_use]
    pub const fn virtual_path(&self) -> &VirtualPath {
        &self.path
    }

    /// The final component of the entry's path.
    #[must_use]
    pub fn file_name(&self) -> &OsStr {
        OsStr::new(&self.name)
    }

    /// Depth below the walk's root; the root itself is 0.
    #[must_use]
    pub const fn depth(&self) -> usize {
        self.depth
    }

    /// The entry's type, describing the link itself unless it was followed.
    #[must_use]
    pub fn file_type(&self) -> std::fs::FileType {
        self.metadata.file_type()
    }

    /// Whether the entry itself is a symlink, whatever [`Self::file_type`] says.
    ///
    /// True for a *followed* link too: the metadata then describes the target,
    /// but the path is still a link, and `find -type l` depends on the
    /// difference. `walkdir` keeps the same two facts apart for the same reason.
    #[must_use]
    pub const fn path_is_symlink(&self) -> bool {
        self.is_symlink || self.followed
    }

    /// The entry's metadata.
    ///
    /// Already known — the walk had to stat the entry to decide whether to
    /// descend — so this cannot fail. It returns a `Result` anyway, because
    /// `walkdir`'s does and the consumers are written against that.
    ///
    /// # Errors
    ///
    /// Never; the signature exists for call-site compatibility.
    pub fn metadata(&self) -> Result<std::fs::Metadata, Error> {
        Ok(self.metadata.clone())
    }

    /// The entry's inode number.
    #[cfg(unix)]
    #[must_use]
    pub fn ino(&self) -> u64 {
        use std::os::unix::fs::MetadataExt as _;
        self.metadata.ino()
    }
}

/// Why a walk could not produce an entry.
#[derive(Debug)]
pub struct Error {
    depth: usize,
    path: Option<PathBuf>,
    kind: ErrorKind,
}

#[derive(Debug)]
enum ErrorKind {
    Io(std::io::Error),
    /// A followed symlink reached a directory already on the path to it.
    Loop {
        ancestor: PathBuf,
    },
}

impl Error {
    /// `path` is the caller's spelling, not the resolved one.
    ///
    /// An error is output too: `find -L` reports a broken link by the path it
    /// was asked about, and findutils turns a not-found error back into an entry
    /// using exactly this path. Handing back the resolved form there printed
    /// absolute paths in the middle of relative ones.
    fn io(depth: usize, path: Option<&Path>, error: std::io::Error) -> Self {
        Self {
            depth,
            path: path.map(Path::to_path_buf),
            kind: ErrorKind::Io(error),
        }
    }

    fn symlink_loop(depth: usize, ancestor: &Path, child: &Path) -> Self {
        Self {
            depth,
            path: Some(child.to_path_buf()),
            kind: ErrorKind::Loop {
                ancestor: ancestor.to_path_buf(),
            },
        }
    }

    /// The path the error is about, if it has one.
    #[must_use]
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Depth at which the error occurred.
    #[must_use]
    pub const fn depth(&self) -> usize {
        self.depth
    }

    /// The underlying I/O error, if this was one.
    ///
    /// `None` for a symlink loop, which is how `uucore::perms` distinguishes the
    /// two — so this must stay `None` there.
    #[must_use]
    pub const fn io_error(&self) -> Option<&std::io::Error> {
        match &self.kind {
            ErrorKind::Io(e) => Some(e),
            ErrorKind::Loop { .. } => None,
        }
    }

    /// The ancestor a loop closed back onto, if this was a loop.
    #[must_use]
    pub fn loop_ancestor(&self) -> Option<&Path> {
        match &self.kind {
            ErrorKind::Loop { ancestor } => Some(ancestor),
            ErrorKind::Io(_) => None,
        }
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.kind {
            ErrorKind::Io(e) => match &self.path {
                Some(p) => write!(f, "IO error for operation on {}: {e}", p.display()),
                None => write!(f, "IO error: {e}"),
            },
            ErrorKind::Loop { ancestor } => write!(
                f,
                "File system loop found: {} points to an ancestor {}",
                self.path
                    .as_deref()
                    .unwrap_or_else(|| Path::new("?"))
                    .display(),
                ancestor.display()
            ),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match &self.kind {
            ErrorKind::Io(e) => Some(e),
            ErrorKind::Loop { .. } => None,
        }
    }
}

impl From<Error> for std::io::Error {
    fn from(err: Error) -> Self {
        match err.kind {
            ErrorKind::Io(e) => e,
            ErrorKind::Loop { .. } => Self::other(crate::fs::SYMLINK_LOOP_MESSAGE),
        }
    }
}

type Sorter = Box<dyn FnMut(&DirEntry, &DirEntry) -> Ordering>;

struct Options {
    min_depth: usize,
    max_depth: usize,
    follow_links: bool,
    follow_root_links: bool,
    same_file_system: bool,
    contents_first: bool,
    sorter: Option<Sorter>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            min_depth: 0,
            max_depth: usize::MAX,
            follow_links: false,
            follow_root_links: true,
            same_file_system: false,
            contents_first: false,
            sorter: None,
        }
    }
}

/// A recursive walk, configured but not yet started.
///
/// Built by [`crate::ambient::walk`]. Options mirror `walkdir::WalkDir`.
pub struct Walk {
    vfs: Option<Arc<Vfs>>,
    root: Option<VirtualPath>,
    /// The root as the caller spelled it; see [`DirEntry::path`].
    display: Option<PathBuf>,
    /// A failure to resolve the root, held until the iterator can yield it.
    ///
    /// `walkdir::WalkDir::new` is infallible and reports through the iterator,
    /// so this one is too — otherwise every call site would need a `?` the
    /// original did not have.
    deferred: Option<std::io::Error>,
    opts: Options,
}

impl std::fmt::Debug for Walk {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Walk")
            .field("root", &self.root)
            .finish_non_exhaustive()
    }
}

impl Walk {
    /// A walk rooted at an already-resolved path.
    pub(crate) fn rooted(vfs: Arc<Vfs>, root: VirtualPath, display: PathBuf) -> Self {
        Self {
            vfs: Some(vfs),
            root: Some(root),
            display: Some(display),
            deferred: None,
            opts: Options::default(),
        }
    }

    /// A walk that will yield `error` and nothing else.
    pub(crate) fn failed(error: std::io::Error) -> Self {
        Self {
            vfs: None,
            root: None,
            display: None,
            deferred: Some(error),
            opts: Options::default(),
        }
    }

    /// Entries shallower than `depth` are not yielded; the walk still descends
    /// through them.
    #[must_use]
    pub const fn min_depth(mut self, depth: usize) -> Self {
        self.opts.min_depth = depth;
        if self.opts.min_depth > self.opts.max_depth {
            self.opts.min_depth = self.opts.max_depth;
        }
        self
    }

    /// The walk does not descend past `depth`.
    #[must_use]
    pub const fn max_depth(mut self, depth: usize) -> Self {
        self.opts.max_depth = depth;
        if self.opts.max_depth < self.opts.min_depth {
            self.opts.max_depth = self.opts.min_depth;
        }
        self
    }

    /// Follow symlinks, reporting entries as their targets.
    #[must_use]
    pub const fn follow_links(mut self, yes: bool) -> Self {
        self.opts.follow_links = yes;
        self
    }

    /// Follow a symlink given as the walk's root even when `follow_links` is off.
    #[must_use]
    pub const fn follow_root_links(mut self, yes: bool) -> Self {
        self.opts.follow_root_links = yes;
        self
    }

    /// Do not descend into a directory on a different filesystem from the root.
    #[must_use]
    pub const fn same_file_system(mut self, yes: bool) -> Self {
        self.opts.same_file_system = yes;
        self
    }

    /// Yield a directory's contents before the directory itself.
    #[must_use]
    pub const fn contents_first(mut self, yes: bool) -> Self {
        self.opts.contents_first = yes;
        self
    }

    /// Order each directory's entries.
    #[must_use]
    pub fn sort_by<F>(mut self, cmp: F) -> Self
    where
        F: FnMut(&DirEntry, &DirEntry) -> Ordering + 'static,
    {
        self.opts.sorter = Some(Box::new(cmp));
        self
    }

    /// Order each directory's entries by file name.
    #[must_use]
    pub fn sort_by_file_name(self) -> Self {
        self.sort_by(|a, b| a.file_name().cmp(b.file_name()))
    }
}

impl IntoIterator for Walk {
    type Item = Result<DirEntry, Error>;
    type IntoIter = IntoIter;

    fn into_iter(self) -> Self::IntoIter {
        IntoIter {
            vfs: self.vfs,
            start: self.root.zip(self.display),
            deferred: self.deferred,
            opts: self.opts,
            stack: Vec::new(),
            deferred_dirs: Vec::new(),
            depth: 0,
            root_device: None,
            done: false,
        }
    }
}

/// One open directory level.
struct Frame {
    dir: Dir,
    path: VirtualPath,
    display: PathBuf,
    entries: std::vec::IntoIter<String>,
    /// Present only when following links, which is the only time a loop is
    /// possible and so the only time the ancestry is worth keeping.
    identity: Option<FileIdentity>,
}

/// A walk in progress.
pub struct IntoIter {
    vfs: Option<Arc<Vfs>>,
    start: Option<(VirtualPath, PathBuf)>,
    deferred: Option<std::io::Error>,
    opts: Options,
    stack: Vec<Frame>,
    deferred_dirs: Vec<DirEntry>,
    depth: usize,
    root_device: Option<u64>,
    done: bool,
}

impl std::fmt::Debug for IntoIter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("walk::IntoIter")
            .field("depth", &self.depth)
            .finish_non_exhaustive()
    }
}

impl IntoIter {
    /// Abandon the directory currently being read.
    ///
    /// Mirrors `walkdir::IntoIter::skip_current_dir`, including its shape: it is
    /// called *after* an entry has been yielded, and drops the rest of that
    /// entry's containing directory.
    pub fn skip_current_dir(&mut self) {
        if !self.stack.is_empty() {
            self.pop();
        }
    }

    fn pop(&mut self) {
        self.stack.pop();
    }

    const fn skippable(&self) -> bool {
        self.depth < self.opts.min_depth || self.depth > self.opts.max_depth
    }

    fn get_deferred_dir(&mut self) -> Option<DirEntry> {
        if self.opts.contents_first && self.depth < self.deferred_dirs.len() {
            let deferred = self.deferred_dirs.pop()?;
            if !self.skippable() {
                return Some(deferred);
            }
        }
        None
    }
}

impl Iterator for IntoIter {
    type Item = Result<DirEntry, Error>;

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(error) = self.deferred.take() {
            self.done = true;
            return Some(Err(Error::io(
                0,
                self.start.as_ref().map(|(_, d)| d.as_path()),
                error,
            )));
        }
        if self.done {
            return None;
        }
        let vfs = self.vfs.clone()?;

        if let Some((root, display)) = self.start.take() {
            match self.root_entry(&vfs, root, display) {
                Err(e) => {
                    self.done = true;
                    return Some(Err(e));
                }
                Ok(dent) => {
                    if let Some(result) = self.handle_entry(&vfs, dent) {
                        return Some(result);
                    }
                }
            }
        }

        while !self.stack.is_empty() {
            self.depth = self.stack.len();
            if let Some(dent) = self.get_deferred_dir() {
                return Some(Ok(dent));
            }
            if self.depth > self.opts.max_depth {
                self.pop();
                continue;
            }
            let next = self.stack.last_mut().and_then(|f| f.entries.next());
            match next {
                None => self.pop(),
                Some(name) => match self.child_entry(&name) {
                    Err(e) => return Some(Err(e)),
                    Ok(dent) => {
                        if let Some(result) = self.handle_entry(&vfs, dent) {
                            return Some(result);
                        }
                    }
                },
            }
        }

        if self.opts.contents_first {
            self.depth = self.stack.len();
            if let Some(dent) = self.get_deferred_dir() {
                return Some(Ok(dent));
            }
        }
        self.done = true;
        None
    }
}

impl IntoIter {
    /// Stats the walk's root, which has no parent handle to be relative to.
    #[expect(
        clippy::unused_self,
        reason = "reads as a method alongside child_entry"
    )]
    fn root_entry(
        &self,
        vfs: &Vfs,
        root: VirtualPath,
        display: PathBuf,
    ) -> Result<DirEntry, Error> {
        let metadata = vfs
            .symlink_metadata(&root)
            .map_err(|e| Error::io(0, Some(&display), e))?;
        let name = root
            .as_str()
            .rsplit('/')
            .next()
            .unwrap_or_default()
            .to_string();
        Ok(DirEntry {
            is_symlink: metadata.is_symlink(),
            path: root,
            display,
            name,
            depth: 0,
            metadata,
            followed: false,
        })
    }

    /// Stats a child by name, relative to the directory currently being read.
    ///
    /// The name never leaves the parent's handle, so nothing here re-resolves a
    /// path — the difference from `walkdir` that makes a rename mid-walk unable
    /// to redirect the rest of it.
    fn child_entry(&self, name: &str) -> Result<DirEntry, Error> {
        let depth = self.depth;
        let frame = self.stack.last().ok_or_else(|| {
            Error::io(
                depth,
                None,
                std::io::Error::other("BUG: child entry with no open directory"),
            )
        })?;
        let path = frame.path.resolve(name).map_err(|e| {
            Error::io(
                depth,
                Some(&frame.display),
                std::io::Error::other(e.to_string()),
            )
        })?;
        let display = frame.display.join(name);
        let metadata = frame
            .dir
            .symlink_metadata(name)
            .map_err(|e| Error::io(depth, Some(&display), e))?;
        Ok(DirEntry {
            is_symlink: metadata.is_symlink(),
            path,
            display,
            name: name.to_string(),
            depth,
            metadata,
            followed: false,
        })
    }

    /// Decides whether an entry is descended into, deferred, or yielded.
    ///
    /// The order of the checks is `walkdir`'s, because the observable behaviour
    /// depends on it: a followed link becomes an ordinary directory *before* the
    /// same-filesystem test, and a deferred directory is withheld *after* the
    /// descent has been arranged.
    fn handle_entry(&mut self, vfs: &Vfs, mut dent: DirEntry) -> Option<Result<DirEntry, Error>> {
        if self.opts.follow_links && dent.is_symlink {
            match self.follow(vfs, dent) {
                Ok(d) => dent = d,
                Err(e) => return Some(Err(e)),
            }
        }

        let is_normal_dir = !dent.is_symlink && dent.metadata.is_dir();
        if is_normal_dir {
            if let Err(e) = self.push(vfs, &dent) {
                return Some(Err(e));
            }
        } else if dent.depth == 0 && dent.is_symlink && self.opts.follow_root_links {
            // A root that is a link is followed regardless, but keeps reporting
            // itself as a link -- `walkdir` is careful about this and so is
            // `find`, whose `-type l` on the root would otherwise change.
            match vfs.metadata(&dent.path) {
                Ok(md) if md.is_dir() => {
                    if let Err(e) = self.push(vfs, &dent) {
                        return Some(Err(e));
                    }
                }
                Ok(_) => {}
                Err(e) => return Some(Err(Error::io(dent.depth, Some(&dent.display), e))),
            }
        }

        if is_normal_dir && self.opts.contents_first {
            self.deferred_dirs.push(dent);
            None
        } else if self.skippable() {
            None
        } else {
            Some(Ok(dent))
        }
    }

    /// Re-stats a symlink as its target.
    #[expect(
        clippy::unused_self,
        reason = "reads as a method alongside push/handle_entry"
    )]
    fn follow(&self, vfs: &Vfs, dent: DirEntry) -> Result<DirEntry, Error> {
        let metadata = vfs
            .metadata(&dent.path)
            .map_err(|e| Error::io(dent.depth, Some(&dent.display), e))?;
        Ok(DirEntry {
            metadata,
            is_symlink: false,
            followed: true,
            ..dent
        })
    }

    /// Opens a directory and makes it the level the walk reads from next.
    ///
    /// Also where the two checks that can *refuse* a descent live, because both
    /// need the open handle: a loop closes back onto an ancestor's identity, and
    /// a different filesystem shows up as a different device. `walkdir` stats
    /// the path for each instead; taking them from the handle costs one fewer
    /// syscall and cannot describe a different file than the one being entered.
    fn push(&mut self, vfs: &Vfs, dent: &DirEntry) -> Result<(), Error> {
        let dir = self.open_child(vfs, dent)?;

        let identity = if self.opts.follow_links || self.opts.same_file_system {
            Some(
                dir.identity()
                    .map_err(|e| Error::io(dent.depth, Some(&dent.display), e))?,
            )
        } else {
            None
        };

        if dent.depth == 0 {
            self.root_device = identity.map(|i| i.device);
        } else if self.opts.same_file_system
            && let (Some(id), Some(root)) = (identity, self.root_device)
            && id.device != root
        {
            // Not an error: crossing a mount point simply ends this branch.
            return Ok(());
        }

        if dent.followed
            && let Some(id) = identity
            && let Some(ancestor) = self.stack.iter().rev().find(|f| f.identity == Some(id))
        {
            return Err(Error::symlink_loop(
                dent.depth,
                &ancestor.display,
                &dent.display,
            ));
        }

        let mut entries = dir
            .entry_names()
            .map_err(|e| Error::io(dent.depth, Some(&dent.display), e))?;

        if let Some(cmp) = self.opts.sorter.as_mut() {
            // Sorting wants entries, but reading every child's metadata to build
            // them would cost a stat per entry even when the comparator only
            // looks at names. The names are what a comparator can see without
            // one, so they are what is compared.
            let path = dent.path.clone();
            let mut keyed: Vec<(String, DirEntry)> = entries
                .into_iter()
                .map(|name| {
                    let entry = DirEntry {
                        path: path.clone(),
                        display: PathBuf::from(&name),
                        name: name.clone(),
                        depth: dent.depth + 1,
                        metadata: dent.metadata.clone(),
                        is_symlink: false,
                        followed: false,
                    };
                    (name, entry)
                })
                .collect();
            keyed.sort_by(|a, b| cmp(&a.1, &b.1));
            entries = keyed.into_iter().map(|(name, _)| name).collect();
        }

        self.stack.push(Frame {
            dir,
            path: dent.path.clone(),
            display: dent.display.clone(),
            entries: entries.into_iter(),
            identity,
        });
        Ok(())
    }

    /// Opens the directory a walk is about to descend into.
    ///
    /// An ordinary child comes from the handle above it, which is what keeps the
    /// descent anchored. Two cases cannot: the root, which has nothing above it,
    /// and a *followed symlink*, which by definition names somewhere else in the
    /// namespace — possibly above the parent, as `../..` does. Asking the
    /// parent's handle for those would be refused by cap-std, correctly, since
    /// they are not beneath it. Both go through the namespace instead, which is
    /// the only thing that can resolve a link and still confine the result.
    fn open_child(&self, vfs: &Vfs, dent: &DirEntry) -> Result<Dir, Error> {
        let anchored = self.stack.last().filter(|_| !dent.followed);
        let opened = match anchored {
            Some(frame) => frame.dir.open_dir(&dent.name),
            None => vfs.open_dir(&dent.path),
        };
        opened.map_err(|e| Error::io(dent.depth, Some(&dent.display), e))
    }
}
