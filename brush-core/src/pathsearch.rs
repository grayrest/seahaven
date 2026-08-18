//! Path searching utilities.

use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
};

use crate::namespace::{is_dir, is_executable, to_virtual_path};
use crate::sys;

/// Encapsulates the result of a path search.
pub struct ExecutablePathSearch<'a, PI, N> {
    session: &'a brush_vfs::Session,
    paths: VecDeque<PI>,
    filename: N,
}

impl<PI, N> Iterator for ExecutablePathSearch<'_, PI, N>
where
    PI: AsRef<Path>,
    N: AsRef<Path>,
{
    type Item = PathBuf;

    fn next(&mut self) -> Option<Self::Item> {
        while let Some(path) = self.paths.pop_front() {
            let path = PathBuf::from(path.as_ref()).join(self.filename.as_ref());
            // Skip directories outright, then take the first name the platform
            // would accept for this command (on Windows, that may mean a
            // PATHEXT extension appended) that the namespace says is
            // executable. Candidate enumeration is lexical; only the namespace
            // decides what exists.
            if is_dir(self.session, &path) {
                continue;
            }
            if let Some(resolved) = sys::fs::executable_candidates(path)
                .into_iter()
                .find(|candidate| is_executable(self.session, candidate))
            {
                return Some(resolved);
            }
        }
        None
    }
}

pub(crate) struct ExecutablePathPrefixSearch<'a, PI> {
    session: &'a brush_vfs::Session,
    paths: VecDeque<PI>,
    queued_items: VecDeque<PathBuf>,
    filename_prefix: String,
    case_insensitive: bool,
}

impl<PI> Iterator for ExecutablePathPrefixSearch<'_, PI>
where
    PI: AsRef<Path>,
{
    type Item = PathBuf;

    fn next(&mut self) -> Option<Self::Item> {
        // If we already found some items and queued them, then yield one now.
        if let Some(item) = self.queued_items.pop_front() {
            return Some(item);
        }

        while let Some(path) = self.paths.pop_front() {
            let path = PathBuf::from(path.as_ref());

            let names = to_virtual_path(self.session, &path)
                .and_then(|dir| self.session.vfs().read_dir_names(&dir));
            if let Ok(names) = names {
                for name in names {
                    let comparable = if self.case_insensitive {
                        name.to_ascii_lowercase()
                    } else {
                        name.clone()
                    };

                    if !comparable.starts_with(&self.filename_prefix) {
                        continue;
                    }

                    // A directory is never a completion for a command, but a
                    // symlink to one elsewhere in the namespace is fine, so
                    // ask about the link itself and let `is_executable`
                    // follow it.
                    let entry_path = path.join(&name);
                    let is_dir_entry = to_virtual_path(self.session, &entry_path)
                        .ok()
                        .and_then(|p| self.session.vfs().facts(&p, false))
                        .is_some_and(|facts| facts.is_dir);
                    if is_dir_entry {
                        continue;
                    }

                    if is_executable(self.session, &entry_path) {
                        self.queued_items.push_back(entry_path);
                    }
                }
            }
            if let Some(item) = self.queued_items.pop_front() {
                return Some(item);
            }
        }

        None
    }
}

/// Search for the given executable name in the provided paths.
///
/// # Arguments
///
/// * `session` - The namespace the search resolves against.
/// * `paths` - An iterator over the paths to search.
/// * `filename` - The name of the executable file to search for.
pub fn search_for_executable<P, PI, N>(
    session: &brush_vfs::Session,
    paths: P,
    filename: N,
) -> ExecutablePathSearch<'_, PI, N>
where
    P: Iterator<Item = PI>,
    PI: AsRef<Path>,
    N: AsRef<Path>,
{
    ExecutablePathSearch {
        session,
        paths: paths.collect(),
        filename,
    }
}

pub(crate) fn search_for_executable_with_prefix<'a, P, PI>(
    session: &'a brush_vfs::Session,
    paths: P,
    filename_prefix: &str,
    case_insensitive: bool,
) -> ExecutablePathPrefixSearch<'a, PI>
where
    P: Iterator<Item = PI>,
    PI: AsRef<Path>,
{
    let stored_prefix = if case_insensitive {
        filename_prefix.to_ascii_lowercase()
    } else {
        filename_prefix.into()
    };

    ExecutablePathPrefixSearch {
        session,
        paths: paths.collect(),
        queued_items: VecDeque::new(),
        filename_prefix: stored_prefix,
        case_insensitive,
    }
}
