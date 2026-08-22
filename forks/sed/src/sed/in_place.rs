// Support for in-place editing
//
// SPDX-License-Identifier: MIT
// Copyright (c) 2025 Diomidis Spinellis
//
// This file is part of the uutils sed package.
// It is licensed under the MIT License.
// For the full copyright and license information, please view the LICENSE
// file that was distributed with this source code.

use std::fs;
use std::io::stdout;
use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use uucore::display::Quotable;
use uucore::error::{FromIo, UIoError, UResult, USimpleError};

use crate::sed::command::ProcessingContext;
use crate::sed::fast_io::OutputBuffer;

/// SEAHAVEN DIVERGENCE: `sed -i`'s replacement file, inside the namespace.
///
/// This is *not* the scratch space D38 places outside the namespace. The
/// replacement is created next to the file being edited and renamed over it, so
/// it is namespace content from the moment it exists. `NamedTempFile::new_in`
/// creates through `std::fs`, so under a mount `sed -i` built the replacement on
/// the host and then renamed a file that had never been in the mount.
///
/// The name is derived rather than random, because randomness is not available:
/// the sandbox's `/dev` is synthetic (D20) and carries `null` and `fd`, not
/// `urandom`. `O_EXCL` -- `with_create_new` -- is what makes a derived name
/// safe, and is the real protection either way: it refuses an existing entry and
/// refuses to follow a symlink planted at the path, which is the attack a random
/// name defends against. The counter makes a second `sed` in the same process
/// and directory pick a different name rather than collide.
struct NamespacedTempFile {
    path: PathBuf,
    persisted: bool,
}

impl NamespacedTempFile {
    /// Creates a uniquely-named file in `dir`, through the namespace.
    fn new_in(dir: &Path) -> std::io::Result<Self> {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);

        let pid = std::process::id();
        for _ in 0..64 {
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = dir.join(format!(".sed{pid}-{n}.tmp"));
            match brush_vfs::ambient::open_with(
                &path,
                brush_vfs::OpenMode::write().with_create_new(true),
            ) {
                Ok(_) => {
                    return Ok(Self {
                        path,
                        persisted: false,
                    });
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e),
            }
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "could not allocate a unique temporary name in the destination directory",
        ))
    }

    fn path(&self) -> &Path {
        &self.path
    }

    /// A second handle on the same file, as `NamedTempFile::reopen` gives.
    fn reopen(&self) -> std::io::Result<fs::File> {
        brush_vfs::ambient::open_with(
            &self.path,
            brush_vfs::OpenMode::write().with_truncate(false),
        )
    }

    /// Renames the temporary over `dest`, as `NamedTempFile::persist` does.
    fn persist(mut self, dest: &Path) -> std::io::Result<()> {
        brush_vfs::ambient::rename(&self.path, dest)?;
        self.persisted = true;
        Ok(())
    }
}

impl Drop for NamespacedTempFile {
    fn drop(&mut self) {
        if !self.persisted {
            // Best-effort, as `NamedTempFile`'s own drop is: a temporary that
            // cannot be removed is not worth failing an edit that succeeded.
            let _ = brush_vfs::ambient::remove_file(&self.path);
        }
    }
}

/// Context for in-place editing
pub struct InPlace {
    pub output: OutputBuffer,
    pub in_place: bool,
    pub in_place_suffix: Option<String>,
    pub follow_symlinks: bool,
    temp_file: Option<NamespacedTempFile>,
    pub original_path: Option<PathBuf>,
}

impl InPlace {
    /// Create an in-place editing engine based on ProcessingContext.
    /// Depending on its settings it may or may not perform in-place
    /// editing, backup the original file, or follow symlinks.
    pub fn new(context: ProcessingContext) -> Self {
        Self {
            output: OutputBuffer::new(Box::new(stdout())),
            in_place: context.in_place,
            in_place_suffix: context.in_place_suffix,
            follow_symlinks: context.follow_symlinks,
            temp_file: None,
            original_path: None,
        }
    }

