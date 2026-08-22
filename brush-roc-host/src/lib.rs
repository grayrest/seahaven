//! The Roc platform host (step 9 of `plans/2026-08-22-platform.md`).
//!
//! This is the native tier's binding (D17): a `staticlib` the Roc compiler links
//! with a compiled Roc app. It supplies three things the app needs to run:
//!
//! 1. The Roc runtime symbols — `roc_alloc` and friends — that Roc-generated
//!    code calls to manage memory and report failures.
//! 2. `main`, which builds the argument list, hands control to the compiled Roc
//!    `roc_main`, and returns its exit code.
//! 3. The `hosted_*` effect functions the platform declares (`platform/Host.roc`),
//!    each routed through [`brush_platform`] so the confinement is the same one
//!    the whole milestone built.
//!
//! # Status
//!
//! This is step 9's **foundation**, not its completion. The runtime symbols and
//! the entrypoint are here and compile; the scalar effects (those with no
//! refcounted arguments) are wired to a global session. The effects that marshal
//! Roc's refcounted `Str`/`List` across the ABI — the bulk — and the
//! broker-backed executor and the real argument list are the remaining work,
//! marked `TODO(step-9)` where they belong. See the crate README.

#![allow(clippy::missing_safety_doc)]

use core::ffi::c_void;
use std::cell::RefCell;

use brush_platform::{PlatformEffects, SessionFacts, VfsPlatform};
use brush_vfs::{Policy, Session, Vfs};

#[allow(warnings)]
mod roc_platform_abi;

use roc_platform_abi::{OsStr, RocList, make_roc_host};

thread_local! {
    /// The one session every effect resolves against.
    ///
    /// Thread-local rather than a `static Mutex` because the session holds
    /// non-`Send` sources (its clock, RNG and executor), and D25 makes the guest
    /// single-threaded, so every `hosted_*` call runs on the thread `main` set
    /// it up on. Installed by [`main`] before `roc_main` runs; a launcher builds
    /// it from a derived grant (D44), and the foundation installs identity.
    static SESSION: RefCell<Option<VfsPlatform>> = const { RefCell::new(None) };
}

/// Runs `f` against the installed session, or returns `default` if none is
/// installed (which cannot happen once `main` has run).
fn with_session<T>(default: T, f: impl FnOnce(&mut VfsPlatform) -> T) -> T {
    SESSION.with_borrow_mut(|slot| slot.as_mut().map_or(default, f))
}

// --- Roc runtime symbols ---------------------------------------------------
//
// The simplest correct implementations: libc, whose `malloc` is
// maximally aligned, which is all Roc's allocations need. Roc stores a refcount
// ahead of the pointer and asks for the total size, so the host only forwards.

#[unsafe(no_mangle)]
pub unsafe extern "C" fn roc_alloc(length: usize, _alignment: usize) -> *mut c_void {
    unsafe { libc::malloc(length) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn roc_dealloc(ptr: *mut c_void, _alignment: usize) {
    unsafe { libc::free(ptr) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn roc_realloc(
    ptr: *mut c_void,
    new_length: usize,
    _alignment: usize,
) -> *mut c_void {
    unsafe { libc::realloc(ptr, new_length) }
}

/// Debug output. Writes the message to stderr — a later step routes it into the
/// D28 output log like every other write.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn roc_dbg(bytes: *const u8, len: usize) {
    write_stderr(bytes, len);
}

/// An `expect` failed. Reported like `roc_dbg`; execution continues, matching
/// Roc's own behaviour for a non-fatal expect.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn roc_expect_failed(bytes: *const u8, len: usize) {
    write_stderr(bytes, len);
}

/// The Roc program crashed. Report and abort — there is no unwinding across the
/// C ABI, and under D25 a crash must not be turned into a silent success.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn roc_crashed(bytes: *const u8, len: usize) {
    write_stderr(bytes, len);
    std::process::abort();
}

fn write_stderr(bytes: *const u8, len: usize) {
    use std::io::Write as _;
    if bytes.is_null() {
        return;
    }
    let slice = unsafe { core::slice::from_raw_parts(bytes, len) };
    let _ = std::io::stderr().write_all(slice);
}

// --- Entrypoint ------------------------------------------------------------

unsafe extern "C" {
    /// The compiled Roc `main_for_host!`. Provided by the linked Roc object.
    fn roc_main(args: RocList<OsStr>) -> i32;
}

/// The process entrypoint. Installs the session, runs the Roc app, exits.
///
/// The argument list is empty for now: marshalling `argv` into a
/// `RocList<OsStr>` is the same refcounted-`List` marshalling the effects need,
/// and lands with them. `make_roc_host` is the seam the glue exposes for
/// building Roc values from the host side.
#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    install_identity_session();

    // TODO(step-9): build the real argument list from `std::env::args_os`,
    // marshalling each into an `OsStr` tag-union value.
    let host = make_roc_host(core::ptr::null_mut());
    let _ = &host;
    let args = RocList::<OsStr>::empty();

    unsafe { roc_main(args) }
}

/// Installs an identity session so the pipeline can run. A launcher replaces
/// this with a session derived from a grant (D44) at the confined tier.
fn install_identity_session() {
    let Ok(mounts) = Policy::identity() else {
        return;
    };
    let session = Session::new(std::sync::Arc::new(Vfs::new(mounts)));
    let platform = VfsPlatform::new(session, SessionFacts::neutral());
    SESSION.with_borrow_mut(|slot| *slot = Some(platform));
}

// --- Scalar effects (no refcounted marshalling) ----------------------------
//
// These prove the effect-wiring pattern: each `hosted_*` symbol the platform
// declares becomes a `#[unsafe(no_mangle)] extern "C"` function reading or driving the
// global session. The refcounted effects follow the same shape with `RocStr`
// and `RocList` marshalling added.

#[unsafe(no_mangle)]
pub extern "C" fn hosted_env_pid() -> i64 {
    with_session(0, |s| s.env_pid())
}

#[unsafe(no_mangle)]
pub extern "C" fn hosted_env_num_cpus() -> i64 {
    with_session(1, |s| s.env_num_cpus())
}

#[unsafe(no_mangle)]
pub extern "C" fn hosted_env_tz_offset() -> i64 {
    with_session(0, |s| s.tz_offset_seconds())
}

#[unsafe(no_mangle)]
pub extern "C" fn hosted_signal_install_handler() {
    with_session((), |s| s.signal_install());
}

#[unsafe(no_mangle)]
pub extern "C" fn hosted_signal_take() -> i64 {
    with_session(0, |s| s.signal_take())
}

#[unsafe(no_mangle)]
pub extern "C" fn hosted_tty_is_terminal(_fd: u64) -> bool {
    // D36: the sandbox has no terminal, so this is the constant false.
    false
}

#[unsafe(no_mangle)]
pub extern "C" fn hosted_tty_enable_raw_mode() {
    // D36: no terminal, so raw mode is a no-op rather than an error.
}

#[unsafe(no_mangle)]
pub extern "C" fn hosted_tty_disable_raw_mode() {}
