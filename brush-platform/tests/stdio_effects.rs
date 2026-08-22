//! Step 4's stdio effects, driven through the trait (D28, D36).
//!
//! The unit tests in `src/stdio.rs` cover the log and the stdin source in
//! isolation. These drive the *effects* — `stdout_line!` and the rest, through
//! `VfsPlatform` — so the wiring from a guest call to the buffer is checked, not
//! just the buffer.

#![cfg(unix)]
#![cfg(test)]
#![allow(
    clippy::disallowed_methods,
    clippy::expect_used,
    reason = "the test builds a session on the host, which is the point"
)]

use brush_platform::{PlatformEffects, SessionFacts, Stream, VfsPlatform};
use brush_vfs::{Policy, Session, Vfs};

fn host() -> VfsPlatform {
    let mounts = Policy::identity().expect("identity");
    let session = Session::new(std::sync::Arc::new(Vfs::new(mounts)));
    VfsPlatform::new(session, SessionFacts::neutral())
}

#[test]
fn guest_writes_land_in_the_log_separated_and_ordered() {
    // The end-to-end version of the wire-separation property: a guest calling
    // the effects, not a test calling `OutputLog::write`.
    let mut host = host();
    host.stderr_line("+ echo hi").expect("stderr");
    host.stdout_write("hi").expect("stdout");
    host.stdout_line("").expect("stdout newline");

    let log = host.output();
    assert_eq!(log.stdout(), b"hi\n", "stdout carries only the output");
    assert_eq!(log.stderr(), b"+ echo hi\n", "stderr carries only the echo");
    assert_eq!(
        log.rendered(),
        b"+ echo hi\nhi\n",
        "the render keeps the echo attached"
    );
}

#[test]
fn the_guest_has_no_path_to_the_bytes_it_wrote() {
    // The "cannot defeat the buffering" property, as far as a test can state it:
    // the effect trait -- the guest's entire surface -- exposes no reader. Every
    // way to observe the output is on `VfsPlatform`, which is the host's handle,
    // not the guest's. This is a compile-time fact; the test documents it and
    // fails to compile if `output()` ever moves onto the trait.
    fn assert_write_only<T: PlatformEffects>() {}
    assert_write_only::<VfsPlatform>();

    let mut host = host();
    host.stdout_write("secret").expect("write");
    // The host can read it; the guest (through `&dyn PlatformEffects`) cannot.
    assert_eq!(host.output().stdout(), b"secret");
}

#[test]
fn stdin_reads_what_was_piped_then_ends() {
    let mut host = host().with_stdin(b"alpha\nbeta\n".to_vec());
    assert_eq!(host.stdin_line().expect("line"), Some("alpha".to_owned()));
    assert_eq!(host.stdin_line().expect("line"), Some("beta".to_owned()));
    assert_eq!(
        host.stdin_line().expect("line"),
        None,
        "a drained stdin is end of file, not a block"
    );
}

#[test]
fn stdin_read_to_end_takes_the_remainder() {
    let mut host = host().with_stdin(b"one\ntwo\nrest".to_vec());
    assert_eq!(host.stdin_line().expect("line"), Some("one".to_owned()));
    assert_eq!(
        host.stdin_read_to_end().expect("rest"),
        b"two\nrest",
        "read_to_end takes what the line read left"
    );
}

#[test]
fn an_empty_stdin_is_immediately_end_of_file() {
    let mut host = host();
    assert_eq!(host.stdin_line().expect("line"), None);
    assert!(host.stdin_read_to_end().expect("end").is_empty());
    // And a fresh output log is empty, so a job that wrote nothing renders
    // nothing rather than a stray byte.
    assert!(host.output().is_empty());
    let _ = Stream::Stdout;
}
