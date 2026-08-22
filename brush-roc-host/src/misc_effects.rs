//! The regex, random, and clock hosted effects.
//!
//! Regex is pure computation over its `Str` arguments — no path, no namespace —
//! and its error is the engine's own message. Random and the clock read the
//! session's sources (D15), so hermetic mode (D14) pins them; rocjust imports
//! neither, but the platform exposes them, so the symbols exist and route the
//! same way.

use core::mem::ManuallyDrop;

use brush_platform::PlatformEffects;

use crate::marshal::{effect, host, io_err, rocstr_into_string};
use crate::roc_platform_abi::{
    HostRandomSeedU32Result, HostRandomSeedU32ResultPayload, HostRandomSeedU32ResultTag,
    HostRandomSeedU64Result, HostRandomSeedU64ResultPayload, HostRandomSeedU64ResultTag,
    HostRegexIsMatchResult, HostRegexIsMatchResultPayload, HostRegexIsMatchResultTag,
    HostRegexReplaceAllResult, HostRegexReplaceAllResultPayload, HostRegexReplaceAllResultTag,
    HostUtcNowResult, HostUtcNowResultPayload, HostUtcNowResultTag, RocStr,
};

// --- Regular expressions ---------------------------------------------------

#[unsafe(no_mangle)]
pub extern "C" fn hosted_regex_is_match(
    pattern: RocStr,
    haystack: RocStr,
) -> HostRegexIsMatchResult {
    let host = host();
    // Both arguments are owned; release them before returning either way.
    let pattern = rocstr_into_string(pattern, &host);
    let haystack = rocstr_into_string(haystack, &host);
    match effect(|s| s.regex_is_match(&pattern, &haystack).map_err(regex_err)) {
        Ok(matched) => HostRegexIsMatchResult {
            payload: HostRegexIsMatchResultPayload {
                ok: ManuallyDrop::new(matched),
            },
            tag: HostRegexIsMatchResultTag::Ok,
        },
        Err(message) => HostRegexIsMatchResult {
            payload: HostRegexIsMatchResultPayload {
                err: ManuallyDrop::new(RocStr::from_str(&regex_message(&message), &host)),
            },
            tag: HostRegexIsMatchResultTag::Err,
        },
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn hosted_regex_replace_all(
    pattern: RocStr,
    haystack: RocStr,
    replacement: RocStr,
) -> HostRegexReplaceAllResult {
    let host = host();
    let pattern = rocstr_into_string(pattern, &host);
    let haystack = rocstr_into_string(haystack, &host);
    let replacement = rocstr_into_string(replacement, &host);
    match effect(|s| {
        s.regex_replace_all(&pattern, &haystack, &replacement)
            .map_err(regex_err)
    }) {
        Ok(replaced) => HostRegexReplaceAllResult {
            payload: HostRegexReplaceAllResultPayload {
                ok: ManuallyDrop::new(RocStr::from_str(&replaced, &host)),
            },
            tag: HostRegexReplaceAllResultTag::Ok,
        },
        Err(message) => HostRegexReplaceAllResult {
            payload: HostRegexReplaceAllResultPayload {
                err: ManuallyDrop::new(RocStr::from_str(&regex_message(&message), &host)),
            },
            tag: HostRegexReplaceAllResultTag::Err,
        },
    }
}

// The regex trait method returns `Result<_, String>`, so the engine's message
// rides through the effect layer as a `PlatformError::Other`. These two helpers
// carry it in and back out without losing it.
fn regex_err(message: String) -> brush_platform::PlatformError {
    brush_platform::PlatformError::Other(message)
}

fn regex_message(error: &brush_platform::PlatformError) -> String {
    match error {
        brush_platform::PlatformError::Other(message) => message.clone(),
        other => other.to_string(),
    }
}

// --- Random ----------------------------------------------------------------

#[unsafe(no_mangle)]
pub extern "C" fn hosted_random_seed_u64() -> HostRandomSeedU64Result {
    let host = host();
    match effect(|s| s.random_seed_u64()) {
        Ok(value) => HostRandomSeedU64Result {
            payload: HostRandomSeedU64ResultPayload {
                ok: ManuallyDrop::new(value),
            },
            tag: HostRandomSeedU64ResultTag::Ok,
        },
        Err(error) => HostRandomSeedU64Result {
            payload: HostRandomSeedU64ResultPayload {
                err: ManuallyDrop::new(io_err(&error, &host)),
            },
            tag: HostRandomSeedU64ResultTag::Err,
        },
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn hosted_random_seed_u32() -> HostRandomSeedU32Result {
    let host = host();
    match effect(|s| s.random_seed_u32()) {
        Ok(value) => HostRandomSeedU32Result {
            payload: HostRandomSeedU32ResultPayload {
                ok: ManuallyDrop::new(value),
            },
            tag: HostRandomSeedU32ResultTag::Ok,
        },
        Err(error) => HostRandomSeedU32Result {
            payload: HostRandomSeedU32ResultPayload {
                err: ManuallyDrop::new(io_err(&error, &host)),
            },
            tag: HostRandomSeedU32ResultTag::Err,
        },
    }
}

// --- Clock -----------------------------------------------------------------

#[unsafe(no_mangle)]
pub extern "C" fn hosted_utc_now() -> HostUtcNowResult {
    // The error is unit (`ClockBeforeEpoch`), so a clock before the epoch is
    // simply the `Err`; the session's clock supplies the value (D15).
    match effect(|s| s.utc_now()) {
        Ok(nanos) => HostUtcNowResult {
            payload: HostUtcNowResultPayload {
                ok: ManuallyDrop::new(nanos),
            },
            tag: HostUtcNowResultTag::Ok,
        },
        Err(_) => HostUtcNowResult {
            payload: HostUtcNowResultPayload { err: [] },
            tag: HostUtcNowResultTag::Err,
        },
    }
}
