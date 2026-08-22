//! The broker-backed [`Executor`]: the platform's process-execution seam
//! (D2, D24, D25), bound to `brush-core`'s session broker.
//!
//! [`brush_platform`] declares execution as a trait, [`Executor`], and does not
//! itself spawn anything — running a process under a closed world crosses D2's
//! predicate and D24's broker, both of which live in `brush-core`. This crate is
//! that binding. It is where the two halves the plan kept apart finally meet: the
//! platform's `Cmd` on one side, the launcher's broker on the other.
//!
//! # Closed world by construction (D2)
//!
//! [`BrokerExecutor`] never runs `cmd.program` as a host program. It runs *one*
//! program — the trusted launcher, re-invoked as `<launcher> --invoke-bundled
//! <name> <args…>` — and `cmd.program` is the bundled-utility name that
//! re-invocation dispatches. An arbitrary program is therefore not refused by a
//! check that could be forgotten; it is unreachable, because the only `exec` this
//! type performs is of the launcher itself. That is exactly D2's predicate — the
//! launcher's own path *and* the dispatch flag — enforced by leaving no other
//! path to `Command::new`.
//!
//! # The child is confined by the handshake (D24)
//!
//! The dispatched child installs the namespace it is served into
//! [`brush_vfs::ambient`], and the bundled utility's filesystem access flows
//! through that — so a child served this session's mounts is confined to them. If
//! the handshake is expected and fails, the child fails closed; a child that
//! quietly fell back to the host filesystem is the hole D24 exists to close.
//!
//! # Ordering (D24)
//!
//! The credential is the child's pid, which does not exist until the spawn, and
//! the child blocks in `connect` until it is served — so the order is fixed:
//! create the rendezvous, spawn, *then* serve. Serving before collecting output
//! is likewise required: the child runs the utility only once it has its session,
//! so a parent that waited for output first would wait on a child still blocked
//! for its namespace.

use std::io::Write as _;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use brush_platform::{Cmd, Executor, Exit, PlatformError, RunResult};
use brush_vfs::Session;

/// Runs a platform [`Cmd`] as a confined bundled dispatch (D2, D24).
///
/// Holds the session it serves to each child and the launcher it re-invokes.
/// The session is `Arc`-backed, so this is a cheap handle onto the same namespace
/// the rest of the platform resolves against — a child sees exactly what the
/// guest sees.
pub struct BrokerExecutor {
    /// The namespace served to every child (D24).
    session: Session,
    /// The trusted launcher, re-invoked per run (D2). For the native tier this is
    /// the running executable — the Roc app is its own bundled trampoline.
    launcher: PathBuf,
    /// The argument marking a re-invocation as bundled dispatch — `brush-shell`'s
    /// `bundled::DISPATCH_FLAG`, supplied by the caller so this crate does not
    /// depend on the shell and the flag stays single-sourced.
    dispatch_flag: String,
}

impl BrokerExecutor {
    /// Binds an executor to a session, a launcher path, and the dispatch flag.
    ///
    /// `launcher` is the binary re-invoked to run each bundled utility (its own
    /// path, for a self-trampolining binary); `dispatch_flag` is the argument
    /// that marks the re-invocation as dispatch rather than a fresh run.
    #[must_use]
    pub fn new(
        session: Session,
        launcher: impl Into<PathBuf>,
        dispatch_flag: impl Into<String>,
    ) -> Self {
        Self {
            session,
            launcher: launcher.into(),
            dispatch_flag: dispatch_flag.into(),
        }
    }

    /// Binds an executor whose launcher is the running executable.
    ///
    /// The native tier's case: the linked binary (a Roc app plus this host) is
    /// its own bundled trampoline, so it re-invokes *itself* to run a utility.
    ///
    /// # Errors
    ///
    /// Returns [`std::io::Error`] if the running executable's path cannot be
    /// determined.
    pub fn for_current_exe(
        session: Session,
        dispatch_flag: impl Into<String>,
    ) -> std::io::Result<Self> {
        // The launcher is the host's own binary path, which lives *outside* the
        // namespace -- exactly the exemption `TrustedLauncher` and the broker's
        // rendezvous take. It is host scratch the executor re-invokes, never a
        // path the guest resolves, so it is read from the host rather than the
        // vfs (which would report it missing under any restrictive policy).
        #[expect(
            clippy::disallowed_methods,
            reason = "the launcher path is outside the namespace; see D2's TrustedLauncher"
        )]
        let exe = std::env::current_exe()?;
        Ok(Self::new(session, exe, dispatch_flag))
    }
}

