//! Step 5: `spawn`/`wait_any` and the four `exec_*` effects (D25, D2, D24).
//!
//! Execution is a seam — [`Executor`] — so these drive the shape with a test
//! executor rather than a real process: the broker-backed executor is the link
//! step's, and the properties that matter now are the ABI shape and the routing,
//! both of which a fake exercises exactly.

#![cfg(unix)]
#![cfg(test)]
#![allow(
    clippy::disallowed_methods,
    clippy::expect_used,
    reason = "the test builds a session on the host, which is the point"
)]

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use brush_platform::{
    Cmd, ExecOutput, Executor, Exit, IoPlan, PlatformEffects, PlatformError, RunResult,
    SessionFacts, StdinMode, VfsPlatform,
};
use brush_vfs::{Policy, Session, Vfs};

/// A programmable executor that records what it was asked to run.
#[derive(Clone, Default)]
struct FakeExecutor {
    responses: HashMap<String, RunResult>,
    runs: Rc<RefCell<u32>>,
    last_stdin: Rc<RefCell<Vec<u8>>>,
}

impl FakeExecutor {
    fn responding(program: &str, result: RunResult) -> Self {
        let mut responses = HashMap::new();
        responses.insert(program.to_owned(), result);
        Self {
            responses,
            ..Self::default()
        }
    }
}

impl Executor for FakeExecutor {
    fn run(&mut self, cmd: &Cmd, stdin: &[u8]) -> Result<RunResult, PlatformError> {
        *self.runs.borrow_mut() += 1;
        *self.last_stdin.borrow_mut() = stdin.to_vec();
        self.responses
            .get(&cmd.program)
            .cloned()
            .ok_or(PlatformError::NotFound)
    }
}

fn result(exit: Exit, stdout: &[u8], stderr: &[u8]) -> RunResult {
    RunResult {
        exit,
        stdout: stdout.to_vec(),
        stderr: stderr.to_vec(),
    }
}

fn host_with(executor: FakeExecutor) -> VfsPlatform {
    let mounts = Policy::identity().expect("identity");
    let session = Session::new(std::sync::Arc::new(Vfs::new(mounts)));
    VfsPlatform::new(session, SessionFacts::neutral()).with_executor(Box::new(executor))
}

#[test]
fn exec_status_returns_the_code_and_negates_a_signal() {
    let mut ok = host_with(FakeExecutor::responding(
        "true",
        result(Exit::Code(0), b"", b""),
    ));
    assert_eq!(ok.cmd_exec_status(&Cmd::new("true")).expect("status"), 0);

    let mut nonzero = host_with(FakeExecutor::responding(
        "false",
        result(Exit::Code(3), b"", b""),
    ));
    assert_eq!(
        nonzero.cmd_exec_status(&Cmd::new("false")).expect("status"),
        3
    );

    let mut killed = host_with(FakeExecutor::responding(
        "sleep",
        result(Exit::Signal(15), b"", b""),
    ));
    assert_eq!(
        killed.cmd_exec_status(&Cmd::new("sleep")).expect("status"),
        -15,
        "a signal death is the negated signal, unambiguous against real codes"
    );
}

#[test]
fn exec_exit_code_errors_on_a_signal() {
    let mut killed = host_with(FakeExecutor::responding(
        "sleep",
        result(Exit::Signal(9), b"", b""),
    ));
    assert!(
        killed.cmd_exec_exit_code(&Cmd::new("sleep")).is_err(),
        "exec_exit_code collapses a signal death into an error"
    );

    let mut ok = host_with(FakeExecutor::responding(
        "true",
        result(Exit::Code(0), b"", b""),
    ));
    assert_eq!(ok.cmd_exec_exit_code(&Cmd::new("true")).expect("code"), 0);
}

#[test]
fn exec_output_separates_streams_and_flags_nonzero() {
    let mut ok = host_with(FakeExecutor::responding(
        "echo",
        result(Exit::Code(0), b"out\n", b"err\n"),
    ));
    assert_eq!(
        ok.cmd_exec_output(&Cmd::new("echo")).expect("output"),
        ExecOutput::Success {
            stdout: b"out\n".to_vec(),
            stderr: b"err\n".to_vec(),
        }
    );

    let mut bad = host_with(FakeExecutor::responding(
        "grep",
        result(Exit::Code(2), b"", b"boom\n"),
    ));
    assert_eq!(
        bad.cmd_exec_output(&Cmd::new("grep")).expect("output"),
        ExecOutput::NonZeroExit {
            exit_code: 2,
            stdout: Vec::new(),
            stderr: b"boom\n".to_vec(),
        },
        "a command that ran and exited non-zero is a successful run, not an Err"
    );
}

