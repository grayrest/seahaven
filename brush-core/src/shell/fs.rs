//! Filesystem interaction in the shell.

use std::path::{Path, PathBuf};

use normalize_path::NormalizePath as _;

use crate::{
    ExecutionParameters, ShellFd,
    env::{EnvironmentLookup, EnvironmentScope},
    error, openfiles, pathsearch,
    sys::{fs::PathExt as _, users},
    variables,
};

impl<SE: crate::extensions::ShellExtensions> crate::Shell<SE> {
    /// Sets the shell's current working directory to the given path.
    ///
    /// # Arguments
    ///
    /// * `target_dir` - The path to set as the working directory.
    pub fn set_working_dir(&mut self, target_dir: impl AsRef<Path>) -> Result<(), error::Error> {
        let abs_path = self.absolute_path(target_dir.as_ref());

        match std::fs::metadata(&abs_path) {
            Ok(m) => {
                if !m.is_dir() {
                    return Err(error::ErrorKind::NotADirectory(abs_path).into());
                }
            }
            Err(e) => {
                return Err(e.into());
            }
        }

        // Normalize the path (but don't canonicalize it).
        let cleaned_path = abs_path.normalize();

        let pwd = cleaned_path.to_string_lossy().to_string();

        self.env.update_or_add(
            "PWD",
            variables::ShellValueLiteral::Scalar(pwd),
            |_| Ok(()),
            EnvironmentLookup::Anywhere,
            EnvironmentScope::Global,
        )?;
        let oldpwd = std::mem::replace(self.working_dir_mut(), cleaned_path);

        self.env.update_or_add(
            "OLDPWD",
            variables::ShellValueLiteral::Scalar(oldpwd.to_string_lossy().to_string()),
            |_| Ok(()),
            EnvironmentLookup::Anywhere,
            EnvironmentScope::Global,
        )?;

        Ok(())
    }

    /// Tilde-shortens the given string, replacing the user's home directory with a tilde.
    ///
    /// # Arguments
    ///
    /// * `s` - The string to shorten.
    pub fn tilde_shorten(&self, s: String) -> String {
        if let Some(home_dir) = self.home_dir()
            && let Some(stripped) = s.strip_prefix(home_dir.to_string_lossy().as_ref())
        {
            return format!("~{stripped}");
        }
        s
    }

    /// Returns the shell's current home directory, if available.
    pub(crate) fn home_dir(&self) -> Option<PathBuf> {
        if let Some(home) = self.env.get_str("HOME", self) {
            Some(PathBuf::from(home.to_string()))
        } else {
            // HOME isn't set, so let's sort it out ourselves.
            users::get_current_user_home_dir()
        }
    }

