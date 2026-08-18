//! Completeness test for the namespace, enforced by the kernel rather than by a lint.
//!
//! The ban in `clippy.toml` proves that no *source line* reaches the host
//! filesystem outside the exemptions. It cannot prove that the exemptions are
//! narrow enough, that a dependency does not reach past them, or that a path
//! this test does not exercise stays inside. Landlock can: it restricts the
//! process to the namespace's mount roots, so any access that did not route
//! through `brush-vfs` is refused by the kernel.
//!
//! The assertions are all *positive*. A negative case -- "reading /etc/passwd
//! fails" -- proves nothing, because the namespace already refuses it and the
//! kernel's refusal would be indistinguishable. What catches an unrouted access
//! is an operation that ought to succeed and does not, because somewhere below
//! it the code asked the host for a path outside the mount.
//!
//! This is a test, not shipped enforcement. Nothing here changes how the shell
//! behaves for a user; see D41.

#![allow(
    clippy::disallowed_methods,
    reason = "test fixtures are built on the host, which is the one place that is the point"
)]

/// Set by CI on a lane that is expected to run the test for real. When it is
/// set, every reason for not running becomes a failure -- otherwise a kernel
/// downgrade, a dropped feature flag or a misspelled cfg turns the gate into a
/// no-op that still reports success.
const REQUIRED_ENV: &str = "BRUSH_LANDLOCK_REQUIRED";

#[cfg(not(all(unix, feature = "landlock-tests")))]
fn main() -> std::process::ExitCode {
    let reason = "the Landlock completeness test requires --features landlock-tests";
    if std::env::var_os(REQUIRED_ENV).is_some() {
        eprintln!("FAILED: {REQUIRED_ENV} is set but {reason}");
        return std::process::ExitCode::FAILURE;
    }
    eprintln!("skipped: {reason}");
    std::process::ExitCode::SUCCESS
}

#[cfg(all(unix, feature = "landlock-tests"))]
fn main() -> std::process::ExitCode {
    harness::main()
}

/// Applying the ruleset. Everything Linux-specific lives here, so that the
/// rest of the test stays type-checked when it is built anywhere else.
#[cfg(all(unix, feature = "landlock-tests"))]
mod ruleset {
    use std::path::Path;

    /// Why the test could not run, as opposed to why it failed.
    pub struct Skip(pub String);

    /// ABI 3 (Linux 6.2) is the floor because it is the first to gate
    /// `LANDLOCK_ACCESS_FS_TRUNCATE`. Below it, `> file` truncates whatever the
    /// process can reach regardless of the ruleset, and the shell's single most
    /// common filesystem operation would be untested.
    #[cfg(target_os = "linux")]
    const REQUIRED_ABI: landlock::ABI = landlock::ABI::V3;

    /// Confines this process to the namespace's mount roots.
    ///
    /// Irreversible, and inherited by every thread created afterwards, which is
    /// why the caller's runtime is single-threaded and built beforehand.
    #[cfg(target_os = "linux")]
    pub fn restrict(mount_root: &Path) -> Result<(), Skip> {
        // `TMPDIR` was pointed at a sibling of the mount root before this ran,
        // so the two regions below are disjoint and both are narrow. Allowing
        // the process's ordinary temporary directory instead would subsume the
        // mount root, which is created there, and reduce the ruleset to
        // "anywhere in /tmp".
        use landlock::{
            Access as _, AccessFs, CompatLevel, Compatible as _, RulesetAttr as _,
            RulesetCreatedAttr as _, RulesetStatus, path_beneath_rules,
        };

        let all = AccessFs::from_all(REQUIRED_ABI);

        // The shell's scratch space, which is *not* in the namespace: every
        // here-document materializes through a host temporary file (D38), and
        // that is a deliberate exemption rather than an oversight. Allowing it
        // is what lets the here-doc case mean anything; without it that case
        // would fail for a reason with nothing to do with routing.
        let scratch = std::env::temp_dir();

        let describe = |e: landlock::RulesetError| {
            Skip(format!(
                "Landlock ABI {REQUIRED_ABI:?} is unavailable on this kernel ({e})"
            ))
        };

        let ruleset = landlock::Ruleset::default()
            .set_compatibility(CompatLevel::HardRequirement)
            .handle_access(all)
            .map_err(describe)?
            .create()
            .map_err(describe)?
            .add_rules(path_beneath_rules([mount_root, scratch.as_path()], all))
            .map_err(describe)?;

        let status = ruleset.restrict_self().map_err(describe)?;

        match status.ruleset {
            RulesetStatus::FullyEnforced => Ok(()),
            other => Err(Skip(format!("ruleset was not fully enforced: {other:?}"))),
        }
    }

