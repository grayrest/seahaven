//! The standard-I/O hosted effects, marshalled onto the session (D28, D36).
//!
//! stdout and stderr append to the job's output log — there is no descriptor to
//! fail against — so they always succeed. stdin is a fixed source: a read is
//! either the next bytes or end of file, never a blocking wait, because D36
//! removed the terminal there would be nothing to wait on.

use core::mem::ManuallyDrop;

use brush_platform::PlatformEffects;

use crate::marshal::{host, roc_bytes_from_slice, roc_bytes_into_vec, rocstr_into_string};
use crate::roc_platform_abi::{
    EndOfFileOrStdinErr, EndOfFileOrStdinErrPayload, EndOfFileOrStdinErrTag, HostStderrLineResult,
    HostStderrLineResultPayload, HostStderrLineResultTag, HostStdinBytesResult,
    HostStdinBytesResultPayload, HostStdinBytesResultTag, HostStdinLineResult,
    HostStdinLineResultPayload, HostStdinLineResultTag, HostStdinReadToEndResult,
    HostStdinReadToEndResultPayload, HostStdinReadToEndResultTag, HostStdoutLineResult,
    HostStdoutLineResultPayload, HostStdoutLineResultTag, RocListWith, RocStr,
};
use crate::with_session;

// stdout and stderr share the unit-ok / IOErr-err shape and never actually err,
// so each needs only an `ok` constructor. `$ty` is the effect's own result type.
macro_rules! ok_unit {
    ($fn:ident, $ty:ident, $tag:ident, $payload:ident) => {
        fn $fn() -> $ty {
            $ty {
                payload: $payload { ok: [] },
                tag: $tag::Ok,
            }
        }
    };
}

ok_unit!(
    stdout_ok,
    HostStdoutLineResult,
    HostStdoutLineResultTag,
    HostStdoutLineResultPayload
);
ok_unit!(
    stderr_ok,
    HostStderrLineResult,
    HostStderrLineResultTag,
    HostStderrLineResultPayload
);

// --- stdout ----------------------------------------------------------------

#[unsafe(no_mangle)]
pub extern "C" fn hosted_stdout_line(message: RocStr) -> HostStdoutLineResult {
    let host = host();
    let text = rocstr_into_string(message, &host);
    with_session((), |s| {
        let _ = s.stdout_line(&text);
    });
    stdout_ok()
}

#[unsafe(no_mangle)]
pub extern "C" fn hosted_stdout_write(message: RocStr) -> HostStdoutLineResult {
    let host = host();
    let text = rocstr_into_string(message, &host);
    with_session((), |s| {
        let _ = s.stdout_write(&text);
    });
    stdout_ok()
}

#[unsafe(no_mangle)]
pub extern "C" fn hosted_stdout_write_bytes(bytes: RocListWith<u8, false>) -> HostStdoutLineResult {
    let host = host();
    let bytes = roc_bytes_into_vec(bytes, &host);
    with_session((), |s| {
        let _ = s.stdout_write_bytes(&bytes);
    });
    stdout_ok()
}

// --- stderr ----------------------------------------------------------------

#[unsafe(no_mangle)]
pub extern "C" fn hosted_stderr_line(message: RocStr) -> HostStderrLineResult {
    let host = host();
    let text = rocstr_into_string(message, &host);
    with_session((), |s| {
        let _ = s.stderr_line(&text);
    });
    stderr_ok()
}

#[unsafe(no_mangle)]
pub extern "C" fn hosted_stderr_write(message: RocStr) -> HostStderrLineResult {
    let host = host();
    let text = rocstr_into_string(message, &host);
    with_session((), |s| {
        let _ = s.stderr_write(&text);
    });
    stderr_ok()
}

#[unsafe(no_mangle)]
pub extern "C" fn hosted_stderr_write_bytes(bytes: RocListWith<u8, false>) -> HostStderrLineResult {
    let host = host();
    let bytes = roc_bytes_into_vec(bytes, &host);
    with_session((), |s| {
        let _ = s.stderr_write_bytes(&bytes);
    });
    stderr_ok()
}

// --- stdin -----------------------------------------------------------------

fn end_of_file() -> EndOfFileOrStdinErr {
    EndOfFileOrStdinErr {
        payload: EndOfFileOrStdinErrPayload { end_of_file: [] },
        tag: EndOfFileOrStdinErrTag::EndOfFile,
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn hosted_stdin_line() -> HostStdinLineResult {
    let host = host();
    match with_session(None, |s| s.stdin_line().unwrap_or(None)) {
        Some(line) => HostStdinLineResult {
            payload: HostStdinLineResultPayload {
                ok: ManuallyDrop::new(RocStr::from_str(&line, &host)),
            },
            tag: HostStdinLineResultTag::Ok,
        },
        None => HostStdinLineResult {
            payload: HostStdinLineResultPayload {
                err: ManuallyDrop::new(end_of_file()),
            },
            tag: HostStdinLineResultTag::Err,
        },
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn hosted_stdin_bytes() -> HostStdinBytesResult {
    let host = host();
    match with_session(None, |s| s.stdin_bytes().unwrap_or(None)) {
        Some(bytes) => HostStdinBytesResult {
            payload: HostStdinBytesResultPayload {
                ok: ManuallyDrop::new(roc_bytes_from_slice(&bytes, &host)),
            },
            tag: HostStdinBytesResultTag::Ok,
        },
        None => HostStdinBytesResult {
            payload: HostStdinBytesResultPayload {
                err: ManuallyDrop::new(end_of_file()),
            },
            tag: HostStdinBytesResultTag::Err,
        },
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn hosted_stdin_read_to_end() -> HostStdinReadToEndResult {
    let host = host();
    let bytes = with_session(Vec::new(), |s| s.stdin_read_to_end().unwrap_or_default());
    HostStdinReadToEndResult {
        payload: HostStdinReadToEndResultPayload {
            ok: ManuallyDrop::new(roc_bytes_from_slice(&bytes, &host)),
        },
        tag: HostStdinReadToEndResultTag::Ok,
    }
}