    /// Finds executables in the shell's current default PATH, matching the given glob pattern.
    ///
    /// # Arguments
    ///
    /// * `required_glob_pattern` - The glob pattern to match against.
    pub fn find_executables_in_path<'a>(
        &'a self,
        filename: &'a str,
    ) -> impl Iterator<Item = PathBuf> + 'a {
        let path_var = self.env.get_str("PATH", self).unwrap_or_default();
        let paths = crate::sys::fs::split_paths(path_var.as_ref());

        pathsearch::search_for_executable(paths, filename)
    }

    /// Finds executables in the shell's current default PATH, with filenames matching the
    /// given prefix.
    ///
    /// # Arguments
    ///
    /// * `filename_prefix` - The prefix to match against executable filenames.
    pub fn find_executables_in_path_with_prefix(
        &self,
        filename_prefix: &str,
        case_insensitive: bool,
    ) -> impl Iterator<Item = PathBuf> {
        let path_var = self.env.get_str("PATH", self).unwrap_or_default();
        let paths = crate::sys::fs::split_paths(path_var.as_ref());

        pathsearch::search_for_executable_with_prefix(paths, filename_prefix, case_insensitive)
    }

    /// Determines whether the given filename is the name of an executable in one of the
    /// directories in the shell's current PATH. If found, returns the path.
    ///
    /// # Arguments
    ///
    /// * `candidate_name` - The name of the file to look for.
    pub fn find_first_executable_in_path<S: AsRef<str>>(
        &self,
        candidate_name: S,
    ) -> Option<PathBuf> {
        let path = self.env_str("PATH").unwrap_or_default();
        for one_dir in crate::sys::fs::split_paths(path.as_ref()) {
            let candidate_path = one_dir.join(candidate_name.as_ref());
            if candidate_path.executable() {
                return Some(candidate_path);
            }
        }
        None
    }

    /// Uses the shell's hash-based path cache to check whether the given filename is the name
    /// of an executable in one of the directories in the shell's current PATH. If found,
    /// ensures the path is in the cache and returns it.
    ///
    /// # Arguments
    ///
    /// * `candidate_name` - The name of the file to look for.
    pub fn find_first_executable_in_path_using_cache<S: AsRef<str>>(
        &mut self,
        candidate_name: S,
    ) -> Option<PathBuf>
    where
        String: From<S>,
    {
        if let Some(cached_path) = self.program_location_cache.get(&candidate_name) {
            Some(cached_path)
        } else if let Some(found_path) = self.find_first_executable_in_path(&candidate_name) {
            self.program_location_cache
                .set(candidate_name, found_path.clone());
            Some(found_path)
        } else {
            None
        }
    }

    /// Gets the absolute form of the given path.
    ///
    /// # Arguments
    ///
    /// * `path` - The path to get the absolute form of.
    pub fn absolute_path(&self, path: impl AsRef<Path>) -> PathBuf {
        let path = path.as_ref();
        if path.as_os_str().is_empty() || path.is_absolute() {
            path.to_owned()
        } else {
            self.working_dir().join(path)
        }
    }

    /// Opens the given file, using the context of this shell and the provided execution parameters.
    ///
    /// # Arguments
    ///
    /// * `options` - The options to use opening the file.
    /// * `path` - The path to the file to open; may be relative to the shell's working directory.
    /// * `params` - Execution parameters.
    pub(crate) fn open_file(
        &self,
        mode: brush_vfs::OpenMode,
        path: impl AsRef<Path>,
        params: &ExecutionParameters,
    ) -> Result<openfiles::OpenFile, std::io::Error> {
        // Platform special files first, before path resolution. That ordering
        // was previously a hazard -- the Windows check compared trailing path
        // *components*, so a repo containing `dev/null` matched -- but the check
        // now compares the whole path, so running it early is safe and is in
        // fact required: `absolute_path` mangles `/dev/null` on Windows, where
        // it is not an absolute path, before the check would ever see it.
        if let Some(result) = crate::sys::fs::try_open_special_file(path.as_ref()) {
            return result.map(openfiles::OpenFile::from);
        }

        let path_to_open = self.absolute_path(path.as_ref());

        // Synthetic fd paths address the shell's own descriptor table rather
        // than the filesystem, so they are answered before the namespace is
        // consulted at all.
        if let Some(fd_num) = shell_fd_path_to_fd(&path_to_open)
            && let Some(open_file) = params.try_fd(self, fd_num)
        {
            return Ok(open_file);
        }

        let virtual_path = self.to_virtual_path(&path_to_open)?;
        self.session()
            .vfs()
            .open_with(&virtual_path, mode)
            .map(openfiles::OpenFile::from)
    }

    /// Interprets a host-shaped absolute path as a virtual one.
    ///
    /// Transitional. The shell still carries its working directory as a host
    /// `PathBuf`, so paths reaching the vfs are host-shaped and need
    /// translating. Under the identity policy the two coincide on Unix; on
    /// Windows the drive prefix is dropped, since the virtual grammar has no
    /// syntax for one. This disappears once the working directory is itself
    /// virtual.
    pub(crate) fn to_virtual_path(
        &self,
        path: &Path,
    ) -> Result<brush_vfs::VirtualPath, std::io::Error> {
        let text = path.to_string_lossy().replace('\\', "/");
        let text = match text.split_once(":/") {
            Some((prefix, rest)) if prefix.len() == 1 => format!("/{rest}"),
            _ => text,
        };

        self.session()
            .resolve(&text)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e.to_string()))
    }

    /// Replaces the shell's currently configured open files with the given set.
    /// Typically only used by exec-like builtins.
    ///
    /// # Arguments
    ///
    /// * `open_files` - The new set of open files to use.
    pub fn replace_open_files(
        &mut self,
        open_fds: impl Iterator<Item = (ShellFd, openfiles::OpenFile)>,
    ) {
        self.open_files = openfiles::OpenFiles::from(open_fds);
    }

    pub(crate) const fn persistent_open_files(&self) -> &openfiles::OpenFiles {
        &self.open_files
    }
}

fn shell_fd_path_to_fd(path: &Path) -> Option<ShellFd> {
    match path.to_str()? {
        "/dev/stdin" => return Some(openfiles::OpenFiles::STDIN_FD),
        "/dev/stdout" => return Some(openfiles::OpenFiles::STDOUT_FD),
        "/dev/stderr" => return Some(openfiles::OpenFiles::STDERR_FD),
        _ => {}
    }

    if let Some(parent) = path.parent()
        && parent == Path::new("/dev/fd")
        && let Some(filename) = path.file_name()
    {
        filename.to_string_lossy().parse::<ShellFd>().ok()
    } else {
        None
    }
}
