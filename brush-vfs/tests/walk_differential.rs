//! The walk is required to agree with `walkdir`, entry for entry.
//!
//! Reimplementing a mature traversal is the risk the walker carries: `walkdir`'s
//! edge behaviours — error-as-item, ordering under `contents_first`, what
//! `skip_current_dir` does, loop reporting — are load-bearing for four
//! utilities, and a walker that is subtly different is worse than an honestly
//! unrouted one, because the utilities would be quietly wrong instead of
//! visibly unconfined.
//!
//! So both walk the same tree under the identity policy and their output is
//! compared. Identity is the only policy where that comparison is meaningful:
//! under a restrictive one `walkdir` sees a different filesystem, which is the
//! entire point of the milestone and is covered by the confinement tests
//! instead.
//!
//! Ordering is pinned with `sort_by_file_name` on both sides, since `readdir`
//! order is not a property either implementation controls.

#![expect(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::disallowed_methods,
    clippy::tests_outside_test_module,
    reason = "clippy's allow-unwrap-in-tests reaches `#[test]` bodies but not the \
              helpers beside them, and an integration test crate is not a \
              `cfg(test)` module, so every test here reads as outside one. A \
              failed fixture should abort the test rather than be handled, and \
              the fixture tree is built on the host, which is the point."
)]
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use brush_vfs::{Access, MountTable, Session, Vfs};

/// The ambient session is process-global, so these must not run concurrently.
static GUARD: Mutex<()> = Mutex::new(());

/// A tree with the shapes that separate a correct walk from a plausible one.
fn fixture() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().expect("temp dir");
    let root = tmp.path();

    std::fs::create_dir_all(root.join("a/b/c")).unwrap();
    std::fs::create_dir_all(root.join("empty")).unwrap();
    std::fs::write(root.join("top.txt"), b"top").unwrap();
    std::fs::write(root.join("a/one.txt"), b"one").unwrap();
    std::fs::write(root.join("a/b/two.txt"), b"two").unwrap();
    std::fs::write(root.join("a/b/c/three.txt"), b"three").unwrap();

    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        // A link to a file, a link to a directory, and one that dangles.
        symlink("one.txt", root.join("a/link-to-file")).unwrap();
        symlink("b", root.join("a/link-to-dir")).unwrap();
        symlink("nowhere", root.join("a/dangling")).unwrap();
        // A link that points back up but stays inside the tree: legal, and not
        // a loop, so it must not be reported as one.
        symlink("../empty", root.join("a/b/sideways")).unwrap();
        // A FIFO, which an open-to-stat implementation can block on forever.
        let fifo = std::ffi::CString::new(
            root.join("a/pipe").as_os_str().as_encoded_bytes(),
        )
        .unwrap();
        // SAFETY: a valid NUL-terminated path and a valid mode.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o644) }, 0);
    }
    // A deeper chain, so max_depth has something to actually cut.
    std::fs::create_dir_all(root.join("deep/1/2/3/4")).unwrap();
    std::fs::write(root.join("deep/1/2/3/4/bottom.txt"), b"bottom").unwrap();
    tmp
}

fn install(root: &Path) {
    let mounts = MountTable::builder()
        .mount("/work", root, Access::ReadWrite)
        .expect("mount")
        .build()
        .expect("build");
    brush_vfs::ambient::install(Session::new(Arc::new(Vfs::new(mounts))));
}

/// What a walk yielded, in a form the two implementations can be compared on.
///
/// Paths are made relative to each walk's own root, since one is host-rooted
/// and the other namespace-rooted — comparing absolute paths would only ever
/// prove they are different filesystems.
#[derive(Debug, PartialEq, Eq)]
enum Item {
    Entry {
        rel: PathBuf,
        depth: usize,
        is_dir: bool,
        is_symlink: bool,
    },
    Err {
        depth: usize,
        is_loop: bool,
    },
}