    /// Everywhere else there is no kernel primitive to apply, so the test can
    /// only report that it did not run. It still type-checks here, which is
    /// most of why the split exists.
    #[cfg(not(target_os = "linux"))]
    pub fn restrict(_mount_root: &Path) -> Result<(), Skip> {
        Err(Skip("Landlock is a Linux facility".to_owned()))
    }
}

#[cfg(all(unix, feature = "landlock-tests"))]
mod harness {
    use std::path::Path;
    use std::process::ExitCode;

    use super::ruleset::{Skip, restrict};

    /// A shell script and what it must produce, run entirely inside the mount.
    struct Case {
        name: &'static str,
        script: &'static str,
        expected: &'static str,
    }

    /// Each case writes its answer to `/work/result`, which is then read back
    /// through the namespace. Going through a file rather than stdout is
    /// deliberate: it exercises the redirect path on the way out.
    const CASES: &[Case] = &[
        Case {
            name: "redirect out and back",
            script: "echo alpha > /work/out.txt; read -r line < /work/out.txt; \
                     echo \"$line\" > /work/result",
            expected: "alpha",
        },
        Case {
            name: "append",
            script: "echo one > /work/a.txt; echo two >> /work/a.txt; \
                     read -r first < /work/a.txt; echo \"$first\" > /work/result",
            expected: "one",
        },
        Case {
            name: "glob expansion reads the directory",
            script: "echo x > /work/g1.txt; echo x > /work/g2.txt; cd /work; \
                     set -- *.txt; echo \"$#\" > /work/result",
            expected: "2",
        },
        Case {
            name: "test predicates probe the file",
            script: "echo body > /work/p.txt; \
                     if [[ -e /work/p.txt && -f /work/p.txt && -r /work/p.txt \
                           && -s /work/p.txt && -d /work ]]; then \
                       echo yes > /work/result; else echo no > /work/result; fi",
            expected: "yes",
        },
        Case {
            name: "cd and pwd",
            script: "cd /work; pwd > /work/result",
            expected: "/work",
        },
        Case {
            name: "cd -P resolves physically",
            script: "cd /work; cd -P .; pwd > /work/result",
            expected: "/work",
        },
        Case {
            name: "source reads and runs a file",
            script: "echo 'sourced=42' > /work/s.sh; . /work/s.sh; \
                     echo \"$sourced\" > /work/result",
            expected: "42",
        },
        Case {
            name: "command substitution",
            script: "echo inner > /work/c.txt; out=$(read -r l < /work/c.txt; echo \"$l\"); \
                     echo \"$out\" > /work/result",
            expected: "inner",
        },
        Case {
            name: "here-doc materializes",
            script: "read -r line <<EOF\nheredoc\nEOF\necho \"$line\" > /work/result",
            expected: "heredoc",
        },
        Case {
            name: "the null device is still writable",
            script: "echo discarded > /dev/null; echo \"$?\" > /work/result",
            expected: "0",
        },
        Case {
            name: "command completion walks PATH",
            script: "PATH=/work; compgen -c nothing-matches-this; \
                     echo \"walked\" > /work/result",
            expected: "walked",
        },
    ];

    pub fn main() -> ExitCode {
        let scratch = match tempfile::tempdir() {
            Ok(dir) => dir,
            Err(e) => {
                eprintln!("failed to create scratch directory: {e}");
                return ExitCode::FAILURE;
            }
        };

        // Two directories, deliberately siblings rather than one inside the
        // other.
        //
        // The shell needs a scratch space that is *not* in the namespace: every
        // here-document materializes through `tempfile::tempfile()` (D38), which
        // resolves against `TMPDIR`. If that scratch were the process's ordinary
        // temporary directory, the ruleset would have to allow all of it -- and
        // since the mount root is itself created there, allowing it would
        // subsume the mount root and the ruleset would reduce to "anywhere in
        // /tmp". An unrouted access to any other temporary file would then pass.
        //
        // So `TMPDIR` is pointed at a directory of our own first, and the two
        // allowed regions are narrow and disjoint.
        let mount_root = scratch.path().join("mount");
        let shell_scratch = scratch.path().join("scratch");
        for dir in [&mount_root, &shell_scratch] {
            if let Err(e) = std::fs::create_dir(dir) {
                eprintln!("failed to create {}: {e}", dir.display());
                return ExitCode::FAILURE;
            }
        }

        // SAFETY: single-threaded; no runtime has been built yet and no other
        // thread exists to observe the environment concurrently.
        unsafe {
            std::env::set_var("TMPDIR", &shell_scratch);
        }

        // Everything that must touch the host happens here, before the
        // ruleset is applied: opening the mount's directory handle, taking the
        // null device's descriptor, reading the process's working directory.
        let runtime = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime,
            Err(e) => {
                eprintln!("failed to build the runtime: {e}");
                return ExitCode::FAILURE;
            }
        };

