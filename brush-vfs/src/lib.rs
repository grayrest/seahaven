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
//!
//! # The namespace's own directories are not directories
//!
//! Every path resolved here is backed by a host object, and the shape of the
//! namespace is not. `/` exists because something was mounted beneath it, and
//! nothing on any host *is* `/`; the same holds for any ancestor of a mount
//! point, so mounting `/deep/nested` leaves `/deep` nothing at all.
//!
//! In practice: under a policy that does not mount `/` itself, `/` does not
//! [`exists`](Vfs::exists), has no [`facts`](Vfs::facts), cannot be listed and
//! cannot be a working directory — which is why [`Session::set_cwd`] refuses it
//! and why a shell lands at its shallowest mount point instead.
//! [`MountTable`]'s `has_mount_below` lets a *traversal* step through such a
//! directory, because a walk must, but only while more components follow.
//!
//! This is a decision (D6), not an oversight: synthesising these directories
//! means fabricating metadata that no inode backs. It is pinned by tests so it
//! cannot quietly become one.

#![deny(missing_docs)]

pub mod ambient;
pub mod dir;
pub mod fs;
pub mod mount;
pub mod path;
pub mod session;
pub mod walk;

pub use fs::{AccessModes, FileFacts, OpenMode, Vfs};
pub use mount::{Access, Mount, MountError, MountHandle, MountLoan, MountTable};
pub use path::{PathError, VirtualPath};
pub use session::{Policy, Session};
