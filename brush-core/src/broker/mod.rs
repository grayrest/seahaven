//! The session broker (D24): how a confined namespace reaches a child process.
//!
//! A bundled utility does not run in the shell's process. It runs in one the
//! shell re-execs (D5), and nothing crosses that boundary except argv, the
//! environment and file descriptors — so before this module existed, the child
//! was handed the *identity* namespace and read the host freely under every
//! confinement flag the shell has.
//!
//! # What crosses, and what does not
//!
//! Only the namespace and the working directory. Redirections already cross on
//! their own — `inject_fds` hands the child its descriptors, so `cat x > y` and
//! `exec 3>z` land inside the mount without help from here — and a bundled
//! child runs one utility rather than a shell, so none of the `Shell` serde
//! blob is needed. Per mount that leaves a mount point, an [`Access`], and a
//! directory capability.
//!
//! [`Access`]: brush_vfs::Access
//!
//! # The credential is the pid
//!
//! macOS reports pid *xor* uid on a unix socket — `LOCAL_PEERPID` carries no
//! uid, `LOCAL_PEERCRED` carries no pid — so a design wanting one answer on
//! every platform has to pick one. This picks the pid: `LOCAL_PEERPID` on
//! macOS, `SO_PEERCRED` on Linux, `GetNamedPipeClientProcessId` on Windows.
//!
//! **Nothing here checks uid.** Cross-user isolation is the rendezvous
//! directory's `0700` mode, and it is the only layer, not a second one.
//!
//! # Ordering
//!
//! Peer credentials bind a *connection*, not a session, so the parent cannot
//! validate anything until it knows the child's pid. The order is: create the
//! listener, spawn, *then* serve. The child blocks in `connect` until the
//! parent is ready, so no connection is ever checked against an unknown pid.
//! Acceptance is one-shot — a second connection passes the credential check
//! identically, and passing a descriptor cannot be undone.
//!
//! # The rendezvous path travels in the environment
//!
//! Not in argv. D32 rejected a nonce in argv because `/proc/PID/cmdline` is
//! world-readable; `/proc/PID/environ` is not. It also leaves `argv[2]` as the
//! dispatched utility name, which D11's exec predicate reads.

#[cfg_attr(unix, path = "unix.rs")]
#[cfg_attr(windows, path = "windows.rs")]
#[cfg(any(unix, windows))]
mod platform;

#[cfg(any(unix, windows))]
pub use platform::Rendezvous;

use brush_vfs::Access;

/// Names the rendezvous for the child. Set by the parent on the child's
/// environment; absent for any process that is not a bundled dispatch.
pub const RENDEZVOUS_ENV: &str = "BRUSH_SESSION_RENDEZVOUS";

/// Guards against a payload from a different build. The handshake is between
/// two runs of the *same* executable, so a mismatch is a bug rather than a
/// compatibility problem, and failing loudly beats misreading handle counts.
const WIRE_VERSION: u32 = 1;

/// The namespace and working directory, flattened for the wire.
///
/// Handles are owned rather than borrowed: the parent duplicates each one
/// before serving so the payload can move onto a blocking task without
/// borrowing the shell's mount table.
pub struct SessionPayload {
    /// The session's working directory, as a virtual path.
    pub cwd: String,
    /// One entry per mount: where it appears, what it permits, its capability.
    pub mounts: Vec<(String, Access, brush_vfs::MountHandle)>,
}

impl SessionPayload {
    /// Duplicates a session's mount handles for handing to a child.
    ///
    /// # Errors
    ///
    /// Returns [`std::io::Error`] if a handle cannot be duplicated.
    pub fn capture(session: &brush_vfs::Session) -> std::io::Result<Self> {
        let mut mounts = Vec::new();
        for loan in session.vfs().mounts().lend_to_child() {
            mounts.push((
                loan.mount_point().as_str().to_owned(),
                loan.access(),
                platform::duplicate(&loan)?,
            ));
        }
        Ok(Self {
            cwd: session.cwd().as_str().to_owned(),
            mounts,
        })
    }
}

/// Encodes everything except the handles themselves.
///
/// Hand-rolled rather than serde: the format is four field kinds wide, it has
/// exactly one producer and one consumer in the same binary, and a decoder that
/// is visible in full is worth more here than one that is derived.
fn encode(cwd: &str, mounts: &[(String, Access, brush_vfs::MountHandle)]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&WIRE_VERSION.to_le_bytes());
    put_str(&mut out, cwd);
    let count = u32::try_from(mounts.len()).unwrap_or(u32::MAX);
    out.extend_from_slice(&count.to_le_bytes());
    for (at, access, _) in mounts {
        put_str(&mut out, at);
        out.push(match access {
            Access::ReadOnly => 0,
            Access::ReadWrite => 1,
        });
    }
    out
}

fn put_str(out: &mut Vec<u8>, s: &str) {
    let len = u32::try_from(s.len()).unwrap_or(u32::MAX);
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(s.as_bytes());
}

/// The decoded payload minus handles, which the platform supplies separately.
struct Decoded {
    cwd: String,
    mounts: Vec<(String, Access)>,
}