#[test]
fn inherited_output_lands_in_the_job_log_and_captured_does_not() {
    // exec_status inherits stdio, so the child's output is the job's output
    // (D28). exec_output captures, so the log stays clean.
    let mut inherit = host_with(FakeExecutor::responding(
        "make",
        result(Exit::Code(0), b"building\n", b"warning\n"),
    ));
    inherit.cmd_exec_status(&Cmd::new("make")).expect("status");
    assert_eq!(inherit.output().stdout(), b"building\n");
    assert_eq!(inherit.output().stderr(), b"warning\n");
    assert_eq!(
        inherit.output().rendered(),
        b"building\nwarning\n",
        "the child's streams keep their order in the render"
    );

    let mut capture = host_with(FakeExecutor::responding(
        "make",
        result(Exit::Code(0), b"building\n", b"warning\n"),
    ));
    capture.cmd_exec_output(&Cmd::new("make")).expect("output");
    assert!(
        capture.output().is_empty(),
        "captured output must not leak into the job log"
    );
}

#[test]
fn inherit_stdin_hands_the_session_stdin_to_the_child() {
    // just's backticks: `x := \`cat\`` reads what was piped to just itself.
    let exec = FakeExecutor::responding("cat", result(Exit::Code(0), b"", b""));
    let seen = Rc::clone(&exec.last_stdin);
    let mut host = host_with(exec).with_stdin(b"piped input".to_vec());

    host.cmd_exec_output_inherit_stdin(&Cmd::new("cat"))
        .expect("output");
    assert_eq!(
        *seen.borrow(),
        b"piped input",
        "the child must receive the session's stdin"
    );
}

#[test]
fn plain_exec_output_gives_the_child_no_stdin() {
    let exec = FakeExecutor::responding("cat", result(Exit::Code(0), b"", b""));
    let seen = Rc::clone(&exec.last_stdin);
    let mut host = host_with(exec).with_stdin(b"not for the child".to_vec());

    host.cmd_exec_output(&Cmd::new("cat")).expect("output");
    assert!(
        seen.borrow().is_empty(),
        "exec_output runs with a null stdin"
    );
}

#[test]
fn spawn_and_wait_any_hold_two_handles_at_once() {
    // The ABI-shape property: two handles live at the same time, waited in
    // either order. A blocking-exec ABI cannot express this; a serial-but-
    // correctly-shaped one can, which is why the shape lands now (D25).
    let mut exec = FakeExecutor::default();
    exec.responses
        .insert("a".to_owned(), result(Exit::Code(10), b"", b""));
    exec.responses
        .insert("b".to_owned(), result(Exit::Code(20), b"", b""));
    let mut host = host_with(exec);

    let a = host
        .spawn(&Cmd::new("a"), IoPlan::capture(StdinMode::Null))
        .expect("spawn a");
    let b = host
        .spawn(&Cmd::new("b"), IoPlan::capture(StdinMode::Null))
        .expect("spawn b");
    assert_ne!(a, b, "two live handles must be distinct");

    let (first, f1) = host.wait_any(&[a, b]).expect("wait a");
    assert_eq!(first, a);
    assert_eq!(f1.exit, Exit::Code(10));
    let (second, f2) = host.wait_any(&[b]).expect("wait b");
    assert_eq!(second, b);
    assert_eq!(f2.exit, Exit::Code(20));
}

#[test]
fn a_waited_handle_names_nothing_and_ids_are_not_reused() {
    let mut host = host_with(FakeExecutor::responding(
        "x",
        result(Exit::Code(0), b"", b""),
    ));
    let first = host
        .spawn(&Cmd::new("x"), IoPlan::capture(StdinMode::Null))
        .expect("spawn");
    host.wait_any(&[first]).expect("wait");
    assert!(
        host.wait_any(&[first]).is_err(),
        "a waited handle must not name its job a second time"
    );

    let second = host
        .spawn(&Cmd::new("x"), IoPlan::capture(StdinMode::Null))
        .expect("spawn");
    assert_ne!(
        first, second,
        "a fresh handle must not reuse a retired id (the aliasing bug D25 warns of)"
    );
}

#[test]
fn without_an_executor_exec_fails_exactly_as_spawn_does() {
    // Gate 6, stated structurally. If `exec_*` had a path to execution that did
    // not go through `spawn`, removing the executor would break them
    // differently. It does not: both are `Unsupported`, because `exec_*` is
    // `spawn` + `wait_any` and nothing else.
    let mounts = Policy::identity().expect("identity");
    let session = Session::new(std::sync::Arc::new(Vfs::new(mounts)));
    let mut host = VfsPlatform::new(session, SessionFacts::neutral()); // no executor

    assert_eq!(
        host.spawn(&Cmd::new("x"), IoPlan::inherit()).unwrap_err(),
        PlatformError::Unsupported
    );
    assert_eq!(
        host.cmd_exec_status(&Cmd::new("x")).unwrap_err(),
        PlatformError::Unsupported,
        "exec_status must fail through spawn, not around it"
    );
    assert_eq!(
        host.cmd_exec_output(&Cmd::new("x")).unwrap_err(),
        PlatformError::Unsupported
    );
    assert_eq!(
        host.cmd_exec_exit_code(&Cmd::new("x")).unwrap_err(),
        PlatformError::Unsupported
    );
}