    /// Return an OutputBuffer for outputting the edits to the specified file.
    /// The file may be a symbolic link, which will be processed according
    /// to the context specification.
    pub fn begin(&mut self, file_name: &Path) -> UResult<&mut OutputBuffer> {
        let resolved = if self.follow_symlinks {
            brush_vfs::ambient::canonicalize(file_name)
                .map_err_context(|| format!("resolving symlink {}", file_name.quote()))?
        } else {
            file_name.to_path_buf()
        };
        self.begin_resolved(&resolved)
    }

    /// Return an OutputBuffer for outputting the edits to the specified file.
    /// The passed file name should have resolved symbolic links according
    /// to the context settings.
    fn begin_resolved(&mut self, file_name: &Path) -> UResult<&mut OutputBuffer> {
        if !self.in_place {
            self.output = OutputBuffer::new(Box::new(stdout()));
            return Ok(&mut self.output);
        }

        let metadata = brush_vfs::ambient::metadata(file_name).map_err_context(|| {
            format!(
                "error Reading metadata of {} for in-place edit",
                file_name.quote()
            )
        })?;

        if !metadata.is_file() {
            return Err(USimpleError::new(
                2,
                format!(
                    "cannot in-place edit non-regular file {}",
                    file_name.quote()
                ),
            ));
        }

        let dir = file_name.parent().unwrap_or_else(|| Path::new("."));
        let temp_file = NamespacedTempFile::new_in(dir)
            .map_err_context(|| format!("error creating temporary file in {}", dir.quote()))?;

        // TODO: On Unix use fchown(metadata.{uid,dig}) and fchmod(mode)
        // on let fd = temp_file.as_file().as_raw_fd() when uucore::libc
        // support them.
        #[cfg(unix)]
        {
            let mode = metadata.mode() & 0o7777;
            let perms = fs::Permissions::from_mode(mode);
            brush_vfs::ambient::set_permissions(temp_file.path(), perms)?;
        }

        let output = OutputBuffer::new(Box::new(
            temp_file.reopen().expect("reopening the temporary file"),
        ));
        self.output = output;
        self.temp_file = Some(temp_file);
        self.original_path = Some(file_name.to_path_buf());

        Ok(&mut self.output)
    }

