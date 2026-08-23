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
//!    the whole milestone built. The scalar effects live here; the refcounted
//!    ones — every effect marshalling Roc's `Str`/`List` — are in the
//!    per-category modules ([`fs_effects`], [`env_effects`], [`io_effects`],
//!    [`cmd_effects`], [`misc_effects`]), all sharing the marshalling discipline
//!    in [`marshal`].
//!
//! # The two allocators must be one
//!
//! Roc-generated code calls the exported `roc_alloc` linker symbol; host-side
//! helpers (`RocStr::from_str`, `RocList::allocate`) allocate through the
//! [`RocHost`](roc_platform_abi::RocHost) that [`roc_host`] hands back. A value
//! allocated on one side is freed on the other — a host-built `RocStr` passed
//! into `roc_main`, a Roc-built `RocStr` returned from an effect — so the two
//! paths must be the *same* allocator or a free reads a header that is not
//! there. Both therefore delegate to the glue's `DefaultAllocators`; the
//! exported symbols below are thin forwarders, not an independent `malloc`.

#![allow(clippy::missing_safety_doc)]

use core::ffi::c_void;
use std::cell::RefCell;

use brush_platform::{PlatformEffects, VfsPlatform};

// Used only by the `#[cfg(not(test))]` entrypoint below.
#[cfg(not(test))]
use brush_platform::{PlatformTarget, SessionFacts, TargetArch, TargetOs};
#[cfg(not(test))]
use brush_vfs::{Policy, Session, Vfs};
#[cfg(not(test))]
use core::ffi::c_char;

#[allow(warnings)]
mod roc_platform_abi;

mod cmd_effects;
mod env_effects;
mod fs_effects;
mod io_effects;
mod marshal;
mod misc_effects;

#[cfg(test)]
mod host_tests;

/// Installs a session for the marshalling tests, standing in for [`main`]'s
/// setup. Test-only: the effects resolve against whatever fixture the test built.
#[cfg(test)]
pub(crate) fn install_session_for_test(platform: VfsPlatform) {
    SESSION.with_borrow_mut(|slot| *slot = Some(platform));
}

use roc_platform_abi::{DefaultAllocators, DefaultHandlers, RocHost, make_roc_host};

// Used only by the `#[cfg(not(test))]` entrypoint below.
#[cfg(not(test))]
use marshal::roc_u8_list_from_slice;
#[cfg(not(test))]
use roc_platform_abi::{OsStr, OsStrPayload, OsStrTag, RocList};

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
pub(crate) fn with_session<T>(default: T, f: impl FnOnce(&mut VfsPlatform) -> T) -> T {
    SESSION.with_borrow_mut(|slot| slot.as_mut().map_or(default, f))
}

/// The host allocation context the glue helpers need.
///
/// Reconstructed on demand rather than stored: it is a struct of fixed function
/// pointers into the glue's `DefaultAllocators`, so building one is free and it
/// needs no global. `env` is null because those allocators ignore it.
pub(crate) fn roc_host() -> RocHost {
    make_roc_host(core::ptr::null_mut())
}

// --- Roc runtime symbols ---------------------------------------------------
//
// Thin forwarders to the glue's defaults, so the exported symbols Roc calls and
// the host-side helpers share one allocator (see the module docs).

#[unsafe(no_mangle)]
pub unsafe extern "C" fn roc_alloc(length: usize, alignment: usize) -> *mut c_void {
    DefaultAllocators::roc_alloc(core::ptr::null_mut(), length, alignment)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn roc_dealloc(ptr: *mut c_void, alignment: usize) {
    DefaultAllocators::roc_dealloc(core::ptr::null_mut(), ptr, alignment);
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn roc_realloc(
    ptr: *mut c_void,
    new_length: usize,
    alignment: usize,
) -> *mut c_void {
    DefaultAllocators::roc_realloc(core::ptr::null_mut(), ptr, new_length, alignment)
}

/// Debug output — routed to the glue's handler, which writes to stderr.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn roc_dbg(bytes: *const u8, len: usize) {
    DefaultHandlers::roc_dbg(core::ptr::null_mut(), bytes, len);
}

/// A failed `expect` — reported like `roc_dbg`; execution continues.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn roc_expect_failed(bytes: *const u8, len: usize) {
    DefaultHandlers::roc_expect_failed(core::ptr::null_mut(), bytes, len);
}

