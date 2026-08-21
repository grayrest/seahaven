//! Execution logic for running shell commands.

use crate::config::{ShellConfig, WhichShell};
use crate::testcase::{ShellInvocation, TestCase, TestCaseSet, TestFile};
use anyhow::{Context, Result};
use assert_fs::fixture::{FileWriteStr, PathChild};
#[cfg(pty)]
use std::os::unix::process::ExitStatusExt;
#[cfg(unix)]
use std::os::unix::{fs::PermissionsExt, process::CommandExt};
use std::{path::PathBuf, process::ExitStatus};

/// Default timeout for test commands in seconds.
pub const DEFAULT_TIMEOUT_IN_SECONDS: u64 = 15;

/// Result of running a shell command.
#[derive(Debug)]
pub struct RunResult {
    /// Exit status of the command.
    pub exit_status: ExitStatus,
    /// Standard output.
    pub stdout: String,
    /// Standard error.
    pub stderr: String,
    /// Duration of the command.
    pub duration: std::time::Duration,
}

/// How many times a pty case will re-spawn a shell that never started.
///
/// See `run_shell`: the fork-plus-controlling-terminal setup loses a race
/// against the rest of the runner about one time in ten, and a shell that never
/// wrote a byte has not disagreed with anything.
const PTY_SPAWN_ATTEMPTS: usize = 3;

impl TestCase {
    /// Runs this test case with the given shell configuration.
    pub async fn run_shell(
        &self,
        shell_config: &ShellConfig,
        working_dir: &assert_fs::TempDir,
    ) -> Result<RunResult> {
        if !self.pty {
            let test_cmd = self.create_command_for_shell(shell_config, working_dir);
            return self.run_command_with_stdin(test_cmd).await;
        }

        // Retry a pty attempt that never got the shell running.
        //
        // Even serialized against each other, `Session::spawn`'s fork-plus-
        // controlling-terminal dance loses about one run in ten against the
        // other 31 threads' spawns: the shell comes up with no pty of its own
        // and dies having written *nothing*, so the first `#expect-prompt`
        // waits out its timeout on a prompt that was never coming. Both shells
        // are exposed, and it was usually the oracle -- which fails the case on
        // output neither side produced.
        //
        // The signature is narrow on purpose: not one byte was read. A shell
        // that started and then misbehaved has written its prompt, so this
        // cannot mask a real difference between `bash` and `brush`; it can only
        // re-run a spawn that did not happen.
        let mut result = None;
        for _ in 0..PTY_SPAWN_ATTEMPTS {
            let test_cmd = self.create_command_for_shell(shell_config, working_dir);
            let candidate = self.run_command_with_pty(test_cmd).await?;
            let never_started =
                candidate.stdout.is_empty() && candidate.stderr.starts_with("failed to expect");
            result = Some(candidate);
            if !never_started {
                break;
            }
        }

        result.ok_or_else(|| anyhow::anyhow!("no pty attempt ran"))
    }

    /// Creates the test files in the given temporary directory.
    pub fn create_test_files_in(
        &self,
        temp_dir: &assert_fs::TempDir,
        test_case_set: &TestCaseSet,
    ) -> Result<()> {
        for test_file in test_case_set
            .common_test_files
            .iter()
            .chain(self.test_files.iter())
        {
            Self::create_test_file(temp_dir, test_file, &test_case_set.source_dir)?;
        }

        Ok(())
    }