        let shell = match runtime.block_on(build_shell(&mount_root)) {
            Ok(shell) => shell,
            Err(e) => {
                eprintln!("failed to build the shell: {e}");
                return ExitCode::FAILURE;
            }
        };

        match restrict(&mount_root) {
            Ok(()) => {}
            Err(Skip(reason)) => {
                // Gate 8 requires that a kernel regression below ABI 3 fails
                // rather than quietly stops testing anything. `PartiallyEnforced`
                // and `NotEnforced` arrive here too, and both mean the ruleset
                // is not the one the cases were written against.
                let required = super::REQUIRED_ENV;
                if std::env::var_os(required).is_some() {
                    eprintln!("FAILED: {required} is set but {reason}");
                    return ExitCode::FAILURE;
                }
                eprintln!("skipped: {reason}");
                return ExitCode::SUCCESS;
            }
        }

        runtime.block_on(run_cases(shell, &mount_root))
    }

    /// Builds a shell confined to `mount_root`, mounted at `/work`.
    async fn build_shell(
        mount_root: &Path,
    ) -> Result<brush_core::Shell, Box<dyn std::error::Error>> {
        let mut shell = brush_core::Shell::builder()
            .builtins(brush_builtins::default_builtins(
                brush_builtins::BuiltinSet::BashMode,
            ))
            .profile(brush_core::ProfileLoadBehavior::Skip)
            .rc(brush_core::RcLoadBehavior::Skip)
            .working_dir(mount_root.to_path_buf())
            .build()
            .await?;

        let mounts = brush_core::vfs::MountTable::builder()
            .mount("/work", mount_root, brush_core::vfs::Access::ReadWrite)?
            .build()?;
        shell.set_mounts(mounts);
        shell.set_working_dir("/work")?;

        Ok(shell)
    }

    async fn run_cases(mut shell: brush_core::Shell, mount_root: &Path) -> ExitCode {
        let mut failures = 0usize;

        for case in CASES {
            // A fresh shell per case would have to open the mount again, which
            // the ruleset now forbids, so the cases share one and clean up
            // after themselves.
            let result_path = mount_root.join("result");
            let _ = std::fs::remove_file(&result_path);

            let params = shell.default_exec_params();
            let source = brush_core::SourceInfo::from(std::path::PathBuf::from(case.name));
            match shell.run_string(case.script, &source, &params).await {
                Ok(result) if result.is_success() => {}
                Ok(result) => {
                    eprintln!(
                        "FAILED [{}]: exited {}",
                        case.name,
                        u8::from(result.exit_code)
                    );
                    failures += 1;
                    continue;
                }
                Err(e) => {
                    eprintln!("FAILED [{}]: {e}", case.name);
                    failures += 1;
                    continue;
                }
            }

            let actual = match std::fs::read_to_string(&result_path) {
                Ok(text) => text.trim_end_matches('\n').to_owned(),
                Err(e) => {
                    eprintln!("FAILED [{}]: could not read the result: {e}", case.name);
                    failures += 1;
                    continue;
                }
            };

            if actual == case.expected {
                eprintln!("ok [{}]", case.name);
            } else {
                eprintln!(
                    "FAILED [{}]: expected {:?}, got {:?}",
                    case.name, case.expected, actual
                );
                failures += 1;
            }
        }

        if failures == 0 {
            eprintln!("{} case(s) ran under Landlock, all passing.", CASES.len());
            ExitCode::SUCCESS
        } else {
            eprintln!(
                "{failures} of {} case(s) failed under Landlock. A case that passes without the \
                 ruleset and fails with it names a filesystem access that did not route through \
                 brush-vfs.",
                CASES.len()
            );
            ExitCode::FAILURE
        }
    }
}
