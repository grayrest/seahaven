//! Local time without a timezone-database dependency.
//!
//! `chrono::Local` pulls `iana-time-zone`, whose macOS backend calls
//! CoreFoundation -- a framework a static host linked into a Roc binary cannot
//! resolve (there is no way to pass `-framework CoreFoundation` through the Roc
//! toolchain). The shell only ever needs local time to *format* it -- the
//! `\d`/`\t` prompt escapes and `history`'s timestamps -- so instead of a
//! timezone database, the offset is read from the C library and applied as a
//! [`FixedOffset`]. That formats identically and needs neither a database nor a
//! framework, which lets `brush-core` drop chrono's `clock` feature.

use chrono::{DateTime, FixedOffset, Offset, Utc};

/// The local timezone's current offset from UTC, read from the C library.
///
/// Falls back to UTC if the offset cannot be determined (or off Unix, where
/// there is no `localtime_r`).
#[must_use]
pub fn local_offset() -> FixedOffset {
    FixedOffset::east_opt(offset_east_seconds()).unwrap_or_else(|| Utc.fix())
}

/// The current instant in local time.
#[must_use]
pub fn now_local() -> DateTime<FixedOffset> {
    Utc::now().with_timezone(&local_offset())
}

#[cfg(unix)]
fn offset_east_seconds() -> i32 {
    // SAFETY: `time` accepts a null pointer and returns the current time by value.
    let now = unsafe { libc::time(std::ptr::null_mut()) };
    // SAFETY: `libc::tm` is a plain integer struct; an all-zero value is valid.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: both pointers address live local storage; `localtime_r` reads the
    // `time_t` and writes the `tm`, returning null only on failure.
    let filled = unsafe { libc::localtime_r(std::ptr::addr_of!(now), std::ptr::addr_of_mut!(tm)) };
    if filled.is_null() {
        return 0;
    }
    i32::try_from(tm.tm_gmtoff).unwrap_or(0)
}

#[cfg(not(unix))]
fn offset_east_seconds() -> i32 {
    0
}
