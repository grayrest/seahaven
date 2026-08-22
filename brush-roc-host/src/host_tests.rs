//! Marshalling round-trips: Roc values built host-side, driven through the
//! `hosted_*` functions, checked coming back.
//!
//! These do not link a Roc binary — they cannot, that is the step-9 integration
//! that is still ahead. What they *can* do is exercise the marshalling itself:
//! build the owned Roc argument the compiler would pass, call the effect, and
//! read the owned Roc value it returns. That covers the parts a compile check
//! cannot — the tag and payload of every result, and the release discipline,
//! since the system allocator aborts the test on a double free and a wrong tag
//! reads the wrong union arm. What remains for the link step is the true ABI:
//! the calling convention across the language boundary, which only a linked
//! binary exercises.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use core::mem::ManuallyDrop;
use std::path::Path;
use std::sync::Arc;

use brush_platform::{SessionFacts, VfsPlatform};
use brush_vfs::{Access, MountTable, Session, Vfs};

use crate::marshal::native_from_str;
use crate::roc_host;
use crate::roc_platform_abi::{
    HostCmdExecExitCodeArgs, HostCmdExecExitCodeResultTag, HostDirListResultTag,
    HostFileDeleteResultTag, HostFileIsExecutableResultTag, HostFileReadBytesResultTag,
    HostFileReadUtf8ResultTag, HostFileSizeInBytesResultTag, HostFileTimeAccessedResultTag,
    HostPathTypeResultTag, HostRegexIsMatchResultTag, RocList, RocStr,
    UnixBytesOrUtf8OrWindowsU16s as Native,
};

/// Builds the fixture tree, mounts its `work/` at `/work` read-write, installs
/// the session, and returns the tempdir (kept alive for the test's duration).
fn setup() -> tempfile::TempDir {
    use std::os::unix::fs::PermissionsExt as _;
    let temp = tempfile::tempdir().expect("temp dir");
    let root = temp.path().canonicalize().expect("canonicalize");
    let work = root.join("work");
    std::fs::create_dir(&work).expect("mkdir work");
    // A name longer than the small-string cap (23 bytes) so at least one path
    // and one payload force a heap `RocStr` -- exercising the refcounted path,
    // not just the inline one.
    std::fs::write(
        work.join("a-file-with-a-long-name.txt"),
        b"hello from the fixture\n",
    )
    .expect("write data");
    std::fs::write(work.join("run.sh"), b"#!/bin/sh\n").expect("write run.sh");
    std::fs::set_permissions(work.join("run.sh"), std::fs::Permissions::from_mode(0o755))
        .expect("chmod");
    std::fs::create_dir(work.join("sub")).expect("mkdir sub");
    install(&work);
    temp
}

fn install(work: &Path) {
    let mounts = MountTable::builder()
        .mount("/work", work, Access::ReadWrite)
        .expect("mount")
        .build()
        .expect("build");
    let mut session = Session::new(Arc::new(Vfs::new(mounts)));
    session.set_cwd("/work").expect("cd /work");
    crate::install_session_for_test(VfsPlatform::new(session, SessionFacts::neutral()));
}

/// A `Utf8` path argument, as the compiler would pass one.
fn path(name: &str) -> Native {
    native_from_str(name, &roc_host())
}

const LONG: &str = "a-file-with-a-long-name.txt";

#[test]
fn file_read_utf8_round_trips_its_contents() {
    let _temp = setup();
    let result = crate::fs_effects::hosted_file_read_utf8(path(LONG));
    assert!(matches!(result.tag, HostFileReadUtf8ResultTag::Ok));
    let text = unsafe { ManuallyDrop::into_inner(result.payload.ok) };
    assert_eq!(text.as_str(), "hello from the fixture\n");
    unsafe { text.decref(&roc_host()) };
}

#[test]
fn file_read_bytes_round_trips_its_length() {
    let _temp = setup();
    let result = crate::fs_effects::hosted_file_read_bytes(path(LONG));
    assert!(matches!(result.tag, HostFileReadBytesResultTag::Ok));
    let bytes = unsafe { ManuallyDrop::into_inner(result.payload.ok) };
    assert_eq!(bytes.as_slice(), b"hello from the fixture\n");
    unsafe { bytes.decref(&roc_host()) };
}