fn decode(bytes: &[u8]) -> std::io::Result<Decoded> {
    let mut r = Reader { bytes, at: 0 };
    if r.u32()? != WIRE_VERSION {
        return Err(malformed("session payload has the wrong wire version"));
    }
    let cwd = r.string()?;
    let count = r.u32()?;
    let mut mounts = Vec::new();
    for _ in 0..count {
        let at = r.string()?;
        let access = match r.byte()? {
            0 => Access::ReadOnly,
            1 => Access::ReadWrite,
            _ => return Err(malformed("session payload has an unknown access mode")),
        };
        mounts.push((at, access));
    }
    Ok(Decoded { cwd, mounts })
}

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Reader<'_> {
    fn take(&mut self, n: usize) -> std::io::Result<&[u8]> {
        let end = self
            .at
            .checked_add(n)
            .filter(|e| *e <= self.bytes.len())
            .ok_or_else(|| malformed("session payload ended early"))?;
        let out = &self.bytes[self.at..end];
        self.at = end;
        Ok(out)
    }

    fn byte(&mut self) -> std::io::Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> std::io::Result<u32> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn string(&mut self) -> std::io::Result<String> {
        let len = self.u32()? as usize;
        let b = self.take(len)?;
        String::from_utf8(b.to_vec())
            .map_err(|_| malformed("session payload has a non-UTF-8 field"))
    }
}

fn malformed(message: &str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, message)
}

/// Receives the session this process was handed, if it was handed one.
///
/// `None` when [`RENDEZVOUS_ENV`] is unset, which is every process that is not
/// a bundled dispatch from a confined parent. `Some(Err(..))` when a handshake
/// was expected and failed — which the caller must treat as fatal rather than
/// falling back to an unconfined namespace, since that fallback is the hole
/// this module exists to close.
#[cfg(any(unix, windows))]
pub fn receive() -> Option<std::io::Result<brush_vfs::Session>> {
    let path = std::env::var_os(RENDEZVOUS_ENV)?;
    // The variable is consumed, not merely read: it names a one-shot
    // rendezvous, and leaving it in the environment would hand every
    // grandchild a path that no longer works and a reason to think it should.
    // SAFETY: called from `maybe_dispatch`, before the process has started any
    // thread of its own, so there is no concurrent reader of the environment.
    unsafe { std::env::remove_var(RENDEZVOUS_ENV) };
    Some(platform::receive(std::path::Path::new(&path)))
}

/// Never a handshake on a platform with neither sockets nor pipes.
#[cfg(not(any(unix, windows)))]
pub fn receive() -> Option<std::io::Result<brush_vfs::Session>> {
    None
}

/// Serves one child its session, if the platform has a broker at all.
///
/// Split from [`Rendezvous::serve`] so the capture and the transport are one
/// call at the spawn site, and so the no-broker platforms have somewhere to be
/// a no-op.
///
/// # Errors
///
/// Returns [`std::io::Error`] if the handles cannot be duplicated, if the
/// child never connects, or if the connecting process is not `pid`.
#[cfg(any(unix, windows))]
pub fn serve(
    rendezvous: Rendezvous,
    pid: Option<i32>,
    session: &brush_vfs::Session,
) -> std::io::Result<()> {
    let pid = pid
        .and_then(|p| u32::try_from(p).ok())
        .ok_or_else(|| std::io::Error::other("the child reported no pid to authenticate"))?;
    let payload = SessionPayload::capture(session)?;
    rendezvous.serve(pid, payload)
}

/// Builds a session from what the handshake delivered.
#[cfg(any(unix, windows))]
fn assemble(
    decoded: Decoded,
    handles: Vec<brush_vfs::MountHandle>,
) -> std::io::Result<brush_vfs::Session> {
    if decoded.mounts.len() != handles.len() {
        return Err(malformed(
            "session payload describes a different number of mounts than it delivered",
        ));
    }
    let entries: Vec<_> = decoded
        .mounts
        .into_iter()
        .zip(handles)
        .map(|((at, access), handle)| (at, access, handle))
        .collect();

    let table = brush_vfs::MountTable::from_child_handles(entries)
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    let mut session = brush_vfs::Session::new(std::sync::Arc::new(brush_vfs::Vfs::new(table)));
    session.set_cwd(&decoded.cwd)?;
    Ok(session)
}

/// A platform with neither unix sockets nor named pipes cannot hand a child a
/// namespace, so there is nothing to create and nothing to serve.
#[cfg(not(any(unix, windows)))]
pub struct Rendezvous;

#[cfg(not(any(unix, windows)))]
impl Rendezvous {
    /// Always fails: there is no transport here.
    ///
    /// # Errors
    ///
    /// Always returns [`std::io::Error`].
    pub fn create() -> std::io::Result<Self> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "session broker: this platform has no way to hand a child a namespace",
        ))
    }

    /// Unreachable: [`Rendezvous::create`] never succeeds here.
    #[must_use]
    pub fn path(&self) -> &std::path::Path {
        std::path::Path::new("")
    }
}

/// Unreachable: no rendezvous can exist on this platform.
///
/// # Errors
///
/// Always returns [`std::io::Error`].
#[cfg(not(any(unix, windows)))]
pub fn serve(
    _rendezvous: Rendezvous,
    _pid: Option<i32>,
    _session: &brush_vfs::Session,
) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "session broker: this platform has no way to hand a child a namespace",
    ))
}
