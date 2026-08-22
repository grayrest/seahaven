//! Confinement should be invisible.
//!
//! A bundled utility run against a mounted directory must behave exactly as it
//! does with no policy at all. Every difference is either a bug or a deliberate
//! virtualisation, and there are very few of the latter — so the whole property
//! is expressible as a differential: run the same script twice, once under a
//! restrictive mount and once under the identity policy, and compare the bytes.
//!
//! This is the gate the source lint cannot be. `check lint` proves no *source
//! line* in a fork reaches the host outside `forks/UNROUTED.txt`; it said the
//! tree was clean while `cp -r`, `mkdir -p`, `tail FILE` and `dd` were all
//! broken under a mount, because the calls it cannot see are exactly the ones
//! that were wrong. Landlock (D41) can prove completeness with the kernel, but
//! only on Linux and only for the in-process shell — it never runs on the
//! platform most of this was found on, and it cannot reach the twelve files of
//! Apple-gated code in the forks. This runs anywhere the shell builds, covers
//! the bundled utilities and the D24 child, and would have caught every
//! confinement bug fixed on this branch.
//!
//! Two rules make the comparison meaningful:
//!
//! - **Scripts name only relative paths.** The two trees live at different
//!   absolute paths, so a utility that echoes an absolute argument would differ
//!   for an uninteresting reason. After the `cd`, relative names are identical
//!   on both sides.
//! - **The mount is a subdirectory.** With `--mount /work:.` the host working
//!   directory and the mount root are the same directory, so a write that
//!   escapes the namespace lands in the same place as one that does not and the
//!   comparison cannot see it. Mounting a subdirectory puts them somewhere
//!   different, which is the whole point — it is how `ln -s` was caught writing
//!   outside the mount.
//!
//! A case that *should* differ says so, with the reason. There are four, and
//! all four are the namespace being visible on purpose rather than leaking --
//! three of them the same fact (a resolved path is spelled virtually) reached
//! through `pwd`, `realpath` and `ls -R`. A declared difference that stops
//! happening fails the test too, the way `check forks` treats a known failure
//! that starts passing: otherwise the list stops describing anything.

#![cfg(unix)]
#![cfg(feature = "experimental-bundled-coreutils")]
#![allow(
    clippy::disallowed_methods,
    reason = "test fixtures are built on the host, which is the one place that is the point"
)]
#![allow(
    clippy::expect_used,
    clippy::tests_outside_test_module,
    reason = "clippy's allow-expect-in-tests reaches `#[test]` bodies but not the \
              helpers beside them, and an integration test crate is not a `cfg(test)` \
              module. A fixture that cannot be built should abort the case rather \
              than be reported as a difference between the two shells."
)]

use std::path::Path;
use std::process::Command;

/// A file to seed into both trees before the script runs.
struct Seed {
    path: &'static str,
    contents: &'static str,
}

const fn seed(path: &'static str, contents: &'static str) -> Seed {
    Seed { path, contents }
}

/// One differential case.
struct Case {
    /// What the case is about, in the failure message.
    name: &'static str,
    /// Shell script, run after a `cd` into the tree. Relative paths only.
    script: &'static str,
    /// Files to create in both trees.
    seeds: &'static [Seed],
    /// `Some(reason)` when the two sides are *supposed* to disagree.
    differs: Option<&'static str>,
}

const fn case(name: &'static str, script: &'static str, seeds: &'static [Seed]) -> Case {
    Case {
        name,
        script,
        seeds,
        differs: None,
    }
}

const fn virtualised(
    name: &'static str,
    script: &'static str,
    seeds: &'static [Seed],
    reason: &'static str,
) -> Case {
    Case {
        name,
        script,
        seeds,
        differs: Some(reason),
    }
}

/// The standard tree: two files, a directory with a file in it, and a symlink.
///
/// The symlink is made by the script rather than seeded, because creating one
/// is itself a routed operation worth exercising.
const TREE: &[Seed] = &[
    seed("f.txt", "alpha\nbravo\ncharlie\n"),
    seed("g.txt", "delta\n"),
    seed("d/inner.txt", "echo\n"),
];