    fn create_test_file(
        temp_dir: &assert_fs::TempDir,
        test_file: &TestFile,
        source_dir: &std::path::Path,
    ) -> Result<()> {
        let test_file_path = temp_dir.child(test_file.path.as_path());

        if let Some(source_path) = &test_file.source_path {
            if !test_file.contents.is_empty() {
                return Err(anyhow::anyhow!(
                    "test file {} has both contents and source_path",
                    test_file_path.to_string_lossy()
                ));
            }

            if source_path.is_absolute() {
                return Err(anyhow::anyhow!(
                    "source_path {} is not a relative path",
                    source_path.to_string_lossy()
                ));
            }

            let abs_source_path = source_dir.join(source_path);

            let source_contents = std::fs::read_to_string(&abs_source_path)
                .with_context(|| format!("reading {}", abs_source_path.to_string_lossy()))?;

            test_file_path.write_str(source_contents.as_str())?;
        } else {
            test_file_path.write_str(test_file.contents.as_str())?;
        }

        #[cfg(unix)]
        if test_file.executable {
            // chmod u+x
            let mut perms = test_file_path.metadata()?.permissions();
            perms.set_mode(perms.mode() | 0o100);
            std::fs::set_permissions(test_file_path, perms)?;
        }

        Ok(())
    }

    /// Constructs a `Command` to invoke the given shell binary, optionally
    /// prepending a launcher (e.g., `["wasmtime", "run", "--"]`). When a
    /// launcher is provided, the first element becomes the program to execute
    /// and the rest are passed as leading arguments before the shell binary path.
    fn new_shell_command(
        shell_path: &std::path::Path,
        launcher: Option<&[String]>,
    ) -> std::process::Command {
        if let Some([program, leading_args @ ..]) = launcher {
            let mut cmd = std::process::Command::new(program);
            cmd.args(leading_args);
            cmd.arg(shell_path);
            cmd
        } else {
            std::process::Command::new(shell_path)
        }
    }

    fn create_command_for_shell(
        &self,
        shell_config: &ShellConfig,
        working_dir: &assert_fs::TempDir,
    ) -> std::process::Command {
        let (mut test_cmd, coverage_target_dir) = match self.invocation {
            ShellInvocation::ExecShellBinary => match &shell_config.which {
                WhichShell::ShellUnderTest(name) => {
                    let cli_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
                    let default_target_dir = || cli_dir.parent().unwrap().join("target");
                    let target_dir = std::env::var("CARGO_TARGET_DIR")
                        .ok()
                        .map_or_else(default_target_dir, PathBuf::from);
                    (
                        Self::new_shell_command(name, shell_config.launcher.as_deref()),
                        Some(target_dir),
                    )
                }
                // Launcher only applies to the shell under test; the oracle is invoked directly.
                WhichShell::NamedShell(name) => (Self::new_shell_command(name, None), None),
            },
            ShellInvocation::ExecScript(_) => unimplemented!("exec script test"),
        };

        if matches!(shell_config.which, WhichShell::ShellUnderTest(_)) {
            for arg in &self.additional_test_args {
                test_cmd.arg(arg);
            }
        }

        for arg in &shell_config.default_args {
            if !self.removed_default_args.contains(arg) {
                test_cmd.arg(arg);
            }
        }

        // Clear all environment vars for consistency.
        test_cmd.args(&self.args).env_clear();

        // Set locale to C for consistent behavior across systems.
        test_cmd.env("LC_ALL", "C");
        // Hard-code a well known prompt for PS1.
        test_cmd.env("PS1", "test$ ");
        // Try to get decent backtraces when problems get hit.
        test_cmd.env("RUST_BACKTRACE", "1");
        // Compute a PATH that contains what we need.
        test_cmd.env("PATH", shell_config.compute_test_path_var());

        // Set up any env vars needed for collecting coverage data.
        if let Some(coverage_target_dir) = &coverage_target_dir {
            test_cmd.env("CARGO_LLVM_COV_TARGET_DIR", coverage_target_dir);
            test_cmd.env(
                "LLVM_PROFILE_FILE",
                coverage_target_dir.join("brush-%p-%40m.profraw"),
            );
        }

        for (k, v) in &self.env {
            test_cmd.env(k, v);
        }

        if let Some(home_dir) = &self.home_dir {
            let abs_home_dir = if home_dir.is_relative() {
                working_dir.join(home_dir)
            } else {
                home_dir.to_owned()
            };

            test_cmd.env("HOME", abs_home_dir.to_string_lossy().to_string());
        }

        test_cmd.current_dir(working_dir.to_string_lossy().to_string());

        test_cmd
    }

