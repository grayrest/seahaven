//! The marshalling discipline every refcounted effect shares.
//!
//! An effect crosses the ABI three times: it takes owned Roc values as
//! arguments, it calls the session, and it returns an owned Roc value. Each
//! crossing has one rule, and getting any of them wrong is a leak or a
//! double-free that only shows at runtime — so they live here, once.
//!
//! # Arguments are owned; release each exactly once
//!
//! Roc hands an effect owned references. The effect must `decref` each argument
//! exactly once, after reading it and before returning — whether it succeeds or
//! fails. The `*_to_string` / `*_to_vec` helpers below consume their argument
//! and decref it, so an effect that funnels every argument through them cannot
//! forget.
//!
//! # A path is a string (D45)
//!
//! Every path and `OsStr` argument is one of three variants —
//! `Utf8`, `UnixBytes`, `WindowsU16s`. On this platform paths are strings, so
//! [`native_into_string`] accepts `Utf8` directly, decodes `UnixBytes` as UTF-8,
//! and rejects a non-UTF-8 or Windows value as [`PlatformError::NotFound`]: it
//! cannot be named here, and an unnameable path is not-found (see
//! [`brush_platform::PlatformError`]). Values handed *back* are always `Utf8`,
//! the honest encoding for a namespace whose paths are strings.

use core::mem::ManuallyDrop;

use brush_platform::PlatformError;

use crate::roc_host;
use crate::roc_platform_abi::{
    HostIOErr, HostIOErrPayload, HostIOErrTag, IOErr, IOErrPayload, IOErrTag, RocHost, RocListWith,
    RocStr, UnixBytesOrUtf8OrWindowsU16s as Native,
    UnixBytesOrUtf8OrWindowsU16sPayload as NativePayload,
    UnixBytesOrUtf8OrWindowsU16sTag as NativeTag,
};

/// Builds an owned byte-`List` from a slice. The glue does not emit a typed
/// helper for `List U8`, so this is the one wrapper over the generic
/// `from_slice`, shared by every effect that returns bytes.
pub(crate) fn roc_u8_list_from_slice(slice: &[u8], host: &RocHost) -> RocListWith<u8, false> {
    unsafe { RocListWith::<u8, false>::from_slice(slice, host) }
}

/// Consumes an owned native path/OsStr argument into a `String`, releasing it.
///
/// The single choke point every path argument passes through. A `Utf8` value is
/// its string; a `UnixBytes` value is decoded as UTF-8; a non-UTF-8 or
/// `WindowsU16s` value is [`PlatformError::NotFound`] (D45) — unnameable here.
/// The argument is decref'd on every path, success or failure.
pub(crate) fn native_into_string(value: Native, host: &RocHost) -> Result<String, PlatformError> {
    match value.tag {
        NativeTag::Utf8 => {
            let text = unsafe { ManuallyDrop::into_inner(value.payload.utf8) };
            let owned = text.as_str().to_owned();
            unsafe { text.decref(host) };
            Ok(owned)
        }
        NativeTag::UnixBytes => {
            let bytes = unsafe { ManuallyDrop::into_inner(value.payload.unix_bytes) };
            let decoded = core::str::from_utf8(bytes.as_slice())
                .map(str::to_owned)
                .map_err(|_| PlatformError::NotFound);
            unsafe { bytes.decref(host) };
            decoded
        }
        NativeTag::WindowsU16s => {
            let u16s = unsafe { ManuallyDrop::into_inner(value.payload.windows_u16s) };
            unsafe { u16s.decref(host) };
            Err(PlatformError::NotFound)
        }
    }
}

/// Builds an owned `Utf8` native path/OsStr from a string.
///
/// The one encoding handed back: paths are strings here (D45), so a virtual
/// path, an environment value, a directory entry all cross as `Utf8`. Every Roc
/// consumer has a `Utf8` arm, so this is fully handled on the far side.
pub(crate) fn native_from_str(text: &str, host: &RocHost) -> Native {
    Native {
        payload: NativePayload {
            utf8: ManuallyDrop::new(RocStr::from_str(text, host)),
        },
        tag: NativeTag::Utf8,
    }
}

