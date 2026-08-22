//! The pure-computation effects added with the Roc host: regex and the 32-bit
//! random draw (D18 keeps them on the one effect trait even though they touch no
//! namespace).

#![cfg(unix)]
#![cfg(test)]
#![allow(
    clippy::disallowed_methods,
    clippy::expect_used,
    reason = "the test builds a session on the host, which is the point"
)]

use brush_platform::{PlatformEffects, Rng, SessionFacts, VfsPlatform};
use brush_vfs::{Policy, Session, Vfs};

fn host() -> VfsPlatform {
    let mounts = Policy::identity().expect("identity");
    let session = Session::new(std::sync::Arc::new(Vfs::new(mounts)));
    VfsPlatform::new(session, SessionFacts::neutral())
}

#[test]
fn regex_matches_and_reports_a_bad_pattern() {
    let host = host();
    assert!(host.regex_is_match("^h.*o$", "hello").expect("match"));
    assert!(!host.regex_is_match("^x", "hello").expect("no match"));

    // A pattern that does not compile is the engine's message, not a panic.
    let error = host
        .regex_is_match("(unclosed", "x")
        .expect_err("bad pattern");
    assert!(!error.is_empty(), "the compile error carries a message");
}

#[test]
fn regex_replace_all_replaces_every_match() {
    let host = host();
    assert_eq!(
        host.regex_replace_all("a", "banana", "o").expect("replace"),
        "bonono"
    );
    assert!(
        host.regex_replace_all("(", "x", "y").is_err(),
        "a bad pattern is an error, not a silent no-op"
    );
}

/// A seeded RNG so the draw is deterministic.
struct CountingRng(u64);
impl Rng for CountingRng {
    fn next_u64(&mut self) -> u64 {
        self.0 += 1;
        self.0
    }
}

#[test]
fn random_seed_u32_is_the_low_half_of_the_same_source() {
    // Both draws read the one RNG (D14 pins them together). A seeded source
    // makes that observable: the u32 is the low 32 bits of the next u64.
    // The RNG increments before returning, so it starts one below the value we
    // want to observe first.
    let mut host = host().with_rng(Box::new(CountingRng(0xFFFF_FFFE)));
    // First next_u64() -> 0x0000_0000_FFFF_FFFF, low half is 0xFFFF_FFFF.
    assert_eq!(host.random_seed_u32().expect("u32"), 0xFFFF_FFFF);
    // Second next_u64() -> 0x0000_0001_0000_0000, low half is 0.
    assert_eq!(host.random_seed_u32().expect("u32"), 0);
}
