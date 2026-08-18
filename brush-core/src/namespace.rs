//! Asking the shell's namespace about host-shaped paths.
//!
//! The shell still carries paths in the host's shape -- inherited from the
//! environment, written by scripts, joined onto a `PathBuf` working directory
//! -- while the answers about them have to come from the namespace. These are
//! the translations in between, and they are public because completion and
//! highlighting live outside this crate and need the same answers.

use std::path::Path;

/// Converts a host-shaped path into a path in the shell's namespace.
///
/// Transitional: paths reach the shell in whatever shape the host, the
/// environment or the script wrote them, so the separator and any drive
/// prefix are folded away before the virtual path grammar sees them. Once
/// paths are virtual end to end this disappears.
pub fn to_virtual_path(
    session: &brush_vfs::Session,
    path: &Path,
) -> Result<brush_vfs::VirtualPath, std::io::Error> {
    let text = path.to_string_lossy().replace('\\', "/");
    let text = match text.split_once(":/") {
        Some((prefix, rest)) if prefix.len() == 1 => format!("/{rest}"),
        _ => text,
    };

    session
        .resolve(&text)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e.to_string()))
}

/// Returns true if the namespace has an executable file at the given path.
///
/// The name is consulted before the namespace because `access(2)`'s executable
/// mode has no Windows equivalent: cap-primitives maps it to opening the file
/// for reading, so on Windows the namespace alone answers yes for every
/// readable file. On Unix the name says nothing and this is a no-op.
pub fn is_executable(session: &brush_vfs::Session, path: &Path) -> bool {
    if !crate::sys::fs::name_permits_execution(path) {
        return false;
    }
    to_virtual_path(session, path).is_ok_and(|p| {
        session.vfs().access(
            &p,
            brush_vfs::AccessModes {
                readable: false,
                writable: false,
                executable: true,
            },
        )
    })
}

/// Returns true if the namespace has a directory at the given path.
pub fn is_dir(session: &brush_vfs::Session, path: &Path) -> bool {
    to_virtual_path(session, path)
        .ok()
        .and_then(|p| session.vfs().facts(&p, true))
        .is_some_and(|facts| facts.is_dir)
}

/// Returns true if the namespace has anything at the given path.
pub fn exists(session: &brush_vfs::Session, path: &Path) -> bool {
    to_virtual_path(session, path).is_ok_and(|p| session.vfs().exists(&p))
}