fn from_walkdir(root: &Path, w: walkdir::WalkDir) -> Vec<Item> {
    w.sort_by_file_name()
        .into_iter()
        .map(|r| match r {
            Ok(e) => Item::Entry {
                rel: e.path().strip_prefix(root).unwrap_or_else(|_| e.path()).to_path_buf(),
                depth: e.depth(),
                is_dir: e.file_type().is_dir(),
                is_symlink: e.path_is_symlink(),
            },
            Err(e) => Item::Err {
                depth: e.depth(),
                is_loop: e.io_error().is_none(),
            },
        })
        .collect()
}

fn from_vfs(root: &Path, w: brush_vfs::walk::Walk) -> Vec<Item> {
    w.sort_by_file_name()
        .into_iter()
        .map(|r| match r {
            Ok(e) => Item::Entry {
                rel: e.path().strip_prefix(root).unwrap_or_else(|_| e.path()).to_path_buf(),
                depth: e.depth(),
                is_dir: e.file_type().is_dir(),
                is_symlink: e.path_is_symlink(),
            },
            Err(e) => Item::Err {
                depth: e.depth(),
                is_loop: e.io_error().is_none(),
            },
        })
        .collect()
}

/// Runs both walks over the same tree with the same options and compares.
fn assert_agrees(
    label: &str,
    host_root: &Path,
    build_walkdir: impl Fn(walkdir::WalkDir) -> walkdir::WalkDir,
    build_vfs: impl Fn(brush_vfs::walk::Walk) -> brush_vfs::walk::Walk,
) {
    let reference = from_walkdir(host_root, build_walkdir(walkdir::WalkDir::new(host_root)));
    let ours = from_vfs(
        Path::new("/work"),
        build_vfs(brush_vfs::ambient::walk("/work")),
    );
    assert_eq!(ours, reference, "{label}: the walk disagrees with walkdir");
}

#[test]
fn the_walk_agrees_with_walkdir() {
    let _g = GUARD.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let tmp = fixture();
    install(tmp.path());

    assert_agrees("default", tmp.path(), |w| w, |w| w);
    assert_agrees(
        "min_depth(1)",
        tmp.path(),
        |w| w.min_depth(1),
        |w| w.min_depth(1),
    );
    assert_agrees(
        "max_depth(2)",
        tmp.path(),
        |w| w.max_depth(2),
        |w| w.max_depth(2),
    );
    assert_agrees(
        "min 1 max 2",
        tmp.path(),
        |w| w.min_depth(1).max_depth(2),
        |w| w.min_depth(1).max_depth(2),
    );
    assert_agrees(
        "contents_first",
        tmp.path(),
        |w| w.contents_first(true),
        |w| w.contents_first(true),
    );
    assert_agrees(
        "follow_links",
        tmp.path(),
        |w| w.follow_links(true),
        |w| w.follow_links(true),
    );
    assert_agrees(
        "max_depth(0)",
        tmp.path(),
        |w| w.max_depth(0),
        |w| w.max_depth(0),
    );
    assert_agrees(
        "contents_first + min_depth(1)",
        tmp.path(),
        |w| w.contents_first(true).min_depth(1),
        |w| w.contents_first(true).min_depth(1),
    );
    assert_agrees(
        "follow_links + contents_first",
        tmp.path(),
        |w| w.follow_links(true).contents_first(true),
        |w| w.follow_links(true).contents_first(true),
    );
    assert_agrees(
        "follow_links + max_depth(3)",
        tmp.path(),
        |w| w.follow_links(true).max_depth(3),
        |w| w.follow_links(true).max_depth(3),
    );
    assert_agrees(
        "same_file_system",
        tmp.path(),
        |w| w.same_file_system(true),
        |w| w.same_file_system(true),
    );

    brush_vfs::ambient::uninstall();
}

