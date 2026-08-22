//! The one implementation of [`PlatformEffects`] that exists: over the vfs.

use std::io::{Read as _, Write as _};

use brush_vfs::{AccessModes, Session, VirtualPath};

use crate::cmd::{
    Cmd, Executor, Exit, Finished, IoPlan, JobHandle, NO_EXECUTOR, OutputMode, RunResult, StdinMode,
};
use crate::effects::{Effect, ExecOutput, PathKind, PlatformEffects};
use crate::error::PlatformError;
use crate::facts::{EXE_PATH, PlatformTarget, SessionFacts};
use crate::runtime::{Clock, Rng, SignalQueue, SystemClock, SystemRng};
use crate::stdio::{Stdio, Stream};

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
    stdio: Stdio,
    /// The execution backend, or `None` before the link step wires the broker
    /// in. Absent means every execution fails with `Unsupported`, uniformly --
    /// including the `exec_*` effects, because they route through `spawn`.
    executor: Option<Box<dyn Executor>>,
    /// Finished jobs awaiting `wait_any`, keyed by handle id.
    jobs: std::collections::BTreeMap<u64, Finished>,
    /// The next handle id. Monotonic, never reused -- see `JobHandle`.
    next_handle: u64,
    /// Signals the guest has caught but not read (D36).
    signals: SignalQueue,
    /// The session's clock (D15); the default reads real time.
    clock: Box<dyn Clock>,
    /// The session's RNG (D15); the default is real entropy.
    rng: Box<dyn Rng>,
}

impl VfsPlatform {
    /// Wraps a session and its policy-chosen facts as a platform host.
    ///
    /// stdin is empty and the clock and RNG are the real ones; use the
    /// `with_*` builders to change any of that. Not `const`: the default clock
    /// and RNG are boxed.
    #[must_use]
    pub fn new(session: Session, facts: SessionFacts) -> Self {
        Self {
            session,
            facts,
            stdio: Stdio::new(Vec::new()),
            executor: None,
            jobs: std::collections::BTreeMap::new(),
            next_handle: 0,
            signals: SignalQueue::new(),
            clock: Box::new(SystemClock),
            rng: Box::new(SystemRng),
        }
    }

    /// Replaces the session clock — D14's hermetic mode pins time this way.
    #[must_use]
    pub fn with_clock(mut self, clock: Box<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }

    /// Replaces the session RNG — D14's hermetic mode seeds it this way.
    #[must_use]
    pub fn with_rng(mut self, rng: Box<dyn Rng>) -> Self {
        self.rng = rng;
        self
    }

    /// Delivers a signal into the guest's queue (the sandbox's path, not the
    /// host's). Dropped if the guest has not installed a handler.
    pub fn deliver_signal(&mut self, sig: i64) {
        self.signals.deliver(sig);
    }

    /// Supplies the bytes the job reads from stdin.
    #[must_use]
    pub fn with_stdin(mut self, stdin: Vec<u8>) -> Self {
        self.stdio = Stdio::new(stdin);
        self
    }

    /// Installs the execution backend (D2's predicate and D24's broker, at the
    /// link step). Without one, every execution is `Unsupported`.
    #[must_use]
    pub fn with_executor(mut self, executor: Box<dyn Executor>) -> Self {
        self.executor = Some(executor);
        self
    }

    /// Mints the next non-aliasing job handle.
    const fn mint_handle(&mut self) -> JobHandle {
        let id = self.next_handle;
        self.next_handle += 1;
        JobHandle(id)
    }

    /// Runs a capturing command through `spawn`/`wait_any`, building the
    /// `ExecOutput` the two `exec_output` effects return. Shared so both stdin
    /// modes take the identical path.
    fn exec_capturing(&mut self, cmd: &Cmd, stdin: StdinMode) -> Effect<ExecOutput> {
        let handle = self.spawn(cmd, IoPlan::capture(stdin))?;
        let (_h, finished) = self.wait_any(&[handle])?;
        let (stdout, stderr) = finished.captured.unwrap_or_default();
        Ok(match finished.exit {
            Exit::Code(0) => ExecOutput::Success { stdout, stderr },
            Exit::Code(exit_code) => ExecOutput::NonZeroExit {
                exit_code,
                stdout,
                stderr,
            },
            // A signal death is a non-zero end; report it as the shell does,
            // 128 + signal, so the guest sees a non-zero code rather than a
            // spurious success.
            Exit::Signal(signal) => ExecOutput::NonZeroExit {
                exit_code: 128 + signal,
                stdout,
                stderr,
            },
        })
    }

