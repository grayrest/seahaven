//! The Unix half of the broker (D24): a unix socket carrying `SCM_RIGHTS`.

use std::os::fd::{AsRawFd as _, BorrowedFd, FromRawFd as _, OwnedFd, RawFd};
use std::os::unix::fs::DirBuilderExt as _;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};

use nix::sys::socket::{ControlMessage, ControlMessageOwned, MsgFlags, recvmsg, sendmsg};

use super::{Decoded, SessionPayload, assemble, decode, encode};

/// How long the parent waits for its child to connect.
///
/// A child that never arrives has died before reaching `main`, so this bounds a
/// hang rather than a slow path. It is generous because the alternative failure
/// -- giving up on a child that was merely descheduled -- confines a utility
/// that should have run.
const ACCEPT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// How long a child waits for the parent to serve it.
const RECEIVE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// The parent's end: a listening socket in a directory only this user can read.
pub struct Rendezvous {
    dir: PathBuf,
    path: PathBuf,
    listener: UnixListener,
}

impl Rendezvous {
    /// Creates the rendezvous directory and binds a listener inside it.
    ///
    /// # Errors
    ///
    /// Returns [`std::io::Error`] if the directory or socket cannot be created.
    pub fn create() -> std::io::Result<Self> {
        // A socket path is a host path and `bind` creates a file outside every
        // mount, which is exactly what the ban says (`clippy.toml`). The
        // exemption is this module and this milestone: the rendezvous is the
        // parent's own scratch, created before the child exists and unlinked
        // when it has been served, and no sandboxed code can name it.
        #[expect(
            clippy::disallowed_methods,
            reason = "D24's rendezvous is the parent's own host scratch; see the module docs"
        )]
        let dir = {
            let base = std::env::temp_dir().join(format!("brush-broker-{}", std::process::id()));
            // 0700 is the *only* thing keeping another user off this socket:
            // the handshake checks a pid and, on macOS, cannot also learn a
            // uid. Created with the mode rather than chmod'd afterwards, since
            // the window between the two is the thing being defended against.
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(&base)?;
            base
        };

        let path = dir.join(format!("s{}", next_id()));
        #[expect(
            clippy::disallowed_methods,
            reason = "D24's rendezvous is the parent's own host scratch; see the module docs"
        )]
        let listener = UnixListener::bind(&path)?;
        listener.set_nonblocking(true)?;
        Ok(Self {
            dir,
            path,
            listener,
        })
    }

    /// The path to hand the child in [`RENDEZVOUS_ENV`].
    ///
    /// [`RENDEZVOUS_ENV`]: super::RENDEZVOUS_ENV
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Accepts exactly one connection from `pid` and sends it the session.
    ///
    /// Consumes the rendezvous: acceptance is one-shot because a second
    /// connection presents identical credentials and a descriptor already
    /// passed cannot be taken back.
    ///
    /// # Errors
    ///
    /// Returns [`std::io::Error`] if no one connects before [`ACCEPT_TIMEOUT`],
    /// if the connecting process is not `pid`, or if the send fails.
    pub fn serve(self, pid: u32, payload: SessionPayload) -> std::io::Result<()> {
        let stream = self.accept_before(deadline(ACCEPT_TIMEOUT))?;
        let peer = peer_pid(&stream)?;
        if peer != pid {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!("session broker: expected a connection from pid {pid}, got {peer}"),
            ));
        }

        let bytes = encode(&payload.cwd, &payload.mounts);
        let fds: Vec<RawFd> = payload
            .mounts
            .iter()
            .map(|(_, _, h)| h.as_raw_fd())
            .collect();
        let cmsg = [ControlMessage::ScmRights(&fds)];
        let iov = [std::io::IoSlice::new(&bytes)];
        sendmsg::<()>(stream.as_raw_fd(), &iov, &cmsg, MsgFlags::empty(), None)
            .map_err(std::io::Error::from)?;

        // The child holds its own descriptors now, so the parent's duplicates
        // are dead weight. Dropped explicitly rather than at end of scope
        // because the ordering is load-bearing: `SCM_RIGHTS` must have copied
        // them first.
        drop(fds);
        drop(payload);
        Ok(())
    }

    fn accept_before(&self, deadline: std::time::Instant) -> std::io::Result<UnixStream> {
        loop {
            match self.listener.accept() {
                Ok((stream, _)) => return Ok(stream),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    if std::time::Instant::now() >= deadline {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::TimedOut,
                            "session broker: the child never connected",
                        ));
                    }
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                Err(e) => return Err(e),
            }
        }
    }
}

