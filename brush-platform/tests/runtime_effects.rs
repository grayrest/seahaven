//! Step 6: signals, clock, and RNG through the trait (D36, D15, D14).

#![cfg(unix)]
#![cfg(test)]
#![allow(
    clippy::disallowed_methods,
    clippy::expect_used,
    reason = "the test builds a session on the host, which is the point"
)]

use brush_platform::runtime::signal;
use brush_platform::{Clock, PlatformEffects, Rng, SessionFacts, VfsPlatform};
use brush_vfs::{Policy, Session, Vfs};

fn host() -> VfsPlatform {
    let mounts = Policy::identity().expect("identity");
    let session = Session::new(std::sync::Arc::new(Vfs::new(mounts)));
    VfsPlatform::new(session, SessionFacts::neutral())
}

/// A pinned clock — what D14's hermetic mode installs.
struct FixedClock {
    nanos: u128,
    offset: i64,
}
impl Clock for FixedClock {
    fn now_nanos(&self) -> Option<u128> {
        Some(self.nanos)
    }
    fn tz_offset_seconds(&self) -> i64 {
        self.offset
    }
}

/// A seeded RNG — deterministic, for hermetic mode.
struct CountingRng(u64);
impl Rng for CountingRng {
    fn next_u64(&mut self) -> u64 {
        self.0 += 1;
        self.0
    }
}

#[test]
fn a_host_signal_is_not_forwarded_but_the_sandbox_can_deliver() {
    // The step-6 decision, stated: the queue's only entry point is the
    // sandbox's `deliver_signal`. There is no path from a real host SIGINT into
    // it, so an ordinary run polls `0` -- which is exactly what the plan says.
    let mut host = host();
    host.signal_install();
    assert_eq!(
        host.signal_take(),
        0,
        "an ordinary run sees no signal: the host's are not forwarded"
    );

    // The sandbox's own path -- a job finishing, a shutdown request -- does
    // reach the guest.
    host.deliver_signal(signal::SIGTERM);
    assert_eq!(host.signal_take(), signal::SIGTERM);
    assert_eq!(host.signal_take(), 0, "and reading clears it");
}

#[test]
fn delivery_needs_an_installed_handler() {
    let mut host = host();
    // No install yet: a delivered signal has nowhere to land.
    host.deliver_signal(signal::SIGINT);
    host.signal_install();
    assert_eq!(
        host.signal_take(),
        0,
        "a signal delivered before install does not survive it"
    );
}

#[test]
fn the_clock_reads_the_session_source() {
    // Default: real time, non-zero, and a UTC offset (D33 keeps TZ a session
    // fact, so the default host offset is not read).
    let live = host();
    assert!(live.utc_now().expect("now") > 0);
    assert_eq!(live.tz_offset_seconds(), 0);

    // Hermetic: the clock is pinned, and the effect reports exactly it.
    let pinned = host().with_clock(Box::new(FixedClock {
        nanos: 1_700_000_000_000_000_000,
        offset: -5 * 3600,
    }));
    assert_eq!(pinned.utc_now().expect("now"), 1_700_000_000_000_000_000);
    assert_eq!(pinned.tz_offset_seconds(), -5 * 3600);
}

#[test]
fn the_rng_reads_the_session_source() {
    // Default: unpredictable -- two draws differ (a 1-in-2^64 collision would
    // read as the RNG being stuck).
    let mut live = host();
    assert_ne!(
        live.random_seed_u64().expect("rng"),
        live.random_seed_u64().expect("rng")
    );

    // Hermetic: a seeded RNG is deterministic, which is the whole point of D14 --
    // a reproducible run. The default must never be this.
    let mut seeded = host().with_rng(Box::new(CountingRng(0)));
    assert_eq!(seeded.random_seed_u64().expect("rng"), 1);
    assert_eq!(seeded.random_seed_u64().expect("rng"), 2);
}