/// Cases, one per behaviour that a namespace could plausibly change.
///
/// Volatile output is normalised in the script rather than in the comparison:
/// `ls -l`'s timestamp columns are dropped, `find` is sorted because two
/// directories need not enumerate in the same order, and `dd`'s stderr carries
/// a transfer rate. Anything left is signal.
const CASES: &[Case] = &[
    // Reading, listing and walking.
    case("cat a file", "cat f.txt", TREE),
    case("ls the working directory", "ls", TREE),
    case("ls a named directory", "ls d", TREE),
    case("ls a named file", "ls f.txt", TREE),
    case(
        "ls -l, mode column only",
        // `cut` rather than `awk`: under `--closed-world` only bundled
        // commands run, and a script that reaches for a host tool is testing
        // the closed world rather than the namespace.
        "ls -l f.txt | cut -c1-10",
        TREE,
    ),
    case("head and tail", "head -n1 f.txt; tail -n1 f.txt", TREE),
    case("wc", "wc -l f.txt", TREE),
    case("du of a directory", "du -s d", TREE),
    case("find, sorted", "find . | sort", TREE),
    case("grep -r", "grep -r bravo . | sort", TREE),
    case(
        "sed in place",
        "sed -i 's/bravo/BRAVO/' f.txt && cat f.txt",
        TREE,
    ),
    case(
        "sed -i with a backup suffix",
        // Exercises the replacement file: created next to the target, renamed
        // over it, with the original moved aside first. It was built on the
        // host through `NamedTempFile`, so the rename moved a file that had
        // never been in the mount.
        "sed -i'.bak' -e 's/bravo/BRAVO/' f.txt && cat f.txt && cat f.txt.bak",
        TREE,
    ),
    case(
        "cksum reports a directory as one",
        // `Path::is_dir` on the host answered "no" for every path in a mount,
        // so a directory argument was read as a file instead.
        "cksum d; echo rc=$?",
        TREE,
    ),
    case(
        "sed's w command writes inside the namespace",
        // The `w` output file is named in the *script*, so an unrouted open
        // wrote it wherever the host resolved that name -- exit 0, file
        // outside the mount. Reading it back is the assertion.
        "sed -n 's/bravo/BRAVO/w out.txt' f.txt && cat out.txt",
        TREE,
    ),
    // Creating and removing.
    case("cp a file", "cp f.txt c.txt && cat c.txt", TREE),
    case(
        "cp -p preserves timestamps",
        // `filetime`'s path setters resolve on the host, so this failed with a
        // bare "No such file or directory". `touch -t` first, so the stamp
        // being compared is a fixed one rather than now.
        "touch -t 202001020304 f.txt && cp -p f.txt c.txt && ls -l c.txt | cut -c1-10",
        TREE,
    ),
    case(
        "cp -Pp preserves a symlink's own timestamps",
        // The no-follow branch, which used `set_symlink_file_times`.
        "ln -s f.txt l && cp -Pp l l2 && readlink l2",
        TREE,
    ),
    case("cp -r a directory", "cp -r d d2 && cat d2/inner.txt", TREE),
    case("mv a file", "mv f.txt m.txt && cat m.txt", TREE),
    case("mv into a directory", "mv f.txt d/ && cat d/f.txt", TREE),
    case("mv refuses a file onto itself", "mv f.txt f.txt", TREE),
    case("mkdir -p", "mkdir -p a/b/c && ls a/b", TREE),
    case(
        "mkdir -m keeps the mode",
        "mkdir -m 700 s && ls -ld s | cut -c1-10",
        TREE,
    ),
    case("rm -r", "rm -r d && ls", TREE),
    case("rmdir", "mkdir empty && rmdir empty && ls", TREE),
    case("touch creates", "touch new.txt && ls new.txt", TREE),
    case(
        "truncate sets the length",
        "truncate -s 3 f.txt && wc -c < f.txt",
        TREE,
    ),
    // Links, the class that escaped.
    case("ln -s and read back", "ln -s f.txt link && cat link", TREE),
    case(
        "ln -sf replaces a link in place",
        "ln -s f.txt link && ln -sf g.txt link && cat link",
        TREE,
    ),
    case(
        "ln -s into a directory operand",
        "ln -s ../f.txt d/ && cat d/f.txt",
        TREE,
    ),
    case("a hard link", "ln f.txt hard && cat hard", TREE),
    case("readlink", "ln -s f.txt link && readlink link", TREE),
    // Opens that carry flags or modes.
    case(
        "dd copies",
        "dd if=f.txt of=dd.txt 2>/dev/null && cat dd.txt",
        TREE,
    ),
    case("split", "split -l1 f.txt p && ls p*", TREE),
    case("sort -o", "sort f.txt -o s.txt && cat s.txt", TREE),
    case("shred", "shred -n1 -u g.txt && ls", TREE),
    // Deliberate: the namespace is visible here on purpose.
    virtualised(
        "realpath reports the virtual path",
        "realpath f.txt",
        TREE,
        "D6: a confined shell names paths in the namespace, and the host's \
         spelling is exactly what must not be reachable",
    ),
    virtualised(
        "pwd reports the virtual path",
        "pwd",
        TREE,
        "as realpath: the working directory is a session fact (D15), and \
         reporting the host's is the leak D6 forbids",
    ),
    virtualised(
        "ls -R heads subdirectories with the virtual path",
        "ls -R",
        TREE,
        "the recursive header is a resolved directory name, so it is the \
         namespace's spelling on one side and the host's on the other -- the \
         same fact `pwd` reports, reached through a different utility",
    ),
    virtualised(
        "ln -s refuses a target that climbs out",
        "ln -s ../f.txt up",
        TREE,
        "the stored target must land inside the mount, which brush-vfs checks \
         and the raw syscall cannot -- unconfined there is nothing to check \
         against, so the link is created",
    ),
];

