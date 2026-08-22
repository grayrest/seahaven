//! The process-execution hosted effects, marshalled onto the session (D25, D2).
//!
//! Each takes a command struct — program, args, envs, a clear-env flag — and
//! returns either an exit code or captured output. The command's execution goes
//! through the session's [`spawn`](brush_platform::PlatformEffects::spawn) /
//! [`wait_any`](brush_platform::PlatformEffects::wait_any) (gate 6), so a
//! confined run and an identity run take the identical path; without an executor
//! installed (the foundation's state), every one fails uniformly with
//! `Unsupported`, which marshals to the effect's error variant.
//!
//! # Releasing the command
//!
//! The command struct owns three refcounted parts — the program `OsStr` and the
//! `args`/`envs` lists, each of whose elements owns an `OsStr`. [`take_command`]
//! releases all of them: every element through [`native_into_string`] (which
//! decrefs it) and each list's spine through its shallow `decref`, exactly once.

use core::mem::ManuallyDrop;

use brush_platform::{Cmd, ExecOutput, PlatformEffects, PlatformError};

use crate::marshal::{effect, host, host_io_err, io_err, native_into_string, roc_bytes_from_slice};
use crate::roc_platform_abi::{
    AnonStruct3e7554e024207e25 as CmdOutputSuccess, AnonStruct3f89ee1e14924626 as CmdOutputFailure,
    FailedToGetExitCodeOrNonZeroExitCode as CmdOutputError,
    FailedToGetExitCodeOrNonZeroExitCodePayload as CmdOutputErrorPayload,
    FailedToGetExitCodeOrNonZeroExitCodeTag as CmdOutputErrorTag, HostCmdExecExitCodeArgs,
    HostCmdExecExitCodeResult, HostCmdExecExitCodeResultPayload, HostCmdExecExitCodeResultTag,
    HostCmdExecOutputArgs, HostCmdExecOutputInheritStdinArgs, HostCmdExecOutputResult,
    HostCmdExecOutputResultPayload, HostCmdExecOutputResultTag, HostCmdExecStatusArgs, RocHost,
    RocList, UnixBytesOrUtf8OrWindowsU16s as Native,
};

/// The four exec effects take four distinct-but-identical command structs. This
/// trait unifies them so [`take_command`] is written once; the macro impls it
/// for each by moving the four fields out.
trait CmdArgs {
    fn into_parts(self) -> (Native, RocList<Native>, RocList<Native>, bool);
}

macro_rules! impl_cmd_args {
    ($ty:ty) => {
        impl CmdArgs for $ty {
            fn into_parts(self) -> (Native, RocList<Native>, RocList<Native>, bool) {
                (self.program, self.args, self.envs, self.clear_envs)
            }
        }
    };
}

impl_cmd_args!(HostCmdExecExitCodeArgs);
impl_cmd_args!(HostCmdExecStatusArgs);
impl_cmd_args!(HostCmdExecOutputArgs);
impl_cmd_args!(HostCmdExecOutputInheritStdinArgs);

/// Consumes and releases a command struct into a [`Cmd`].
///
/// Every owned part is released here — the program, each list element, and each
/// list spine — whether or not the value can be represented as strings. A part
/// that is not valid UTF-8 (D45) makes the whole command unnameable: the error
/// is returned only after everything has been decref'd, so nothing leaks.
fn take_command(args: impl CmdArgs, host: &RocHost) -> Result<Cmd, PlatformError> {
    let (program, args, envs, clear_envs) = args.into_parts();
    let program = native_into_string(program, host);
    let arg_strings = take_native_list(args, host);
    let env_strings = take_native_list(envs, host);

    let program = program?;
    let arg_strings = arg_strings?;
    let env_strings = env_strings?;

    // envs cross as a flat [k0, v0, k1, v1, ...] list; pair them back up. A
    // trailing key with no value is dropped, as `basic-cli`'s chunking does.
    let envs = env_strings
        .chunks_exact(2)
        .map(|pair| (pair[0].clone(), pair[1].clone()))
        .collect();

    Ok(Cmd {
        program,
        args: arg_strings,
        envs,
        clear_envs,
    })
}

/// Consumes a `List(OsStr)`, releasing each element and the spine, into strings.
///
/// Returns the first decode error only after the entire list is released, so a
/// non-UTF-8 element never leaks the rest.
fn take_native_list(list: RocList<Native>, host: &RocHost) -> Result<Vec<String>, PlatformError> {
    let mut values = Vec::with_capacity(list.len());
    let mut first_error = None;
    for item in list.as_slice() {
        // `*item` copies the tag-union shell; `native_into_string` decrefs the
        // inner `OsStr` that copy points at. The spine is freed below.
        match native_into_string(*item, host) {
            Ok(value) => values.push(value),
            Err(error) => {
                if first_error.is_none() {
                    first_error = Some(error);
                }
            }
        }
    }
    unsafe { list.decref(host) };
    first_error.map_or(Ok(values), Err)
}

