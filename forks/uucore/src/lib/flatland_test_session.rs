// FLATLAND DIVERGENCE from upstream — see `plans/2026-08-18-uucore-fork.md`.
//
// Installs an identity vfs session before this crate's own test binary runs.
//
// # Why this has to exist
//
// The codemod routes production filesystem calls through `brush_vfs::ambient`,
// which **fails closed**: with no session installed it refuses every access.
// That is the correct production behaviour and is not negotiable. But upstream's
// own tests call the routed production functions directly, and `cargo test`
// installs no session, so without this hook 11 of them fail with
// "no vfs session is installed" — including every `create_dir_all_safe` case,
// which is precisely the code the hand-routing in step 7 touches.
//
// D13 makes the upstream suite a health metric, and a metric that reports 11
// permanent failures measures nothing. The alternative considered was recording
// them as expected failures; it was rejected because it would go dark over the
// code most in need of the coverage, and because the interesting question is
// exactly what these tests answer: *is the routing semantics-preserving?* Under
// an identity session the answer must be "yes, byte for byte", and any
// divergence is a real bug rather than a mute entry in a list.
//
// # Why it cannot weaken production
//
// This module is `#[cfg(test)]`. `cfg(test)` is enabled only when compiling
// *this crate's own* test harness — never when `brush-coreutils-builtins`,
// `brush-builtins`, or any of the 77 unforked `uu_*` crates build `uucore` as a
// dependency. So the code does not exist in a production build: it is not
// disabled at runtime by a flag someone could flip, it is absent from the
// binary. `ctor` is likewise a dev-dependency and never enters a shipped graph.
//
// The session installed is the *identity* policy — host `/` mounted
// read-write — so routed calls behave exactly as the unrouted ones did and the
// upstream assertions stay meaningful. It confines nothing, which is right for a
// test of upstream's logic; confinement is proven separately by `brush-vfs`'s own
// escape suite and by `brush-coreutils-builtins/tests/routing.rs`, which install
// restrictive policies on purpose.

use std::sync::Arc;

/// Installs the identity session before any test runs.
///
/// A constructor rather than a per-test call because upstream's test bodies are
/// not ours to edit (D13) — the marker for a deliberate divergence lives outside
/// the case, not inside it.
#[ctor::ctor]
fn install_identity_session_for_tests() {
    let Ok(mounts) = brush_vfs::Policy::identity() else {
        // Nothing to do if the host root cannot be opened; the tests that need
        // the namespace will fail with the fail-closed error and say so.
        return;
    };
    let mut session = brush_vfs::Session::new(Arc::new(brush_vfs::Vfs::new(mounts)));
    if let Ok(cwd) = std::env::current_dir() {
        let _ = session.set_cwd(&cwd.to_string_lossy());
    }
    brush_vfs::ambient::install(session);
}
