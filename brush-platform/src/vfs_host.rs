//! The one implementation of [`PlatformEffects`] that exists: over the vfs.

use std::io::{Read as _, Write as _};

use brush_vfs::{AccessModes, Session, VirtualPath};

use crate::effects::{Effect, PathKind, PlatformEffects};
use crate::error::PlatformError;
use crate::facts::{EXE_PATH, PlatformTarget, SessionFacts};

/// A host that routes every effect through a [`brush_vfs::Session`].
///
/// This is the native tier's binding (D17): the effect is a direct method call,
/// and confinement comes entirely from the session's namespace. The wasm tier's
/// binding, when it exists, is a different implementor of the same trait over
/// the same session — which is what D19's "identical capability" will mean once
/// it is a fact the compiler checks rather than a sentence.
pub struct VfsPlatform {
    session: Session,
    facts: SessionFacts,
}

impl VfsPlatform {
    /// Wraps a session and its policy-chosen facts as a platform host.
    #[must_use]
    pub const fn new(session: Session, facts: SessionFacts) -> Self {
        Self { session, facts }
    }

    /// The session this host resolves against.
    #[must_use]
    pub const fn session(&self) -> &Session {
        &self.session
    }

    /// Resolves a script-written path against the session's working directory.
    ///
    /// A path the grammar rejects is [`PlatformError::NotFound`], not a distinct
    /// error: D45 makes it unnameable, and an unnameable path is not-found from
    /// inside the namespace regardless of *why* it cannot be named. This is the
    /// single choke point through which a string becomes something the
    /// filesystem acts on, so the rule holds for every effect below.
    fn resolve(&self, path: &str) -> Effect<VirtualPath> {
        self.session
            .resolve(path)
            .map_err(|_| PlatformError::NotFound)
    }

    /// The facts about a path, or `NotFound` when it names nothing here.
    fn probe(&self, path: &str, follow: bool) -> Effect<brush_vfs::FileFacts> {
        let vp = self.resolve(path)?;
        self.session
            .vfs()
            .facts(&vp, follow)
            .ok_or(PlatformError::NotFound)
    }
}

impl PlatformEffects for VfsPlatform {
    fn file_read_utf8(&self, path: &str) -> Effect<String> {
        let bytes = self.file_read_bytes(path)?;
        // Invalid UTF-8 is refused, not transliterated -- D45's stance for
        // names, applied to contents for the same reason: a lossy `Str` is a
        // value the guest did not read.
        String::from_utf8(bytes)
            .map_err(|_| PlatformError::Other("file is not valid UTF-8".to_owned()))
    }