impl Executor for BrokerExecutor {
    fn run(&mut self, cmd: &Cmd, stdin: &[u8]) -> Result<RunResult, PlatformError> {
        // Create the rendezvous before the spawn: the child is told its path on
        // the environment and blocks connecting to it, so it must exist first.
        let rendezvous = brush_core::broker::Rendezvous::create().map_err(|e| broker_failed(&e))?;

        let mut command = Command::new(&self.launcher);
        command.arg(&self.dispatch_flag).arg(&cmd.program);
        command.args(&cmd.args);
        if cmd.clear_envs {
            command.env_clear();
        }
        for (name, value) in &cmd.envs {
            command.env(name, value);
        }
        command.env(brush_core::broker::RENDEZVOUS_ENV, rendezvous.path());

        // Always capture: the host applies the IoPlan itself (an inherited run
        // copies these into the D28 log). stdin is piped so the child reads the
        // bytes the guest supplied and nothing of the parent's own stdin.
        command.stdin(Stdio::piped());
        command.stdout(Stdio::piped());
        command.stderr(Stdio::piped());

        let mut child = command.spawn().map_err(|e| spawn_failed(&e))?;

        // The credential is the child's pid (D24), known only now.
        #[expect(
            clippy::cast_possible_wrap,
            reason = "a pid fits in i32 on every platform brush-core's broker serves"
        )]
        let pid = child.id() as i32;

        // Serve before collecting output: the child runs the utility only after
        // it has its session, so waiting for output first would deadlock against
        // a child still blocked for its namespace.
        brush_core::broker::serve(rendezvous, Some(pid), &self.session)
            .map_err(|e| broker_failed(&e))?;

        // Feed stdin on a thread so a payload larger than the pipe buffer cannot
        // deadlock against the parent reading stdout/stderr. The bytes are owned
        // by the thread; the pipe closes when it drops, which is the child's EOF.
        if let Some(mut sink) = child.stdin.take() {
            let bytes = stdin.to_vec();
            std::thread::spawn(move || {
                let _ = sink.write_all(&bytes);
            });
        }

        let output = child.wait_with_output().map_err(|e| spawn_failed(&e))?;
        Ok(RunResult {
            exit: exit_of(output.status),
            stdout: output.stdout,
            stderr: output.stderr,
        })
    }
}

/// Reads a finished child's exit as the platform's [`Exit`].
///
/// A normal exit is its code; a signal death is [`Exit::Signal`] with the signal
/// number, which the host renders as `exec_status!`'s negated value or
/// `exec_exit_code!`'s error.
fn exit_of(status: std::process::ExitStatus) -> Exit {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt as _;
        if let Some(signal) = status.signal() {
            return Exit::Signal(signal);
        }
    }
    // `code()` is `Some` for a normal exit on every platform; the `1` fallback is
    // only reached for a signal death on a non-unix target, which has no signal
    // number to report and is a non-zero end regardless.
    Exit::Code(status.code().unwrap_or(1))
}

/// The broker could not hand the child a namespace (D24). Not a command that ran
/// and failed — the ability to run a confined child at all is what is missing —
/// so it is an error, which the host maps to the effect's error channel.
fn broker_failed(error: &std::io::Error) -> PlatformError {
    PlatformError::Other(format!("session broker: {error}"))
}

/// The launcher could not be spawned. A backend failure — the launcher missing
/// or unrunnable is a host-configuration problem, not the guest's program being
/// absent — so it carries its message rather than masquerading as `NotFound` for
/// the requested command.
fn spawn_failed(error: &std::io::Error) -> PlatformError {
    use std::io::ErrorKind;
    match error.kind() {
        ErrorKind::PermissionDenied => PlatformError::PermissionDenied,
        ErrorKind::BrokenPipe => PlatformError::BrokenPipe,
        ErrorKind::Interrupted => PlatformError::Interrupted,
        _ => PlatformError::Other(format!("could not run the launcher: {error}")),
    }
}
