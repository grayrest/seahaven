//! A broker-less executor that delegates to an external `brush` trampoline.
//!
//! [`brush_broker_exec::BrokerExecutor`] is the real backend (D24), but linking
//! it into a Roc binary pulls `brush-core` -> `chrono` -> CoreFoundation, and
//! this roc toolchain has no way to link a macOS framework. This executor avoids
//! that: it depends on nothing but `std` and `brush-platform`, and it runs each
//! command by spawning a *separately built* `brush` binary as the trampoline --
//! `brush --invoke-bundled <program> <args>` -- letting brush's own dispatch
//! confine it (coreutils, and the `sh`/`bash` shell path). It is the identity
//! tier's execution: no namespace is brokered because the whole host filesystem
//! is the namespace, and brush's own closed world still refuses arbitrary
//! external programs (D2).
//!
//! The brush binary's path comes from `SEAHAVEN_BRUSH`; absent, execution is
//! `Unsupported`, as it is with no executor at all.

use std::io::Write as _;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use brush_platform::{Cmd, Executor, Exit, PlatformError, RunResult};

/// The environment variable naming the external `brush` trampoline.
const BRUSH_ENV: &str = "SEAHAVEN_BRUSH";

/// Runs a platform [`Cmd`] by spawning an external `brush` trampoline.
pub(crate) struct ShellExec {
    brush: PathBuf,
}

impl ShellExec {
    /// Binds to the `brush` path named by `SEAHAVEN_BRUSH`, or `None` if unset.
    pub(crate) fn from_env() -> Option<Self> {
        std::env::var_os(BRUSH_ENV).map(|p| Self { brush: p.into() })
    }
}

impl Executor for ShellExec {
    fn run(&mut self, cmd: &Cmd, stdin: &[u8]) -> Result<RunResult, PlatformError> {
        // Delegate to brush's dispatch: it confines coreutils and, for `sh`/
        // `bash`, runs the recipe body through brush itself (closed-world +
        // restricted). One path handles every command the guest asks for.
        let mut command = Command::new(&self.brush);
        command.arg("--invoke-bundled").arg(&cmd.program);
        command.args(&cmd.args);
        if cmd.clear_envs {
            command.env_clear();
        }
        for (name, value) in &cmd.envs {
            command.env(name, value);
        }
        command.stdin(Stdio::piped());
        command.stdout(Stdio::piped());
        command.stderr(Stdio::piped());

        let mut child = command.spawn().map_err(|e| {
            PlatformError::Other(format!("could not run the brush trampoline: {e}"))
        })?;

        if let Some(mut sink) = child.stdin.take() {
            let bytes = stdin.to_vec();
            std::thread::spawn(move || {
                let _ = sink.write_all(&bytes);
            });
        }

        let output = child
            .wait_with_output()
            .map_err(|e| PlatformError::Other(format!("brush trampoline: {e}")))?;
        Ok(RunResult {
            exit: exit_of(&output.status),
            stdout: output.stdout,
            stderr: output.stderr,
        })
    }
}

/// A finished child's exit as the platform's [`Exit`].
fn exit_of(status: &std::process::ExitStatus) -> Exit {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt as _;
        if let Some(signal) = status.signal() {
            return Exit::Signal(signal);
        }
    }
    Exit::Code(status.code().unwrap_or(1))
}
