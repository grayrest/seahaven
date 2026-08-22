//! The error a hosted effect returns, matching `basic-cli`'s `IOErr`.

/// Why a hosted effect failed.
///
/// The variants are `basic-cli`'s `IOErr` tag union, so the Roc side maps onto
/// them without a translation layer. The mapping *into* them is where the
/// security-visible decisions live, and there is one that matters:
///
/// **A path outside the grant returns [`NotFound`](PlatformError::NotFound),
/// not [`PermissionDenied`](PlatformError::PermissionDenied).** D6 makes an
/// out-of-namespace path unnameable, and the honest report for a name that
/// cannot be named is that nothing is there — the same answer a real miss
/// gives. `PermissionDenied` would confirm that *something* exists at a host
/// path the guest was never allowed to learn about, which is an existence
/// oracle. The vfs already reports an unmounted path as `NotFound`; this type
/// preserves that rather than widening it.
///
/// A path the *grammar* rejects (D12/D45 — a colon, a Windows reserved name, a
/// non-UTF-8 byte) is unnameable for a different reason and gets the same
/// answer, for the same purpose: an unnameable path is not-found, however it
/// came to be unnameable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlatformError {
    /// The target already exists, where the effect required it not to.
    AlreadyExists,
    /// A pipe was closed at the far end.
    BrokenPipe,
    /// The operation was interrupted.
    Interrupted,
    /// A directory was found where a non-directory was required.
    IsADirectory,
    /// Nothing is there — including everything outside the grant, and every
    /// path the grammar refuses. See the type docs.
    NotFound,
    /// A non-directory was found where a directory was required.
    NotADirectory,
    /// Anything without a more specific variant, carrying a message.
    ///
    /// The message is virtual-path-only by construction: it is either a fixed
    /// string or one the vfs produced, and the vfs's error strings name virtual
    /// paths, never host ones.
    Other(String),
    /// The operation could not allocate.
    OutOfMemory,
    /// The operation is not permitted — a write to a read-only mount, an
    /// `access(2)` that answered no. Distinct from an unnameable path: this is a
    /// path the guest *can* name and *cannot* act on, which discloses nothing it
    /// did not already have.
    PermissionDenied,
    /// The operation is not supported here.
    Unsupported,
}

impl PlatformError {
    /// Maps a `std::io::Error` from the vfs onto the wire type.
    ///
    /// The vfs has already done the confinement work — an unmounted path is
    /// `NotFound`, a read-only write is `PermissionDenied` — so this is a
    /// kind-to-variant map, with `Other` carrying the message for kinds that
    /// have no dedicated tag.
    pub(crate) fn from_io(error: &std::io::Error) -> Self {
        use std::io::ErrorKind;
        match error.kind() {
            ErrorKind::NotFound => Self::NotFound,
            ErrorKind::AlreadyExists => Self::AlreadyExists,
            ErrorKind::PermissionDenied => Self::PermissionDenied,
            ErrorKind::BrokenPipe => Self::BrokenPipe,
            ErrorKind::Interrupted => Self::Interrupted,
            ErrorKind::OutOfMemory => Self::OutOfMemory,
            ErrorKind::Unsupported => Self::Unsupported,
            ErrorKind::IsADirectory => Self::IsADirectory,
            ErrorKind::NotADirectory => Self::NotADirectory,
            // The message is the vfs's own, which names a virtual path. A raw
            // `io::Error` from the host would be a leak; the vfs does not
            // produce those for the operations this crate calls.
            _ => Self::Other(error.to_string()),
        }
    }
}

impl std::fmt::Display for PlatformError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            Self::AlreadyExists => "entity already exists",
            Self::BrokenPipe => "pipe is closed",
            Self::Interrupted => "operation was interrupted",
            Self::IsADirectory => "expected a non-directory path, but found a directory",
            Self::NotFound => "entity was not found",
            Self::NotADirectory => "expected a directory, but found a non-directory path",
            Self::Other(message) => message,
            Self::OutOfMemory => "operation could not allocate enough memory",
            Self::PermissionDenied => "permission denied",
            Self::Unsupported => "operation is unsupported",
        };
        f.write_str(message)
    }
}

impl std::error::Error for PlatformError {}