    /// Finish (potentially in-place) editing.
    pub fn end(&mut self) -> UResult<()> {
        self.output.flush()?;

        if !self.in_place {
            return Ok(());
        }

        let orig = self.original_path.take().expect("original_path unset");
        let temp = self.temp_file.take().expect("temp_file unset");

        // Backup original if suffix is provided
        if let Some(ref suffix) = self.in_place_suffix {
            let mut backup_path = orig.clone();
            let file_name = backup_path
                .file_name()
                .expect("Missing file name for backup")
                .to_os_string();
            let mut backup_name = file_name;
            backup_name.push(suffix);
            backup_path.set_file_name(backup_name);

            #[cfg(windows)]
            // Try to remove to ensure the rename won't fail on Windows.
            let _ = brush_vfs::ambient::remove_file(&backup_path);

            brush_vfs::ambient::rename(&orig, &backup_path).map_err_context(|| {
                format!(
                    "error backing up {} to {}",
                    orig.quote(),
                    backup_path.quote()
                )
            })?;
        } else {
            #[cfg(windows)]
            // On Windows delete the original file for temp.persist to work
            if brush_vfs::ambient::exists(&(orig)) {
                brush_vfs::ambient::remove_file(&orig).map_err_context(|| {
                    format!("error removing original input file {}", orig.quote())
                })?;
            }
        }

        // Atomically replace the original
        let temp_path = temp.path().to_path_buf();
        if let Err(e) = temp.persist(&orig) {
            return Err(UIoError::new(
                e.kind(),
                format!(
                    "error persisting temporary file {} to {}",
                    temp_path.quote(),
                    orig.quote()
                ),
            ));
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert_fs::TempDir;
    use assert_fs::fixture::PathChild;
    use std::fs;
    use std::io::{Read, Write};
    use std::path::Path;

    fn minimal_context() -> ProcessingContext {
        ProcessingContext {
            in_place: false,
            in_place_suffix: None,
            follow_symlinks: false,
            // fill in default values for the rest as needed
            ..Default::default()
        }
    }

    fn write_original(file: &Path, content: &str) {
        fs::write(file, content).unwrap();
    }

    fn read_file(file: &Path) -> String {
        let mut contents = String::new();
        fs::File::open(file)
            .unwrap()
            .read_to_string(&mut contents)
            .unwrap();
        contents
    }

    #[test]
    fn test_in_place_editing() {
        let temp = TempDir::new().unwrap();
        let file = temp.child("file.txt");
        write_original(file.path(), "original\n");

        let mut ctx = minimal_context();
        ctx.in_place = true;

        let mut inplace = InPlace::new(ctx);
        let buf = inplace.begin(file.path()).unwrap();
        writeln!(buf, "updated").unwrap();
        inplace.end().unwrap();

        assert_eq!(read_file(file.path()), "updated\n");
    }

    #[test]
    fn test_in_place_backup() {
        let temp = TempDir::new().unwrap();
        let file = temp.child("file.txt");
        let backup = temp.child("file.txt.bak");
        write_original(file.path(), "original\n");

        let mut ctx = minimal_context();
        ctx.in_place = true;
        ctx.in_place_suffix = Some(".bak".to_string());

        let mut inplace = InPlace::new(ctx);
        let buf = inplace.begin(file.path()).unwrap();
        writeln!(buf, "new content").unwrap();
        inplace.end().unwrap();

        assert_eq!(read_file(file.path()), "new content\n");
        assert_eq!(read_file(backup.path()), "original\n");
    }

    #[cfg(unix)]
    #[test]
    fn test_symlink_follow_true() {
        let temp = TempDir::new().unwrap();
        let real = temp.child("target.txt");
        let link = temp.child("link.txt");

        write_original(real.path(), "real\n");
        std::os::unix::fs::symlink(real.path(), link.path()).unwrap();

        let mut ctx = minimal_context();
        ctx.in_place = true;
        ctx.follow_symlinks = true;

        let mut inplace = InPlace::new(ctx);
        let buf = inplace.begin(link.path()).unwrap();
        writeln!(buf, "changed").unwrap();
        inplace.end().unwrap();

        assert_eq!(read_file(real.path()), "changed\n");
        assert!(link.path().exists()); // Symlink still exists
    }

    #[cfg(unix)]
    #[test]
    fn test_symlink_follow_false() {
        let temp = TempDir::new().unwrap();
        let real = temp.child("target.txt");
        let link = temp.child("link.txt");

        write_original(real.path(), "real\n");
        std::os::unix::fs::symlink(real.path(), link.path()).unwrap();

        let mut ctx = minimal_context();
        ctx.in_place = true;
        ctx.follow_symlinks = false;

        let mut inplace = InPlace::new(ctx);
        let buf = inplace.begin(link.path()).unwrap();
        writeln!(buf, "linked").unwrap();
        inplace.end().unwrap();

        // real file should remain untouched
        assert_eq!(read_file(real.path()), "real\n");

        // link (symlink path) now contains the new content
        let contents = read_file(link.path());
        assert_eq!(contents, "linked\n");
    }

    #[test]
    fn test_no_in_place_outputs_to_stdout() {
        let mut ctx = minimal_context();
        ctx.in_place = false;

        let mut inplace = InPlace::new(ctx);
        let _buf = inplace.begin(Path::new("fake.txt")).unwrap();
        assert!(inplace.end().is_ok());
    }
}