    /// The session this host resolves against.
    #[must_use]
    pub const fn session(&self) -> &Session {
        &self.session
    }

    /// The job's captured output (D28), for the runtime to drain and render.
    ///
    /// Not on the effect trait: the guest writes, the *host* reads. That
    /// asymmetry is the buffering being undefeatable -- there is no guest path
    /// to the bytes once written.
    #[must_use]
    pub const fn output(&self) -> &crate::stdio::OutputLog {
        &self.stdio.output
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

    fn dir_create(&self, path: &str) -> Effect<()> {
        let vp = self.resolve(path)?;
        self.session.vfs().create_dir(&vp).map_err(|e| io(&e))
    }

    fn file_write_bytes(&self, path: &str, bytes: &[u8]) -> Effect<()> {
        let vp = self.resolve(path)?;
        let mut file = self.session.vfs().create(&vp).map_err(|e| io(&e))?;
        file.write_all(bytes).map_err(|e| io(&e))?;
        Ok(())
    }

    fn file_size_in_bytes(&self, path: &str) -> Effect<u64> {
        let vp = self.resolve(path)?;
        Ok(self.session.vfs().metadata(&vp).map_err(|e| io(&e))?.len())
    }

    fn file_is_readable(&self, path: &str) -> Effect<bool> {
        // A missing path is an error, not `false` -- the is_executable stance,
        // for the same reason: rocjust branches on "not there" separately.
        let _ = self.probe(path, true)?;
        let vp = self.resolve(path)?;
        Ok(self.session.vfs().access(
            &vp,
            AccessModes {
                readable: true,
                writable: false,
                executable: false,
            },
        ))
    }

    fn file_is_writable(&self, path: &str) -> Effect<bool> {
        let _ = self.probe(path, true)?;
        let vp = self.resolve(path)?;
        Ok(self.session.vfs().access(
            &vp,
            AccessModes {
                readable: false,
                writable: true,
                executable: false,
            },
        ))
    }

    fn file_time_accessed(&self, _path: &str) -> Effect<u128> {
        // D46, deferred: a host timestamp is a host-identity leak, and no
        // synthesized answer is designed yet, so the effect is absent.
        Err(PlatformError::Unsupported)
    }

    fn file_time_modified(&self, _path: &str) -> Effect<u128> {
        Err(PlatformError::Unsupported)
    }

    fn file_time_created(&self, _path: &str) -> Effect<u128> {
        Err(PlatformError::Unsupported)
    }

    fn file_hard_link(&self, original: &str, link: &str) -> Effect<()> {
        let original = self.resolve(original)?;
        let link = self.resolve(link)?;
        self.session
            .vfs()
            .hard_link(&original, &link)
            .map_err(|e| io(&e))
    }

    fn file_rename(&self, from: &str, to: &str) -> Effect<()> {
        let from = self.resolve(from)?;
        let to = self.resolve(to)?;
        self.session.vfs().rename(&from, &to).map_err(|e| io(&e))
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

    fn stdout_write(&mut self, text: &str) -> Effect<()> {
        self.stdio.output.write(Stream::Stdout, text.as_bytes());
        Ok(())
    }

    fn stdout_line(&mut self, text: &str) -> Effect<()> {
        self.stdio.output.write(Stream::Stdout, text.as_bytes());
        self.stdio.output.write(Stream::Stdout, b"\n");
        Ok(())
    }

    fn stderr_write(&mut self, text: &str) -> Effect<()> {
        self.stdio.output.write(Stream::Stderr, text.as_bytes());
        Ok(())
    }

    fn stderr_line(&mut self, text: &str) -> Effect<()> {
        self.stdio.output.write(Stream::Stderr, text.as_bytes());
        self.stdio.output.write(Stream::Stderr, b"\n");
        Ok(())
    }

    fn stdin_line(&mut self) -> Effect<Option<String>> {
        Ok(self.stdio.input.read_line())
    }

    fn stdin_read_to_end(&mut self) -> Effect<Vec<u8>> {
        Ok(self.stdio.input.read_to_end())
    }

    fn stdout_write_bytes(&mut self, bytes: &[u8]) -> Effect<()> {
        self.stdio.output.write(Stream::Stdout, bytes);
        Ok(())
    }

    fn stderr_write_bytes(&mut self, bytes: &[u8]) -> Effect<()> {
        self.stdio.output.write(Stream::Stderr, bytes);
        Ok(())
    }

    fn stdin_bytes(&mut self) -> Effect<Option<Vec<u8>>> {
        Ok(self.stdio.input.read_chunk())
    }

    fn spawn(&mut self, cmd: &Cmd, io: IoPlan) -> Effect<JobHandle> {
        // The child's stdin: the session's own, drained, for an inheriting run
        // (just's backticks); empty otherwise. Read first so the executor borrow
        // does not overlap the stdio borrow.
        let stdin = match io.stdin {
            StdinMode::Inherit => self.stdio.input.read_to_end(),
            StdinMode::Null => Vec::new(),
        };

        let result: RunResult = self
            .executor
            .as_mut()
            .ok_or(NO_EXECUTOR)?
            .run(cmd, &stdin)?;

        // Apply the output plan. Inherited output is the child writing where the
        // parent would (D28's log); captured output is held for the guest.
        let captured = match io.output {
            OutputMode::Inherit => {
                self.stdio.output.write(Stream::Stdout, &result.stdout);
                self.stdio.output.write(Stream::Stderr, &result.stderr);
                None
            }
            OutputMode::Capture => Some((result.stdout, result.stderr)),
        };

        let handle = self.mint_handle();
        self.jobs.insert(
            handle.0,
            Finished {
                exit: result.exit,
                captured,
            },
        );
        Ok(handle)
    }

    fn wait_any(&mut self, handles: &[JobHandle]) -> Effect<(JobHandle, Finished)> {
        for handle in handles {
            // Removed on wait: a handle names its job once. With non-aliasing
            // ids, a waited handle then names nothing rather than a later job.
            if let Some(finished) = self.jobs.remove(&handle.0) {
                return Ok((*handle, finished));
            }
        }
        Err(PlatformError::Other(
            "wait_any: no such job (empty list, or already waited)".to_owned(),
        ))
    }

    fn cmd_exec_exit_code(&mut self, cmd: &Cmd) -> Effect<i32> {
        let handle = self.spawn(cmd, IoPlan::inherit())?;
        let (_h, finished) = self.wait_any(&[handle])?;
        match finished.exit {
            Exit::Code(code) => Ok(code),
            // exec_exit_code collapses a signal death into an error, which is
            // basic-cli's documented behaviour: it cannot report which signal.
            Exit::Signal(_) => Err(PlatformError::Other(
                "child terminated by a signal".to_owned(),
            )),
        }
    }

    fn cmd_exec_status(&mut self, cmd: &Cmd) -> Effect<i32> {
        let handle = self.spawn(cmd, IoPlan::inherit())?;
        let (_h, finished) = self.wait_any(&[handle])?;
        Ok(match finished.exit {
            Exit::Code(code) => code,
            // The negated signal, unambiguous because real codes are 0..=255.
            Exit::Signal(signal) => -signal,
        })
    }

    fn cmd_exec_output(&mut self, cmd: &Cmd) -> Effect<ExecOutput> {
        self.exec_capturing(cmd, StdinMode::Null)
    }

    fn cmd_exec_output_inherit_stdin(&mut self, cmd: &Cmd) -> Effect<ExecOutput> {
        self.exec_capturing(cmd, StdinMode::Inherit)
    }

    fn signal_install(&mut self) {
        self.signals.install();
    }

    fn signal_take(&mut self) -> i64 {
        self.signals.take()
    }

    fn utc_now(&self) -> Effect<u128> {
        self.clock
            .now_nanos()
            .ok_or_else(|| PlatformError::Other("clock is before the epoch".to_owned()))
    }

    fn tz_offset_seconds(&self) -> i64 {
        self.clock.tz_offset_seconds()
    }

    fn random_seed_u64(&mut self) -> Effect<u64> {
        Ok(self.rng.next_u64())
    }

    fn random_seed_u32(&mut self) -> Effect<u32> {
        // The low half of the same source, so hermetic mode pins both from one
        // seed rather than two independent streams.
        #[expect(
            clippy::cast_possible_truncation,
            reason = "taking the low 32 bits is the intent"
        )]
        Ok(self.rng.next_u64() as u32)
    }

    fn regex_is_match(&self, pattern: &str, haystack: &str) -> Result<bool, String> {
        let regex = regex::Regex::new(pattern).map_err(|e| e.to_string())?;
        Ok(regex.is_match(haystack))
    }

    fn regex_replace_all(
        &self,
        pattern: &str,
        haystack: &str,
        replacement: &str,
    ) -> Result<String, String> {
        let regex = regex::Regex::new(pattern).map_err(|e| e.to_string())?;
        Ok(regex.replace_all(haystack, replacement).into_owned())
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