    fn file_read_bytes(&self, path: &str) -> Effect<Vec<u8>> {
        let vp = self.resolve(path)?;
        let mut file = self.session.vfs().open(&vp).map_err(|e| io(&e))?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).map_err(|e| io(&e))?;
        Ok(bytes)
    }

    fn file_write_utf8(&self, path: &str, contents: &str) -> Effect<()> {
        let vp = self.resolve(path)?;
        let mut file = self.session.vfs().create(&vp).map_err(|e| io(&e))?;
        file.write_all(contents.as_bytes()).map_err(|e| io(&e))?;
        Ok(())
    }

    fn path_type(&self, path: &str) -> Effect<PathKind> {
        let facts = self.probe(path, false)?;
        // Order matters: a symlink is also not a dir or file here (no-follow),
        // so it is checked first, then dir, then file, then everything else.
        Ok(if facts.is_symlink {
            PathKind::SymLink
        } else if facts.is_dir {
            PathKind::Dir
        } else if facts.is_file {
            PathKind::File
        } else {
            PathKind::Other
        })
    }

    fn dir_create_all(&self, path: &str) -> Effect<()> {
        let vp = self.resolve(path)?;
        self.session.vfs().create_dir_all(&vp).map_err(|e| io(&e))
    }

    fn dir_list(&self, path: &str) -> Effect<Vec<String>> {
        let vp = self.resolve(path)?;
        let names = self.session.vfs().read_dir_names(&vp).map_err(|e| io(&e))?;
        // `dir_list!` returns paths, so each bare name is joined back onto the
        // directory's virtual path. `read_dir_names` has already dropped any
        // entry whose host name is not valid UTF-8 (D45), so every name here
        // resolves.
        let mut paths = Vec::with_capacity(names.len());
        for name in names {
            let child = vp.resolve(&name).map_err(|_| PlatformError::NotFound)?;
            paths.push(child.as_str().to_owned());
        }
        Ok(paths)
    }

    fn file_delete(&self, path: &str) -> Effect<()> {
        let vp = self.resolve(path)?;
        self.session.vfs().remove_file(&vp).map_err(|e| io(&e))
    }

    fn path_canonicalize(&self, path: &str) -> Effect<String> {
        let vp = self.resolve(path)?;
        // The result is a virtual path. The vfs canonicalizes *within* the
        // namespace and never returns a host path, which is why this effect --
        // the one place a naive host leaks its layout -- discloses nothing.
        let canonical = self.session.vfs().canonicalize(&vp).map_err(|e| io(&e))?;
        Ok(canonical.as_str().to_owned())
    }

    fn file_set_executable(&self, path: &str, executable: bool) -> Effect<()> {
        let vp = self.resolve(path)?;
        set_executable(&self.session, &vp, executable)
    }

    fn file_is_executable(&self, path: &str) -> Effect<bool> {
        // A missing path is an error, not `false`: `basic-cli` distinguishes
        // "not executable" from "not there", and so does rocjust when it
        // branches on the result. The probe supplies the distinction.
        let _ = self.probe(path, true)?;
        let vp = self.resolve(path)?;
        Ok(self.session.vfs().access(
            &vp,
            AccessModes {
                readable: false,
                writable: false,
                executable: true,
            },
        ))
    }

    fn dir_delete_empty(&self, path: &str) -> Effect<()> {
        let vp = self.resolve(path)?;
        self.session.vfs().remove_dir(&vp).map_err(|e| io(&e))
    }

    fn dir_delete_all(&self, path: &str) -> Effect<()> {
        let vp = self.resolve(path)?;
        self.session.vfs().remove_dir_all(&vp).map_err(|e| io(&e))
    }

    fn env_var(&self, name: &str) -> Option<String> {
        self.facts.env.get(name).cloned()
    }

    fn env_dict(&self) -> Vec<(String, String)> {
        self.facts
            .env
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    fn env_cwd(&self) -> String {
        // The session's cwd, which is a virtual path by construction -- a
        // `VirtualPath` cannot hold a host path. So this is D6-clean without an
        // extra check: there is no host cwd to accidentally return.
        self.session.cwd().as_str().to_owned()
    }

    fn env_set_cwd(&mut self, path: &str) -> Effect<()> {
        // Moves the *session*, not the process. `Session::set_cwd` never touches
        // `std::env::set_current_dir`; the host process stays where it was,
        // which is what D15 requires and what gate 5 checks.
        self.session.set_cwd(path).map_err(|e| io(&e))
    }

    fn env_temp_dir(&self) -> String {
        self.facts.temp_dir.clone()
    }

    fn env_exe_path(&self) -> String {
        EXE_PATH.to_owned()
    }

    fn env_platform(&self) -> PlatformTarget {
        self.facts.platform.clone()
    }

    fn env_pid(&self) -> i64 {
        self.facts.pid
    }

    fn env_num_cpus(&self) -> i64 {
        self.facts.num_cpus
    }
}

/// Maps a vfs `io::Error` onto the wire type. Named for brevity at call sites.
fn io(error: &std::io::Error) -> PlatformError {
    PlatformError::from_io(error)
}

/// Sets or clears the executable bit, reading the current mode first.
///
/// Unix-only in substance: the executable bit is a Unix concept, and there is
/// no portable "make this runnable". On other platforms the effect is
/// unsupported rather than silently a no-op, so a caller relying on it hears so.
#[cfg(unix)]
fn set_executable(session: &Session, vp: &VirtualPath, executable: bool) -> Effect<()> {
    use std::os::unix::fs::PermissionsExt as _;

    // The three execute bits, owner/group/other, together.
    const EXEC_BITS: u32 = 0o111;

    let metadata = session.vfs().metadata(vp).map_err(|e| io(&e))?;
    let mut perm = metadata.permissions();
    let mode = perm.mode();
    let updated = if executable {
        mode | EXEC_BITS
    } else {
        mode & !EXEC_BITS
    };
    perm.set_mode(updated);
    session.vfs().set_permissions(vp, &perm).map_err(|e| io(&e))
}

#[cfg(not(unix))]
fn set_executable(_session: &Session, _vp: &VirtualPath, _executable: bool) -> Effect<()> {
    Err(PlatformError::Unsupported)
}
