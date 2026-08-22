//! Process execution: `spawn`/`wait_any` and the `exec_*` effects (D25, D2, D24).
//!
//! # The shape, not the scheduler
//!
//! D25 says the guest-facing ABI is `spawn` returning a handle and `wait_any`
//! over a list — never a blocking exec — because retrofitting handles later
//! means re-porting the Roc app. So the shape lands now and the scheduler does
//! not: `spawn` runs the command to completion and stores the result, `wait_any`
//! collects it. Two handles can be held at once and waited on in either order,
//! which is the property a serial-but-correctly-shaped ABI has and a
//! blocking-exec ABI cannot express.
//!
//! The four `exec_*` effects rocjust calls are implemented **through**
//! `spawn`/`wait_any`, with no second path to the executor — so when the real
//! scheduler arrives, no call site changes. The test that removes the executor
//! and watches `exec_status` fail exactly as `spawn` does is what pins "no
//! second path".
//!
//! # The executor is a seam
//!
//! Actually running a process under a closed world crosses D2's predicate and
//! D24's broker, both of which live in `brush-core`. This crate does not depend
//! on `brush-core` and does not spawn host processes — so execution is a trait,
//! [`Executor`], injected into the host. The broker-backed implementation is the
//! link step's (step 9), where `brush-core` is present; until then a test
//! executor drives the shape. This is the same move the whole platform makes:
//! the contract is a trait here, the binding is elsewhere.

use crate::effects::Effect;
use crate::error::PlatformError;

/// A command to run. Paths and arguments are strings (D45).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cmd {
    /// The program to run.
    pub program: String,
    /// Its arguments, not including the program name.
    pub args: Vec<String>,
    /// Environment additions, as name/value pairs.
    pub envs: Vec<(String, String)>,
    /// Whether to start from an empty environment rather than the session's.
    pub clear_envs: bool,
}

impl Cmd {
    /// A command that runs `program` with no arguments and the session's
    /// environment.
    #[must_use]
    pub fn new(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            envs: Vec::new(),
            clear_envs: false,
        }
    }

    /// Adds arguments.
    #[must_use]
    pub fn args(mut self, args: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }
}

/// How a finished job ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    /// Exited with this code.
    Code(i32),
    /// Killed by this signal (a positive number). `exec_status!` reports it
    /// negated; `exec_exit_code!` reports it as an error.
    Signal(i32),
}

/// Where a child's stdout and stderr go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputMode {
    /// Into the job's output log (D28) — what `exec_status!`/`exec_exit_code!`
    /// do, so the child's output appears where the parent's would.
    Inherit,
    /// Captured and returned to the guest — what `exec_output!` does.
    Capture,
}

/// What a child reads on stdin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StdinMode {
    /// The session's stdin, handed to the child (D24's broker, at the link
    /// step). just's backtick captures need this: a recipe body that runs a
    /// command in backticks reads what was piped to `just` itself.
    Inherit,
    /// Nothing — an empty stdin.
    Null,
}

/// The stdio arrangement for one run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IoPlan {
    /// Where stdout/stderr go.
    pub output: OutputMode,
    /// What stdin is.
    pub stdin: StdinMode,
}

impl IoPlan {
    /// Inherit all three streams — `exec_status!`/`exec_exit_code!`.
    #[must_use]
    pub const fn inherit() -> Self {
        Self {
            output: OutputMode::Inherit,
            stdin: StdinMode::Inherit,
        }
    }

    /// Capture stdout/stderr, with the given stdin — the `exec_output!` pair.
    #[must_use]
    pub const fn capture(stdin: StdinMode) -> Self {
        Self {
            output: OutputMode::Capture,
            stdin,
        }
    }
}

/// A handle to a spawned job (D25).
///
/// Opaque and **non-aliasing**: the id is minted from a counter that never
/// reuses a value within a session, so a handle to a job that has finished can
/// never name a different live job. That is the bug D25 warns handles will grow
/// if the shell's `jobs.len() + 1` scheme is exposed — here it cannot arise. It
/// is stronger than D43's generational indices for a single session; D43's full
/// per-frame table is still deferred.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct JobHandle(pub(crate) u64);

/// A finished job's result, held between `spawn` and `wait_any`.
#[derive(Debug, Clone)]
pub struct Finished {
    /// How it ended.
    pub exit: Exit,
    /// Captured stdout/stderr, present only when the job's plan captured. For an
    /// inherited-output job the bytes are already in the log, so there is
    /// nothing to carry here.
    pub captured: Option<(Vec<u8>, Vec<u8>)>,
}

/// What an [`Executor`] returns from a run: always captured, so the host can
/// apply the [`IoPlan`] itself.
#[derive(Debug, Clone)]
pub struct RunResult {
    /// How the child ended.
    pub exit: Exit,
    /// The child's stdout.
    pub stdout: Vec<u8>,
    /// The child's stderr.
    pub stderr: Vec<u8>,
}

/// Runs a process to completion. The seam where D2's predicate and D24's broker
/// plug in (see the module docs).
///
/// It always *captures* stdout and stderr; whether they are then shown as the
/// job's own output or returned to the guest is the host's decision, applied
/// from the [`IoPlan`]. `stdin` is the bytes the child reads — the session's
/// own stdin for an inheriting run, empty otherwise.
pub trait Executor {
    /// Runs `cmd` with `stdin` and returns how it ended plus its captured
    /// output.
    ///
    /// # Errors
    ///
    /// Returns [`PlatformError`] when the command could not be run at all — a
    /// closed world refusing it, or the broker failing. A command that runs and
    /// exits non-zero is a successful *run*: that is `Ok` with a non-zero
    /// [`Exit::Code`], not an error.
    fn run(&mut self, cmd: &Cmd, stdin: &[u8]) -> Effect<RunResult>;
}

/// The exit an errored-out execution reports when there is no executor bound.
///
/// Not `NotFound` — the program is not what is missing, the ability to run
/// anything is. `Unsupported` says the host has no execution backend, which is
/// exactly the state before the link step wires the broker in.
pub(crate) const NO_EXECUTOR: PlatformError = PlatformError::Unsupported;
