//! Platform abstraction facilities
//!
//! # Namespace exemption
//!
//! This is the only part of `brush-core` allowed to reach the host filesystem
//! directly, and the exemption is narrow by construction: nothing here decides
//! *whether* a path may be named. It answers questions the namespace itself
//! has to ask -- the mode bits behind a `Metadata`, the process's own
//! descriptor table -- and the shell reaches it only through `brush-vfs` or
//! through the handful of platform helpers that take no path at all.
//!
//! Everything above this module goes through the namespace; see the ban list
//! in `clippy.toml` and `cargo xtask check ban`.

#![allow(unused)]
#![allow(
    clippy::disallowed_methods,
    reason = "the platform layer is where host access is implemented; see the module docs"
)]

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