    #[expect(clippy::unused_async)]
    #[cfg(not(pty))]
    async fn run_command_with_pty(&self, _cmd: std::process::Command) -> Result<RunResult> {
        Err(anyhow::anyhow!("pty test not supported on this platform"))
    }

    #[expect(clippy::unused_async)]
    #[cfg(pty)]
    async fn run_command_with_pty(&self, cmd: std::process::Command) -> Result<RunResult> {
        use crate::util::{make_expectrl_output_readable, read_expectrl_log};
        use expectrl::{Expect, process::Termios as _};

        // One pty case at a time, across the whole runner.
        //
        // `Session::spawn` forks, allocates a pty and makes the child a session
        // leader with the pty as its controlling terminal. Doing that from a
        // 32-thread runtime while other threads are forking too is a race the
        // *oracle* loses: `bash` comes up with no controlling terminal, takes
        // SIGHUP, and dies having printed nothing, so the case fails on a
        // prompt that was never going to appear. Measured: the case flakes
        // roughly one run in six alongside others and zero times in 25 runs on
        // its own.
        //
        // Nine cases, so serializing them costs almost nothing. A plain
        // `std::sync::Mutex` because this function never awaits -- see the
        // `unused_async` above -- and poisoning is not interesting here, since
        // a panicking pty case has already failed the run.
        static PTY_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _pty_guard = PTY_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let mut log = Vec::new();
        let writer = std::io::Cursor::new(&mut log);

        let start_time = std::time::Instant::now();
        let mut p = expectrl::session::log(expectrl::Session::spawn(cmd)?, writer)?;
        p.set_echo(true)?;

        if let Some(stdin) = &self.stdin {
            for line in stdin.lines() {
                if let Some(expectation) = line.strip_prefix("#expect:") {
                    if let Err(inner) = p.expect(expectation) {
                        return Ok(RunResult {
                            exit_status: ExitStatus::from_raw(1),
                            stdout: read_expectrl_log(log).unwrap_or_default(),
                            stderr: std::format!("failed to expect '{expectation}': {inner}"),
                            duration: start_time.elapsed(),
                        });
                    }
                } else if let Some(control_code) = line.strip_prefix("#send:") {
                    match control_code.to_lowercase().as_str() {
                        "ctrl+d" => p.send(expectrl::ControlCode::EndOfTransmission)?,
                        "tab" => p.send(expectrl::ControlCode::HorizontalTabulation)?,
                        "enter" => p.send(expectrl::ControlCode::LineFeed)?,
                        _ => (),
                    }
                } else if line.trim() == "#expect-prompt" {
                    if let Err(inner) = p.expect("test$ ") {
                        return Ok(RunResult {
                            exit_status: ExitStatus::from_raw(1),
                            stdout: read_expectrl_log(log).unwrap_or_default(),
                            stderr: std::format!("failed to expect prompt: {inner}"),
                            duration: start_time.elapsed(),
                        });
                    }
                } else {
                    p.send(line)?;
                }
            }
        }

        if let Err(inner) = p.expect(expectrl::Eof) {
            return Ok(RunResult {
                exit_status: ExitStatus::from_raw(1),
                stdout: read_expectrl_log(log).unwrap_or_default(),
                stderr: std::format!("failed to expect EOF: {inner}"),
                duration: start_time.elapsed(),
            });
        }

        let mut wait_status = p.get_process().status()?;

        if matches!(wait_status, expectrl::process::unix::WaitStatus::StillAlive) {
            // Try to terminate it safely.
            p.get_process_mut()
                .kill(expectrl::process::unix::Signal::SIGTERM)?;
            wait_status = p.get_process().wait()?;
        }

        let duration = start_time.elapsed();
        let output = read_expectrl_log(log)?;
        let cleaned = make_expectrl_output_readable(output);

        match wait_status {
            expectrl::process::unix::WaitStatus::Exited(_, code) => Ok(RunResult {
                exit_status: ExitStatus::from_raw(code),
                stdout: cleaned,
                stderr: String::new(),
                duration,
            }),
            expectrl::process::unix::WaitStatus::Signaled(_, _, _) => {
                Err(anyhow::anyhow!("process was signaled"))
            }
            _ => Err(anyhow::anyhow!(
                "unexpected status for process: {wait_status:?}"
            )),
        }
    }