#[cfg(unix)]
#[test]
fn a_symlink_loop_terminates_and_is_reported_as_a_loop() {
    // Gate 4. The guard is that this test returns at all: a walker that spins
    // wedges the suite rather than reddening it, which is itself the signal.
    let _g = GUARD.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("a/b")).unwrap();
    std::os::unix::fs::symlink("../..", tmp.path().join("a/b/up")).unwrap();
    install(tmp.path());

    let items = from_vfs(
        Path::new("/work"),
        brush_vfs::ambient::walk("/work").follow_links(true),
    );
    let loops = items
        .iter()
        .filter(|i| matches!(i, Item::Err { is_loop: true, .. }))
        .count();
    assert_eq!(loops, 1, "expected exactly one loop report: {items:?}");

    // And the same shape walkdir reports, since `uucore::perms` matches on it.
    assert_eq!(
        items,
        from_walkdir(
            tmp.path(),
            walkdir::WalkDir::new(tmp.path()).follow_links(true)
        ),
        "the loop report disagrees with walkdir"
    );

    brush_vfs::ambient::uninstall();
}

#[test]
fn skip_current_dir_abandons_the_rest_of_that_directory() {
    let _g = GUARD.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let tmp = fixture();
    install(tmp.path());

    // Skip whatever directory `a/b` is found in, on both sides, and compare.
    let collect_ours = {
        let mut it = brush_vfs::ambient::walk("/work")
            .sort_by_file_name()
            .into_iter();
        let mut out = Vec::new();
        while let Some(Ok(e)) = it.next() {
            let rel = e.path().strip_prefix("/work").unwrap_or_else(|_| e.path()).to_path_buf();
            let hit = rel == Path::new("a/b");
            out.push(rel);
            if hit {
                it.skip_current_dir();
            }
        }
        out
    };
    let collect_reference = {
        let mut it = walkdir::WalkDir::new(tmp.path())
            .sort_by_file_name()
            .into_iter();
        let mut out = Vec::new();
        while let Some(Ok(e)) = it.next() {
            let rel = e.path().strip_prefix(tmp.path()).unwrap_or_else(|_| e.path()).to_path_buf();
            let hit = rel == Path::new("a/b");
            out.push(rel);
            if hit {
                it.skip_current_dir();
            }
        }
        out
    };

    assert_eq!(collect_ours, collect_reference, "skip_current_dir disagrees");
    brush_vfs::ambient::uninstall();
}

#[test]
fn a_walk_rooted_outside_the_mount_enumerates_nothing() {
    // Gate 3, and the reason the milestone exists. `walkdir` reads directories
    // itself, so `grep -r` and `find` over a host path listed the tree and only
    // then failed each read -- content contained, structure not. An exit code
    // cannot tell that apart from never having looked, so this asserts on what
    // the walk *yielded*.
    let _g = GUARD.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let outside = fixture();
    let jail = tempfile::tempdir().unwrap();
    install(jail.path());

    for root in [
        outside.path().to_string_lossy().into_owned(),
        "/etc".to_string(),
        "/work/../..".to_string(),
    ] {
        let items = from_vfs(Path::new("/work"), brush_vfs::ambient::walk(&root));
        let entries = items
            .iter()
            .filter(|i| matches!(i, Item::Entry { .. }))
            .count();
        assert_eq!(entries, 0, "walking {root} yielded {entries} entries: {items:?}");
        assert!(
            matches!(items.as_slice(), [Item::Err { .. }]),
            "walking {root} should report exactly one error and stop: {items:?}"
        );
    }

    // For contrast: the same walk inside the mount does yield entries, so the
    // assertion above is not passing because the walker yields nothing at all.
    let inside = from_vfs(Path::new("/work"), brush_vfs::ambient::walk("/work"));
    assert!(
        inside.iter().any(|i| matches!(i, Item::Entry { .. })),
        "the walker must still walk what it is allowed to"
    );

    brush_vfs::ambient::uninstall();
}

#[test]
fn a_walk_with_no_session_yields_one_error_and_stops() {
    let _g = GUARD.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    brush_vfs::ambient::uninstall();

    let items = from_vfs(Path::new("/work"), brush_vfs::ambient::walk("/work"));
    assert!(
        matches!(items.as_slice(), [Item::Err { .. }]),
        "fail closed: {items:?}"
    );
}