/// The Roc program crashed. Report and abort — there is no unwinding across the
/// C ABI, and under D25 a crash must not be turned into a silent success.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn roc_crashed(bytes: *const u8, len: usize) {
    DefaultHandlers::roc_crashed(core::ptr::null_mut(), bytes, len);
}

// --- Entrypoint ------------------------------------------------------------
//
// The whole entrypoint is `#[cfg(not(test))]`: the test harness supplies its own
// `main`, `roc_main` is an unresolved symbol until the Roc object is linked, and
// the marshalling tests install a fixture session directly. In a test build none
// of this is reachable, so gating it keeps both configurations warning-clean.

#[cfg(not(test))]
unsafe extern "C" {
    /// The compiled Roc `main_for_host!`. Provided by the linked Roc object.
    fn roc_main(args: RocList<OsStr>) -> i32;
}

/// The process entrypoint. Dispatches a bundled utility if invoked as one;
/// otherwise installs the session and runs the Roc app.
///
/// This binary is its own bundled trampoline (D30): a confined `Cmd` runs a
/// utility by re-invoking *this* executable as `<self> --invoke-bundled <name>`,
/// and that re-invocation is caught here — before any Roc code runs — by the
/// launcher's `maybe_dispatch`. The child receives the parent's namespace over
/// the broker (D24) and runs one utility confined to it. A normal invocation
/// falls through to `roc_main`.
///
/// Gated out of test builds: the test harness supplies its own `main`, and the
/// marshalling tests install a fixture session directly rather than through this.
#[cfg(not(test))]
#[unsafe(no_mangle)]
pub extern "C" fn main(argc: i32, argv: *const *const c_char) -> i32 {
    // Register the bundled coreutils, then take the dispatch fast path if this
    // is a `--invoke-bundled` re-invocation. Both must happen before any Roc
    // code, exactly as the shell's own entrypoint orders them. Absent without
    // the `broker-executor` feature, where `Cmd` is `Unsupported` and there is
    // nothing to dispatch.
    #[cfg(feature = "broker-executor")]
    {
        brush_shell::bundled::install_default_providers();
        if let Some(code) = brush_shell::bundled::maybe_dispatch() {
            return code;
        }
    }

    install_identity_session();
    let host = roc_host();
    let args = build_args_list(argc, argv, &host);
    let code = unsafe { roc_main(args) };

    // Drain the guest's buffered output to the real streams (D28). The stdio
    // effects append to the session's output log rather than a descriptor -- the
    // buffering the guest cannot defeat -- so the *host* is the only thing that
    // can emit it, and this is where it does, once, before exit.
    flush_guest_output();
    code
}

/// Writes the session's buffered stdout and stderr (D28) to the real streams.
///
/// stdout and stderr are kept separate on the wire (D28), so each goes to its
/// own descriptor; a caller comparing them apart -- rocjust's harness does -- sees
/// exactly what the guest wrote to each.
#[cfg(not(test))]
fn flush_guest_output() {
    use std::io::Write as _;
    let (out, err) = with_session((Vec::new(), Vec::new()), |s| {
        (s.output().stdout(), s.output().stderr())
    });
    let _ = std::io::stdout().write_all(&out);
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().write_all(&err);
    let _ = std::io::stderr().flush();
}

#[cfg(not(test))]
/// Marshals the process argument vector into a `RocList<OsStr>`.
///
/// Each argument becomes a `Utf8` `OsStr` (D45: strings on this platform); a
/// non-UTF-8 argument is carried losslessly as its bytes so it still round-trips
/// through the guest's `OsStr` handling.
fn build_args_list(argc: i32, argv: *const *const c_char, host: &RocHost) -> RocList<OsStr> {
    if argc <= 0 || argv.is_null() {
        return RocList::empty();
    }
    let count = argc as usize;
    let list = unsafe { RocList::<OsStr>::allocate(count, host) };
    for index in 0..count {
        let element = unsafe {
            let arg_ptr = *argv.add(index);
            if arg_ptr.is_null() {
                os_str_empty()
            } else {
                let bytes = core::ffi::CStr::from_ptr(arg_ptr).to_bytes();
                os_str_from_bytes(bytes, host)
            }
        };
        unsafe { list.elements.add(index).write(element) };
    }
    list
}