/// Consumes an owned `RocStr` argument into a `String`, releasing it.
pub(crate) fn rocstr_into_string(value: RocStr, host: &RocHost) -> String {
    let owned = value.as_str().to_owned();
    unsafe { value.decref(host) };
    owned
}

/// Consumes an owned byte-`List` argument into a `Vec`, releasing it.
pub(crate) fn roc_bytes_into_vec(value: RocListWith<u8, false>, host: &RocHost) -> Vec<u8> {
    let owned = value.as_slice().to_vec();
    unsafe { value.decref(host) };
    owned
}

/// Builds an owned byte-`List` from a slice — the effect-facing name.
pub(crate) fn roc_bytes_from_slice(bytes: &[u8], host: &RocHost) -> RocListWith<u8, false> {
    roc_u8_list_from_slice(bytes, host)
}

// The two isomorphic IOErr wire types the glue emits: `IOErr` for most effects,
// `HostIOErr` for the `cmd_exec_exit_code`/`cmd_exec_status` results. One macro
// builds both from a `PlatformError`; the `Other` variant carries the message,
// which the vfs guarantees names a virtual path, never a host one.
macro_rules! define_io_err_builder {
    ($name:ident, $ty:ident, $tag:ident, $payload:ident) => {
        /// Maps a [`PlatformError`] onto this wire type.
        pub(crate) fn $name(error: &PlatformError, host: &RocHost) -> $ty {
            match error {
                PlatformError::AlreadyExists => $ty {
                    payload: $payload { already_exists: [] },
                    tag: $tag::AlreadyExists,
                },
                PlatformError::BrokenPipe => $ty {
                    payload: $payload { broken_pipe: [] },
                    tag: $tag::BrokenPipe,
                },
                PlatformError::Interrupted => $ty {
                    payload: $payload { interrupted: [] },
                    tag: $tag::Interrupted,
                },
                PlatformError::IsADirectory => $ty {
                    payload: $payload { is_adirectory: [] },
                    tag: $tag::IsADirectory,
                },
                PlatformError::NotADirectory => $ty {
                    payload: $payload { not_adirectory: [] },
                    tag: $tag::NotADirectory,
                },
                PlatformError::NotFound => $ty {
                    payload: $payload { not_found: [] },
                    tag: $tag::NotFound,
                },
                PlatformError::OutOfMemory => $ty {
                    payload: $payload { out_of_memory: [] },
                    tag: $tag::OutOfMemory,
                },
                PlatformError::PermissionDenied => $ty {
                    payload: $payload {
                        permission_denied: [],
                    },
                    tag: $tag::PermissionDenied,
                },
                PlatformError::Unsupported => $ty {
                    payload: $payload { unsupported: [] },
                    tag: $tag::Unsupported,
                },
                PlatformError::Other(message) => $ty {
                    payload: $payload {
                        other: ManuallyDrop::new(RocStr::from_str(message, host)),
                    },
                    tag: $tag::Other,
                },
            }
        }
    };
}

define_io_err_builder!(io_err, IOErr, IOErrTag, IOErrPayload);
define_io_err_builder!(host_io_err, HostIOErr, HostIOErrTag, HostIOErrPayload);

/// Runs a fallible effect closure against the session, returning either the
/// value or the [`PlatformError`]. A thin wrapper over [`crate::with_session`]
/// that supplies the not-installed error uniformly.
pub(crate) fn effect<T>(
    f: impl FnOnce(&mut brush_platform::VfsPlatform) -> Result<T, PlatformError>,
) -> Result<T, PlatformError> {
    crate::with_session(Err(PlatformError::Unsupported), f)
}

/// The host allocation context, for effect modules that build result values.
pub(crate) fn host() -> RocHost {
    roc_host()
}
