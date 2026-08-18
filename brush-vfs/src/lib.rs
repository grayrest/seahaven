//! Virtual filesystem namespace for brush.
//!
//! The sandbox presents sandboxed code with a namespace of its own rather than
//! the host's: a single virtual root composed from mounts. Host paths are
//! *constructed* here and never accepted from a caller, which is what makes an
//! escape unrepresentable rather than merely rejected — there is no syntax for
//! naming a host path.
//!
//! That property is why the grammar in [`path`] is strict about constructs that
//! are legal on some host and not others. A namespace that means the same thing
//! on Linux, macOS and Windows cannot inherit any one of their path dialects.

#![deny(missing_docs)]

pub mod path;

pub use path::{PathError, VirtualPath};