/// What one side of the differential produced.
struct Run {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

impl Run {
    fn rendered(&self) -> String {
        std::format!(
            "exit={:?}\n--- stdout ---\n{}--- stderr ---\n{}",
            self.code,
            self.stdout,
            self.stderr
        )
    }
}

/// Creates a case's tree under `root`, making parent directories as needed.
fn plant(root: &Path, seeds: &[Seed]) {
    for s in seeds {
        let path = root.join(s.path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("seed parent");
        }
        std::fs::write(&path, s.contents).expect("seed file");
    }
}

/// Runs `script` in the built shell and captures everything it produced.
fn run(args: &[&str], cwd: &Path) -> Run {
    let shell = assert_cmd::cargo::cargo_bin!("brush");
    let output = Command::new(shell)
        .args(["--norc", "--noprofile"])
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("failed to spawn brush");
    Run {
        code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

/// Runs one case both ways and returns `(confined, identity)`.
fn differential(c: &Case) -> (Run, Run) {
    // The confined side: the tree is a *subdirectory* of the shell's host
    // working directory, so an escaped write lands somewhere the comparison can
    // see. See the module docs.
    let confined_root = tempfile::tempdir().expect("tempdir");
    let mount = confined_root.path().join("sub");
    std::fs::create_dir(&mount).expect("mount dir");
    plant(&mount, c.seeds);
    let script = std::format!("cd /work && {}", c.script);
    let confined = run(
        &["--mount", "/work:sub", "--closed-world", "-c", &script],
        confined_root.path(),
    );

    // The identity side: no policy at all, same tree, same script.
    let plain_root = tempfile::tempdir().expect("tempdir");
    let tree = plain_root.path().join("sub");
    std::fs::create_dir(&tree).expect("tree dir");
    plant(&tree, c.seeds);
    let identity = run(&["-c", c.script], &tree);

    (confined, identity)
}

#[test]
fn confinement_is_invisible() {
    let mut failures = Vec::new();

    for c in CASES {
        let (confined, identity) = differential(c);
        let same = confined.code == identity.code
            && confined.stdout == identity.stdout
            && confined.stderr == identity.stderr;

        match (same, c.differs) {
            // Agreed, and was expected to.
            (true, None) => {}
            // Disagreed, and was expected to.
            (false, Some(_)) => {}
            (false, None) => failures.push(std::format!(
                "{}: confinement changed the answer.\n  script: {}\n\
                 == confined ==\n{}\n== identity ==\n{}",
                c.name,
                c.script,
                confined.rendered(),
                identity.rendered(),
            )),
            // The mirror rule, as `check forks` and the fork lint both apply it:
            // a declared difference that stopped happening is a failure, or the
            // list quietly stops describing anything.
            (true, Some(reason)) => failures.push(std::format!(
                "{}: declared as a deliberate difference, but the two sides now \
                 agree. Remove the declaration.\n  reason on record: {}\n  \
                 script: {}",
                c.name,
                reason,
                c.script,
            )),
        }
    }

    assert!(
        failures.is_empty(),
        "{} of {} differential case(s) failed:\n\n{}",
        failures.len(),
        CASES.len(),
        failures.join("\n\n")
    );
}

/// The comparison is only worth anything if a real escape would fail it.
///
/// Without this, a mistake that made both sides equally wrong -- a `--mount`
/// that silently did nothing, say -- would leave every case above green while
/// testing nothing at all. So: assert that the confined side genuinely cannot
/// reach outside its mount, on the one operation that is unambiguous.
#[test]
fn the_differential_would_notice_an_escape() {
    let root = tempfile::tempdir().expect("tempdir");
    let mount = root.path().join("sub");
    std::fs::create_dir(&mount).expect("mount dir");
    std::fs::write(mount.join("inside.txt"), "in\n").expect("seed");
    // A real file, in the shell's host working directory, outside the mount.
    std::fs::write(root.path().join("outside.txt"), "out\n").expect("seed");

    let confined = run(
        &[
            "--mount",
            "/work:sub",
            "--closed-world",
            "-c",
            "cd /work && cat ../outside.txt",
        ],
        root.path(),
    );
    assert_ne!(
        confined.code,
        Some(0),
        "a confined shell read a file outside its mount; every case in this \
         file is vacuous. stdout: {:?}",
        confined.stdout
    );

    let identity = run(&["-c", "cat ../outside.txt"], &mount);
    assert_eq!(
        identity.code,
        Some(0),
        "the identity side could not read the control file, so the case above \
         proves nothing. stderr: {:?}",
        identity.stderr
    );
}
