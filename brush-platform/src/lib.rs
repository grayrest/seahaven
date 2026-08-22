//! The Roc platform's hosted effects, expressed once as a Rust trait (D18).
//!
//! seahaven is meant to replace `basic-cli` as `rocjust`'s platform. `basic-cli`
//! exposes ~72 *hosted effects* — `file_read_utf8!`, `dir_list!`, `cmd_exec!`
//! and the rest — as `extern "C"` functions the Roc compiler links. This crate
//! is the same surface, minus what a closed world removes, routed through the
//! sandbox instead of through `std::fs`.
//!
//! # One trait, so the tiers cannot drift
//!
//! D18 says one Roc API binds two ways: wasm imports in the untrusted tier,
//! direct calls in the trusted one. D19 says both tiers are equally capable.
//! Neither claim is worth anything as prose. As a **trait** they are worth
//! something the moment there is more than one implementor — and until then,
//! the trait earns its place a different way: it is the seam a test can drive.
//!
//! [`PlatformEffects`] is that trait. [`VfsPlatform`] is the one implementation
//! that exists today, over a [`brush_vfs::Session`]. The differential gate
//! ([`tests/differential.rs`]) drives the trait under a restrictive mount and
//! under the identity policy and requires the two to agree — which is the only
//! end-to-end confinement property this milestone can check without the Roc
//! toolchain, and the reason the trait is here rather than a bare set of
//! functions.
//!
//! # Paths are strings (D45)
//!
//! Every path crosses this boundary as a `&str`. `basic-cli`'s `NativePath` is a
//! three-way union — UTF-8, raw Unix bytes, raw Windows wide chars — and D45
//! rejects it: the three hosts disagree about what a filename is, so the
//! namespace takes the intersection, and a host name that is not valid UTF-8 is
//! *not a path on this platform*. A path the grammar rejects is unnameable, in
//! the same category [`PlatformError::NotFound`] already covers.

#![deny(missing_docs)]

mod cmd;
mod effects;
mod error;
pub mod facts;
pub mod runtime;
pub mod stdio;
mod vfs_host;

pub use cmd::{Cmd, Executor, Exit, Finished, IoPlan, JobHandle, OutputMode, RunResult, StdinMode};
pub use effects::{ExecOutput, PathKind, PlatformEffects};
pub use error::PlatformError;
pub use facts::{PlatformTarget, SessionFacts, TargetArch, TargetOs};
pub use runtime::{Clock, Rng, SignalQueue, SystemClock, SystemRng};
pub use stdio::{OutputLog, StdinSource, Stdio, Stream};
pub use vfs_host::VfsPlatform;