impl Drop for Rendezvous {
    fn drop(&mut self) {
        // Best effort: the socket and its directory are this process's own
        // scratch, and leaving them behind on a failure path is untidy rather
        // than unsafe.
        #[expect(
            clippy::disallowed_methods,
            reason = "D24's rendezvous is the parent's own host scratch; see the module docs"
        )]
        {
            let _ = std::fs::remove_file(&self.path);
            let _ = std::fs::remove_dir(&self.dir);
        }
    }
}

/// Duplicates a lent mount handle so the payload can outlive the borrow.
pub fn duplicate(loan: &brush_vfs::MountLoan<'_>) -> std::io::Result<OwnedFd> {
    // SAFETY: the loan borrows a `Dir` owned by the caller's mount table, which
    // outlives this call, and `try_clone_to_owned` duplicates rather than
    // taking ownership of the borrowed descriptor.
    let borrowed = unsafe { BorrowedFd::borrow_raw(loan.as_raw_fd()) };
    borrowed.try_clone_to_owned()
}

/// The child's end: connect, read the payload and its descriptors, rebuild.
pub fn receive(path: &Path) -> std::io::Result<brush_vfs::Session> {
    let stream = connect_before(path, deadline(RECEIVE_TIMEOUT))?;

    // Heap rather than stack: the payload is small in practice but the buffer
    // has to be large enough for a namespace with many mounts, and a 64 KiB
    // stack array in a function called at process start is a poor trade.
    let mut buf = vec![0u8; 64 * 1024];
    let mut cmsg = nix::cmsg_space!([RawFd; 64]);
    let mut iov = [std::io::IoSliceMut::new(&mut buf)];
    let msg = recvmsg::<()>(
        stream.as_raw_fd(),
        &mut iov,
        Some(&mut cmsg),
        MsgFlags::empty(),
    )
    .map_err(std::io::Error::from)?;

    let mut handles = Vec::new();
    for c in msg.cmsgs().map_err(std::io::Error::from)? {
        if let ControlMessageOwned::ScmRights(fds) = c {
            for fd in fds {
                // SAFETY: `SCM_RIGHTS` installs these descriptors in this
                // process's table and hands ownership to us; nothing else holds
                // them.
                handles.push(unsafe { OwnedFd::from_raw_fd(fd) });
            }
        }
    }

    let read = msg.bytes;
    let decoded: Decoded = decode(&buf[..read])?;
    assemble(decoded, handles)
}

fn connect_before(path: &Path, deadline: std::time::Instant) -> std::io::Result<UnixStream> {
    loop {
        // The other half of the ban exemption above: the child is naming the
        // rendezvous its parent created for it, before it has a namespace at
        // all.
        #[expect(
            clippy::disallowed_methods,
            reason = "D24's rendezvous is the parent's own host scratch; see the module docs"
        )]
        let attempt = UnixStream::connect(path);
        match attempt {
            Ok(stream) => return Ok(stream),
            // The parent binds before it spawns, so a rendezvous that is not
            // there is not there -- retrying only turns a clear refusal into a
            // ten-second hang that reads like one. Only a listener that exists
            // and is momentarily not accepting is worth waiting for.
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::ConnectionRefused
                        | std::io::ErrorKind::WouldBlock
                        | std::io::ErrorKind::Interrupted
                ) && std::time::Instant::now() < deadline =>
            {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            Err(e) => return Err(e),
        }
    }
}

fn deadline(after: std::time::Duration) -> std::time::Instant {
    std::time::Instant::now() + after
}

/// A counter so one parent can hold several rendezvous at once, as a pipeline
/// of bundled commands does.
fn next_id() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// The connecting process's pid, from the kernel.
///
/// macOS reports pid *xor* uid, and this takes the pid, so the same question is
/// asked on both platforms. Nothing here learns a uid; that is the rendezvous
/// directory's job.
fn peer_pid(stream: &UnixStream) -> std::io::Result<u32> {
    #[cfg(target_vendor = "apple")]
    {
        let pid = nix::sys::socket::getsockopt(stream, nix::sys::socket::sockopt::LocalPeerPid)
            .map_err(std::io::Error::from)?;
        u32::try_from(pid).map_err(|_| std::io::Error::other("peer pid is negative"))
    }
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        let cred = nix::sys::socket::getsockopt(stream, nix::sys::socket::sockopt::PeerCredentials)
            .map_err(std::io::Error::from)?;
        u32::try_from(cred.pid()).map_err(|_| std::io::Error::other("peer pid is negative"))
    }
    #[cfg(not(any(target_vendor = "apple", target_os = "linux", target_os = "android")))]
    {
        let _ = stream;
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "session broker: this platform reports no peer credentials",
        ))
    }
}
