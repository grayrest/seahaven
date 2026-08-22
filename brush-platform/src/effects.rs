//! The hosted-effect trait, and the value types that cross it.

use crate::error::PlatformError;
use crate::facts::PlatformTarget;

/// The result every hosted effect returns.
pub type Effect<T> = Result<T, PlatformError>;

/// What a path names, as `basic-cli`'s `Path.type!` reports it.
///
/// `SymLink` is why the underlying probe does *not* follow links: a follow
/// would resolve a link to its target and this variant would never appear,
/// which is a lie the type would not catch. rocjust branches on it, so the
/// probe is `symlink_metadata`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathKind {
    /// A regular file.
    File,
    /// A directory.
    Dir,
    /// A symbolic link (the link itself, un-followed).
    SymLink,
    /// A device, socket, FIFO — anything with no more specific kind.
    Other,
}

/// The effects a Roc application performs, expressed once (D18).
///
/// This is the subset `rocjust` reaches, built in the order the plan names:
/// the filesystem effects first, because they are where four milestones of
/// sandbox work were spent and where the confinement differential has the most
/// to check. Env, stdio, process and signal effects extend this trait in later
/// steps; the trait is the seam, not the whole surface yet.
///
/// # The methods take `&self`
///
/// The filesystem effects do not mutate the session: the working directory is
/// read to resolve a relative path and never written, and the namespace itself
/// is behind an `Arc`. `set_cwd!` — the one effect that moves the session — is a
/// later step and will take `&mut self`, which is why this trait is written to
/// be extended rather than sealed.
///
/// # Paths
///
/// Every `path` is a `&str` in the virtual namespace (D45), interpreted against
/// the session's working directory exactly as a shell path is. A path the
/// grammar rejects, or one that resolves outside the grant, is
/// [`PlatformError::NotFound`] — unnameable, however it came to be so.
pub trait PlatformEffects {
    /// Reads a file's contents as UTF-8.
    ///
    /// `basic-cli`'s `file_read_utf8!`. Invalid UTF-8 in the file is
    /// [`PlatformError::Other`] rather than a lossy transliteration — the same
    /// stance D45 takes for names.
    fn file_read_utf8(&self, path: &str) -> Effect<String>;

    /// Reads a file's contents as bytes.
    ///
    /// `basic-cli`'s `file_read_bytes!`. Bytes have no encoding to reject.
    fn file_read_bytes(&self, path: &str) -> Effect<Vec<u8>>;

    /// Writes UTF-8 contents to a file, replacing what was there.
    ///
    /// `basic-cli`'s `file_write_utf8!`.
    fn file_write_utf8(&self, path: &str, contents: &str) -> Effect<()>;

    /// Reports what a path names, without following a final symlink.
    ///
    /// `basic-cli`'s `path_type!`, and the funnel `Path.is_file!`,
    /// `Path.is_dir!` and `Path.exists!` all reach.
    fn path_type(&self, path: &str) -> Effect<PathKind>;

    /// Creates a directory and any missing parents.
    ///
    /// `basic-cli`'s `dir_create_all!`.
    fn dir_create_all(&self, path: &str) -> Effect<()>;

    /// Lists a directory, returning the full virtual path of each entry.
    ///
    /// `basic-cli`'s `dir_list!`, which returns paths rather than bare names.
    /// An entry whose host name is not valid UTF-8 is omitted (D45): it is not
    /// a path here, so the listing cannot name it.
    fn dir_list(&self, path: &str) -> Effect<Vec<String>>;

    /// Deletes a file.
    ///
    /// `basic-cli`'s `file_delete!`.
    fn file_delete(&self, path: &str) -> Effect<()>;

    /// Resolves a path to a canonical virtual path.
    ///
    /// `basic-cli`'s `path_canonicalize!`. The result is a *virtual* path — the
    /// vfs never hands back a host path — so this discloses nothing about the
    /// host even though canonicalization is where a naive implementation would.
    fn path_canonicalize(&self, path: &str) -> Effect<String>;

    /// Sets or clears a file's executable bit.
    ///
    /// `basic-cli`'s `file_set_executable!`.
    fn file_set_executable(&self, path: &str, executable: bool) -> Effect<()>;

    /// Whether a path is executable.
    ///
    /// `basic-cli`'s `file_is_executable!`.
    fn file_is_executable(&self, path: &str) -> Effect<bool>;

    /// Removes an empty directory.
    ///
    /// `basic-cli`'s `dir_delete_empty!`.
    fn dir_delete_empty(&self, path: &str) -> Effect<()>;

    /// Removes a directory and everything under it.
    ///
    /// `basic-cli`'s `dir_delete_all!`.
    fn dir_delete_all(&self, path: &str) -> Effect<()>;

    // --- Environment (D21, D15, D22, D30) ---------------------------------

    /// A single environment variable, or `None` when it is not set.
    ///
    /// `basic-cli`'s `env_var!` (whose `VarNotFound` the binding maps from
    /// `None`). The environment is D21's synthesized and passthrough classes,
    /// reduced by the launcher before the session was built — so a host secret
    /// the guest never granted is simply not here to read.
    fn env_var(&self, name: &str) -> Option<String>;

    /// The whole environment, as name/value pairs.
    ///
    /// `basic-cli`'s `env_dict!`. Reads the *same* reduced set `env_var` does —
    /// the property `host_leak_tests` pins at the shell level, that a denied
    /// variable is absent from `dict!` and not merely from `var!`.
    fn env_dict(&self) -> Vec<(String, String)>;

    /// The session's working directory, as a **virtual** path (D15).
    ///
    /// `basic-cli`'s `env_cwd!`. It is the session's cwd, never the host
    /// process's, and it is a virtual path — a host path here would be the D6
    /// leak the launcher milestone spent itself closing.
    fn env_cwd(&self) -> String;

    /// Changes the session's working directory. Does **not** move the host
    /// process (D15).
    ///
    /// `basic-cli`'s `env_set_cwd!`. The one env effect that mutates, which is
    /// why it takes `&mut self`.
    fn env_set_cwd(&mut self, path: &str) -> Effect<()>;

    /// The virtual path temporary files belong under.
    ///
    /// `basic-cli`'s `env_temp_dir!`. Synthesized policy, not read from the
    /// host: a bare namespace has no temp directory, and a launcher granting one
    /// names it.
    fn env_temp_dir(&self) -> String;

    /// The virtual path of the running executable — `/bin/just` (D30).
    ///
    /// `basic-cli`'s `env_exe_path!`. See [`EXE_PATH`](crate::facts::EXE_PATH):
    /// the name the closed world re-invokes through, correct before D22 builds
    /// the directory it names.
    fn env_exe_path(&self) -> String;

    /// The platform the guest believes it is on — **declared, not the machine**.
    ///
    /// `basic-cli`'s `env_platform!`. See [`PlatformTarget`].
    fn env_platform(&self) -> PlatformTarget;

    /// This session's process id — session-scoped, not the host's.
    ///
    /// `basic-cli`'s `env_pid!`.
    fn env_pid(&self) -> i64;

    /// The parallelism the guest may assume — the job limit, not the host's
    /// core count.
    ///
    /// `basic-cli`'s `env_num_cpus!`.
    fn env_num_cpus(&self) -> i64;
}