#[test]
fn a_missing_file_is_the_not_found_error() {
    let _temp = setup();
    let result = crate::fs_effects::hosted_file_read_utf8(path("nope.txt"));
    assert!(matches!(result.tag, HostFileReadUtf8ResultTag::Err));
    let error = unsafe { ManuallyDrop::into_inner(result.payload.err) };
    // NotFound is a unit variant, so there is nothing to release; reading the
    // tag is enough. (Decref is still safe on a unit IOErr.)
    assert!(matches!(
        error.tag,
        crate::roc_platform_abi::IOErrTag::NotFound
    ));
}

#[test]
fn an_unnameable_path_is_also_not_found() {
    // D45: a path the grammar rejects is unnameable, so it is NotFound, not a
    // distinct error. A colon is rejected on every platform.
    let _temp = setup();
    let result = crate::fs_effects::hosted_file_read_utf8(path("a:b"));
    assert!(matches!(result.tag, HostFileReadUtf8ResultTag::Err));
    let error = unsafe { ManuallyDrop::into_inner(result.payload.err) };
    assert!(matches!(
        error.tag,
        crate::roc_platform_abi::IOErrTag::NotFound
    ));
}

#[test]
fn write_then_read_is_the_bytes_written() {
    let _temp = setup();
    // Write via the bytes effect (the one rocjust's `Path` reaches), then read
    // it back via the utf8 effect: two marshallers agreeing on one file.
    let bytes = crate::marshal::roc_bytes_from_slice(b"written through the abi", &roc_host());
    let write = crate::fs_effects::hosted_file_write_bytes(path("out.txt"), bytes);
    assert!(matches!(write.tag, HostFileDeleteResultTag::Ok));

    let read = crate::fs_effects::hosted_file_read_utf8(path("out.txt"));
    assert!(matches!(read.tag, HostFileReadUtf8ResultTag::Ok));
    let text = unsafe { ManuallyDrop::into_inner(read.payload.ok) };
    assert_eq!(text.as_str(), "written through the abi");
    unsafe { text.decref(&roc_host()) };
}

#[test]
fn path_type_reads_each_kind() {
    use crate::roc_platform_abi::DirOrFileOrOtherOrSymLink as Kind;
    let _temp = setup();
    let file = crate::fs_effects::hosted_path_type(path(LONG));
    assert!(matches!(file.tag, HostPathTypeResultTag::Ok));
    assert!(matches!(
        unsafe { ManuallyDrop::into_inner(file.payload.ok) },
        Kind::File
    ));

    let dir = crate::fs_effects::hosted_path_type(path("sub"));
    assert!(matches!(
        unsafe { ManuallyDrop::into_inner(dir.payload.ok) },
        Kind::Dir
    ));
}

#[test]
fn dir_list_returns_the_entries_as_paths() {
    let _temp = setup();
    let result = crate::fs_effects::hosted_dir_list(path("."));
    assert!(matches!(result.tag, HostDirListResultTag::Ok));
    let list = unsafe { ManuallyDrop::into_inner(result.payload.ok) };
    let mut names: Vec<String> = list
        .as_slice()
        .iter()
        .map(|entry| entry_to_string(entry))
        .collect();
    names.sort();
    assert_eq!(
        names,
        vec![
            format!("/work/{LONG}"),
            "/work/run.sh".to_owned(),
            "/work/sub".to_owned(),
        ]
    );
    // Release each element, then the spine -- the discipline `dir_list`'s caller
    // owes, mirrored here so the test frees what the effect built.
    for entry in list.as_slice() {
        release_native(entry);
    }
    unsafe { list.decref(&roc_host()) };
}

#[test]
fn file_size_is_truthful() {
    let _temp = setup();
    let result = crate::fs_effects::hosted_file_size_in_bytes(path(LONG));
    assert!(matches!(result.tag, HostFileSizeInBytesResultTag::Ok));
    let size = unsafe { ManuallyDrop::into_inner(result.payload.ok) };
    assert_eq!(size, "hello from the fixture\n".len() as u64);
}

#[test]
fn is_readable_is_grant_derived_and_missing_is_an_error() {
    let _temp = setup();
    let readable = crate::fs_effects::hosted_file_is_readable(path(LONG));
    assert!(matches!(readable.tag, HostFileIsExecutableResultTag::Ok));
    assert!(unsafe { ManuallyDrop::into_inner(readable.payload.ok) });

    let missing = crate::fs_effects::hosted_file_is_readable(path("nope.txt"));
    assert!(matches!(missing.tag, HostFileIsExecutableResultTag::Err));
}