fn exit_ok(code: i32) -> HostCmdExecExitCodeResult {
    HostCmdExecExitCodeResult {
        payload: HostCmdExecExitCodeResultPayload {
            ok: ManuallyDrop::new(code),
        },
        tag: HostCmdExecExitCodeResultTag::Ok,
    }
}

fn exit_err(error: &PlatformError, host: &RocHost) -> HostCmdExecExitCodeResult {
    HostCmdExecExitCodeResult {
        payload: HostCmdExecExitCodeResultPayload {
            err: ManuallyDrop::new(host_io_err(error, host)),
        },
        tag: HostCmdExecExitCodeResultTag::Err,
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn hosted_cmd_exec_exit_code(
    args: HostCmdExecExitCodeArgs,
) -> HostCmdExecExitCodeResult {
    let host = host();
    let cmd = match take_command(args, &host) {
        Ok(cmd) => cmd,
        Err(error) => return exit_err(&error, &host),
    };
    match effect(|s| s.cmd_exec_exit_code(&cmd)) {
        Ok(code) => exit_ok(code),
        Err(error) => exit_err(&error, &host),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn hosted_cmd_exec_status(args: HostCmdExecStatusArgs) -> HostCmdExecExitCodeResult {
    let host = host();
    let cmd = match take_command(args, &host) {
        Ok(cmd) => cmd,
        Err(error) => return exit_err(&error, &host),
    };
    match effect(|s| s.cmd_exec_status(&cmd)) {
        Ok(code) => exit_ok(code),
        Err(error) => exit_err(&error, &host),
    }
}

fn output_ok(stdout: &[u8], stderr: &[u8], host: &RocHost) -> HostCmdExecOutputResult {
    HostCmdExecOutputResult {
        payload: HostCmdExecOutputResultPayload {
            ok: ManuallyDrop::new(CmdOutputSuccess {
                stdout_bytes: roc_bytes_from_slice(stdout, host),
                stderr_bytes: roc_bytes_from_slice(stderr, host),
            }),
        },
        tag: HostCmdExecOutputResultTag::Ok,
    }
}

fn output_nonzero(
    exit_code: i32,
    stdout: &[u8],
    stderr: &[u8],
    host: &RocHost,
) -> HostCmdExecOutputResult {
    HostCmdExecOutputResult {
        payload: HostCmdExecOutputResultPayload {
            err: ManuallyDrop::new(CmdOutputError {
                payload: CmdOutputErrorPayload {
                    non_zero_exit_code: ManuallyDrop::new(CmdOutputFailure {
                        stdout_bytes: roc_bytes_from_slice(stdout, host),
                        stderr_bytes: roc_bytes_from_slice(stderr, host),
                        exit_code,
                    }),
                },
                tag: CmdOutputErrorTag::NonZeroExitCode,
            }),
        },
        tag: HostCmdExecOutputResultTag::Err,
    }
}

fn output_failed(error: &PlatformError, host: &RocHost) -> HostCmdExecOutputResult {
    HostCmdExecOutputResult {
        payload: HostCmdExecOutputResultPayload {
            err: ManuallyDrop::new(CmdOutputError {
                payload: CmdOutputErrorPayload {
                    failed_to_get_exit_code: ManuallyDrop::new(io_err(error, host)),
                },
                tag: CmdOutputErrorTag::FailedToGetExitCode,
            }),
        },
        tag: HostCmdExecOutputResultTag::Err,
    }
}

/// Marshals an [`ExecOutput`] (or the failure to produce one) into the result.
fn output_result(
    outcome: Result<ExecOutput, PlatformError>,
    host: &RocHost,
) -> HostCmdExecOutputResult {
    match outcome {
        Ok(ExecOutput::Success { stdout, stderr }) => output_ok(&stdout, &stderr, host),
        Ok(ExecOutput::NonZeroExit {
            exit_code,
            stdout,
            stderr,
        }) => output_nonzero(exit_code, &stdout, &stderr, host),
        Err(error) => output_failed(&error, host),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn hosted_cmd_exec_output(args: HostCmdExecOutputArgs) -> HostCmdExecOutputResult {
    let host = host();
    let cmd = match take_command(args, &host) {
        Ok(cmd) => cmd,
        Err(error) => return output_failed(&error, &host),
    };
    output_result(effect(|s| s.cmd_exec_output(&cmd)), &host)
}

#[unsafe(no_mangle)]
pub extern "C" fn hosted_cmd_exec_output_inherit_stdin(
    args: HostCmdExecOutputInheritStdinArgs,
) -> HostCmdExecOutputResult {
    let host = host();
    let cmd = match take_command(args, &host) {
        Ok(cmd) => cmd,
        Err(error) => return output_failed(&error, &host),
    };
    output_result(effect(|s| s.cmd_exec_output_inherit_stdin(&cmd)), &host)
}
