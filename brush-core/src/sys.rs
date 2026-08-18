//! Platform abstraction facilities
//!
//! # Namespace exemption
//!
//! This is the only part of `brush-core` allowed to reach the host filesystem
//! directly, and the exemption is written per call site rather than over the
//! module. It used to be a blanket `#![allow]` across the whole tree, which is
//! a poor way to state a narrow rule: it made two dead helpers that took a
//! caller-supplied path and called `metadata()` on it invisible for as long as
//! they existed, which is exactly the shape the ban exists to catch.
//!
//! On Unix the whole exemption is three sites: the null device, which is in no
//! mount and must work under every policy, and `/proc/self/fd`, which is
//! process introspection wearing a filesystem's clothes. Nothing here decides
//! *whether* a path may be named.
//!
//! Everything above this module goes through the namespace; see the ban list
//! in `clippy.toml` and `cargo xtask check ban`.

#![allow(unused)]

#[cfg(unix)]
pub(crate) mod unix;
#[cfg(unix)]
pub(crate) use unix as platform;

#[cfg(windows)]
pub(crate) mod windows;
#[cfg(windows)]
pub(crate) use windows as platform;

#[cfg(not(unix))]
pub(crate) mod stubs;

#[cfg(any(unix, windows))]
pub(crate) mod hostname;
#[cfg(any(unix, windows))]
pub mod tokio_process;

pub mod fs;

pub use platform::async_pipe;
pub use platform::commands;
pub(crate) use platform::env;
pub use platform::fd;
pub use platform::input;
pub(crate) use platform::network;
pub use platform::poll;
pub use platform::process;
pub use platform::resource;
pub use platform::signal;
pub use platform::terminal;
pub(crate) use platform::users;

pub use platform::PlatformError;