    #[expect(clippy::unused_async)]
    #[allow(unused_mut, reason = "only mutated on some platforms")]
    async fn run_command_with_stdin(&self, mut cmd: std::process::Command) -> Result<RunResult> {
        // The highest descriptor the child could have inherited, read in the
        // *parent* so the child's hook does no allocation and makes no
        // non-async-signal-safe call. Capped: the limit is often 2^63-1 under
        // `RLIMIT_INFINITY`, and closing that many would never return.
        #[cfg(unix)]
        let max_fd: i32 = {
            const CAP: u64 = 4096;
            nix::sys::resource::getrlimit(nix::sys::resource::Resource::RLIMIT_NOFILE)
                .map_or(CAP, |(soft, _)| soft)
                .min(CAP)
                .try_into()
                .unwrap_or(i32::MAX)
        };

        // SAFETY:
        // To avoid bash trying to directly access /dev/tty and generate tty-related signals,
        // we create a new session for the child process. The standard library has a setsid()
        // API but it's unstable, so we use nix here. Calling pre_exec can be unsafe as
        // it runs in the child process after fork() but before exec(), and there are constraints
        // around what can be safely done in that context. However, calling setsid() is generally
        // considered safe as it doesn't allocate memory or perform complex operations to forked
        // state.
        //
        // The close loop below is safe for the same reason and then some: `close(2)` is
        // async-signal-safe and the bound was computed before the fork. It runs *after* the
        // standard library has dup2'd the child's stdio onto 0, 1 and 2, so those survive.
        //
        // Why close at all: a shell under test must not see the test runner's descriptors.
        // `mapfile -u 99` is supposed to fail because fd 99 is closed, and bash returns 0 when
        // it happens to be open -- so whether that case passed depended on the runner's
        // descriptor table at the moment of the spawn, which is why it failed once in a
        // loaded `cargo test --workspace` and never in isolation. Every case that names a
        // descriptor has the same exposure; this closes the class rather than that one case.
        #[cfg(unix)]
        let hook = move || {
            let _ = nix::unistd::setsid();
            for fd in 3..max_fd {
                // SAFETY: `close(2)` is async-signal-safe, and closing a
                // descriptor that is not open is a no-op `EBADF`.
                unsafe { libc::close(fd) };
            }
            Ok(())
        };

        // SAFETY: as described above -- the hook only calls `setsid(2)` and
        // `close(2)`, both async-signal-safe, and allocates nothing.
        #[cfg(unix)]
        unsafe {
            cmd.pre_exec(hook)
        };

        let mut test_cmd = assert_cmd::Command::from_std(cmd);

        test_cmd.timeout(std::time::Duration::from_secs(
            self.timeout_in_seconds
                .unwrap_or(DEFAULT_TIMEOUT_IN_SECONDS),
        ));

        if let Some(stdin) = &self.stdin {
            test_cmd.write_stdin(stdin.as_bytes());
        }

        let start_time = std::time::Instant::now();
        let cmd_result = test_cmd.output()?;
        let duration = start_time.elapsed();

        Ok(RunResult {
            exit_status: cmd_result.status,
            stdout: String::from_utf8_lossy(cmd_result.stdout.as_slice()).to_string(),
            stderr: String::from_utf8_lossy(cmd_result.stderr.as_slice()).to_string(),
            duration,
        })
    }
}