#[test]
fn file_times_are_the_deferred_unsupported_error() {
    // D46: the three time effects report Unsupported rather than a host time.
    let _temp = setup();
    for result in [
        crate::fs_effects::hosted_file_time_accessed(path(LONG)),
        crate::fs_effects::hosted_file_time_modified(path(LONG)),
        crate::fs_effects::hosted_file_time_created(path(LONG)),
    ] {
        assert!(matches!(result.tag, HostFileTimeAccessedResultTag::Err));
        let error = unsafe { ManuallyDrop::into_inner(result.payload.err) };
        assert!(matches!(
            error.tag,
            crate::roc_platform_abi::IOErrTag::Unsupported
        ));
    }
}

#[test]
fn regex_is_match_round_trips() {
    let _temp = setup();
    let host = roc_host();
    let matched = crate::misc_effects::hosted_regex_is_match(
        RocStr::from_str("^h.*e$", &host),
        RocStr::from_str("here", &host),
    );
    assert!(matches!(matched.tag, HostRegexIsMatchResultTag::Ok));
    assert!(unsafe { ManuallyDrop::into_inner(matched.payload.ok) });

    // A bad pattern comes back as the engine's message, which is a heap RocStr.
    let bad = crate::misc_effects::hosted_regex_is_match(
        RocStr::from_str("(unclosed", &host),
        RocStr::from_str("x", &host),
    );
    assert!(matches!(bad.tag, HostRegexIsMatchResultTag::Err));
    let message = unsafe { ManuallyDrop::into_inner(bad.payload.err) };
    assert!(!message.as_str().is_empty());
    unsafe { message.decref(&host) };
}

#[test]
fn stdout_write_lands_in_the_output_log() {
    // The bytes go to the job's output log, not a descriptor (D28). The effect
    // reports Ok; the log is the host's to read, which the test does through the
    // session it installed.
    let _temp = setup();
    let result =
        crate::io_effects::hosted_stdout_line(RocStr::from_str("a line to the log", &roc_host()));
    assert!(matches!(
        result.tag,
        crate::roc_platform_abi::HostStdoutLineResultTag::Ok
    ));
    crate::with_session((), |s| {
        assert_eq!(s.output().stdout(), b"a line to the log\n");
    });
}

#[test]
fn cmd_without_an_executor_is_unsupported_and_releases_the_command() {
    // No executor is installed (the foundation's state), so every exec fails
    // uniformly. The point under test is that `take_command` releases the
    // program and the two lists without leaking or double-freeing.
    let _temp = setup();
    let host = roc_host();
    let args = HostCmdExecExitCodeArgs {
        program: native_from_str("echo", &host),
        args: single_arg_list("hi", &host),
        envs: RocList::<Native>::empty(),
        clear_envs: false,
    };
    let result = crate::cmd_effects::hosted_cmd_exec_exit_code(args);
    assert!(matches!(result.tag, HostCmdExecExitCodeResultTag::Err));
    let error = unsafe { ManuallyDrop::into_inner(result.payload.err) };
    assert!(matches!(
        error.tag,
        crate::roc_platform_abi::HostIOErrTag::Unsupported
    ));
}

// --- helpers ---------------------------------------------------------------

/// Reads a `Native` entry (borrowed) to a `String` without consuming it.
fn entry_to_string(entry: &Native) -> String {
    use crate::roc_platform_abi::UnixBytesOrUtf8OrWindowsU16sTag as Tag;
    match entry.tag {
        Tag::Utf8 => unsafe { entry.payload.utf8.as_str().to_owned() },
        Tag::UnixBytes => unsafe {
            String::from_utf8_lossy(entry.payload.unix_bytes.as_slice()).into_owned()
        },
        Tag::WindowsU16s => String::new(),
    }
}

/// Releases one `Native`'s inner allocation (its `Utf8`/`UnixBytes` payload).
fn release_native(entry: &Native) {
    use crate::roc_platform_abi::UnixBytesOrUtf8OrWindowsU16sTag as Tag;
    let host = roc_host();
    match entry.tag {
        Tag::Utf8 => unsafe { (*entry.payload.utf8).decref(&host) },
        Tag::UnixBytes => unsafe { (*entry.payload.unix_bytes).decref(&host) },
        Tag::WindowsU16s => {}
    }
}

/// A one-element `List(OsStr)`, for a command's args.
fn single_arg_list(arg: &str, host: &crate::roc_platform_abi::RocHost) -> RocList<Native> {
    let list = unsafe { RocList::<Native>::allocate(1, host) };
    unsafe { list.elements.add(0).write(native_from_str(arg, host)) };
    list
}