#[cfg(not(test))]
/// A `Utf8` `OsStr` when the bytes are valid UTF-8, else a `UnixBytes` one.
fn os_str_from_bytes(bytes: &[u8], host: &RocHost) -> OsStr {
    match core::str::from_utf8(bytes) {
        Ok(text) => OsStr {
            payload: OsStrPayload {
                utf8: core::mem::ManuallyDrop::new(roc_platform_abi::RocStr::from_str(text, host)),
            },
            tag: OsStrTag::Utf8,
        },
        Err(_) => OsStr {
            payload: OsStrPayload {
                unix_bytes: core::mem::ManuallyDrop::new(roc_u8_list_from_slice(bytes, host)),
            },
            tag: OsStrTag::UnixBytes,
        },
    }
}

#[cfg(not(test))]
/// An empty `Utf8` `OsStr`, for a null argv slot.
fn os_str_empty() -> OsStr {
    OsStr {
        payload: OsStrPayload {
            utf8: core::mem::ManuallyDrop::new(roc_platform_abi::RocStr::empty()),
        },
        tag: OsStrTag::Utf8,
    }
}

#[cfg(not(test))]
/// Installs an identity session so the pipeline can run, with the broker-backed
/// executor bound so `Cmd` effects run confined children (D24). A launcher
/// replaces the identity policy with a grant-derived one (D44) at the confined
/// tier; the executor it installs is the same.
///
/// This is the **unconfined** tier: with no launcher to choose a grant, the
/// session starts where the process is and reports the host's own environment
/// and platform (see [`host_identity_facts`]). Confinement here is exactly the
/// execution model — `Cmd` still routes through the broker (or is `Unsupported`
/// without it) — and nothing else, which is what makes an identity run a clean
/// baseline for what the execution boundary alone costs.
fn install_identity_session() {
    let Ok(mounts) = Policy::identity() else {
        return;
    };
    let mut session = Session::new(std::sync::Arc::new(Vfs::new(mounts)));

    // Start the session where the process actually is, so relative paths and a
    // CLI's upward file discovery resolve as they would unconfined. Under
    // identity the host and virtual spellings coincide.
    if let Ok(cwd) = std::env::current_dir() {
        let _ = session.set_cwd(&cwd.to_string_lossy());
    }

    #[cfg_attr(not(feature = "broker-executor"), expect(unused_mut))]
    let mut platform = VfsPlatform::new(session.clone(), host_identity_facts());

    // The executor serves this same namespace to every child and re-invokes this
    // binary as the trampoline (see `main`). Without the `broker-executor`
    // feature `Cmd` is `Unsupported`; with it, a bundled utility (or a `sh`/`bash`
    // recipe body) runs confined.
    #[cfg(feature = "broker-executor")]
    if let Ok(executor) = brush_broker_exec::BrokerExecutor::for_current_exe(
        session,
        brush_shell::bundled::DISPATCH_FLAG,
    ) {
        platform = platform.with_executor(Box::new(executor));
    }

    SESSION.with_borrow_mut(|slot| *slot = Some(platform));
}

/// The facts an unconfined identity session reports: the host's own.
///
/// The confined tier's launcher chooses these by policy (D21); the identity tier
/// has no launcher, so it reports the truth — the real environment, platform,
/// pid and parallelism — which is what an unconfined CLI would see. Reading the
/// host directly is exactly what identity means, and is why the confined tier
/// exists to *not* do it.
#[cfg(not(test))]
fn host_identity_facts() -> SessionFacts {
    let os = match std::env::consts::OS {
        "linux" => TargetOs::Linux,
        "macos" => TargetOs::MacOs,
        "windows" => TargetOs::Windows,
        other => TargetOs::Other(other.to_owned()),
    };
    let arch = match std::env::consts::ARCH {
        "x86" => TargetArch::X86,
        "x86_64" => TargetArch::X64,
        "arm" => TargetArch::Arm,
        "aarch64" => TargetArch::Aarch64,
        other => TargetArch::Other(other.to_owned()),
    };
    SessionFacts {
        env: std::env::vars().collect(),
        platform: PlatformTarget { os, arch },
        pid: i64::from(std::process::id()),
        num_cpus: std::thread::available_parallelism()
            .map_or(1, |n| i64::try_from(n.get()).unwrap_or(1)),
        temp_dir: std::env::temp_dir().to_string_lossy().into_owned(),
    }
}

// --- Scalar effects (no refcounted marshalling) ----------------------------
//
// These take and return only machine scalars, so they need no marshalling: each
// reads or drives the session directly. The refcounted effects live in the
// per-category modules and follow the discipline in `marshal`.

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
    with_session((), PlatformEffects::signal_install);
}

#[unsafe(no_mangle)]
pub extern "C" fn hosted_signal_take() -> i64 {
    with_session(0, PlatformEffects::signal_take)
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
