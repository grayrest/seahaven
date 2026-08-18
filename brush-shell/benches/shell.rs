//! Benchmarks for the brush-shell crate.

#![allow(missing_docs)]
#![allow(clippy::unwrap_used)]
#![allow(
    clippy::disallowed_methods,
    reason = "benchmark fixtures are built on the host, which is the one place that is the point"
)]

#[cfg(unix)]
mod unix {
    use brush_builtins::ShellBuilderExt;
    use brush_parser::SourceSpan;
    use criterion::Criterion;
    use std::hint::black_box;

    /// How many files the glob benchmark's directory holds. Large enough that
    /// the per-entry cost dominates the fixed cost of one listing.
    const GLOB_ENTRY_COUNT: usize = 200;

    async fn instantiate_shell() -> brush_core::Shell {
        brush_core::Shell::builder()
            .default_builtins(brush_builtins::BuiltinSet::BashMode)
            .build()
            .await
            .unwrap()
    }

    async fn instantiate_shell_with_init_scripts() -> brush_core::Shell {
        brush_core::Shell::builder()
            .interactive(true)
            .read_commands_from_stdin(true)
            .default_builtins(brush_builtins::BuiltinSet::BashMode)
            .build()
            .await
            .unwrap()
    }

    async fn run_one_command(shell: &mut brush_core::Shell, command: &str) {
        let _ = shell
            .run_string(
                command.to_owned(),
                &brush_core::SourceInfo::default(),
                &shell.default_exec_params(),
            )
            .await
            .unwrap();
    }

    async fn expand_string(shell: &mut brush_core::Shell, s: &str) {
        let params = shell.default_exec_params();
        let _ = shell.basic_expand_string(&params, s).await.unwrap();
    }

    /// Full expansion, which is the one that includes pathname expansion.
    /// `basic_expand_string` deliberately stops short of globbing.
    async fn expand_and_split_string(shell: &mut brush_core::Shell, s: &str) -> usize {
        let params = shell.default_exec_params();
        shell
            .full_expand_and_split_string(&params, s)
            .await
            .unwrap()
            .len()
    }

    fn eval_arithmetic_expr(shell: &mut brush_core::Shell, expr: &str) {
        let parsed_expr = brush_parser::arithmetic::parse(expr).unwrap();
        let _ = shell.eval_arithmetic(&parsed_expr).unwrap();
    }

    /// This function defines core shell benchmarks.
    pub(crate) fn criterion_benchmark(c: &mut Criterion) {
        // Construct a runtime for us to run async code on.
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();

        // Benchmark shell instantiation.
        c.bench_function("instantiate_shell", |b| {
            b.to_async(&rt).iter(|| black_box(instantiate_shell()));
        });
        c.bench_function("instantiate_shell_with_init_scripts", |b| {
            b.to_async(&rt)
                .iter(|| black_box(instantiate_shell_with_init_scripts()));
        });

        // Benchmark: cloning a shell object.
        let shell = rt.block_on(instantiate_shell());
        c.bench_function("clone_shell_object", |b| {
            b.iter(|| black_box(shell.clone()));
        });

        // Benchmark: parsing and evaluating an arithmetic expression..
        let shell = rt.block_on(instantiate_shell());
        c.bench_function("eval_arithmetic", |b| {
            b.iter_batched_ref(
                || shell.clone(),
                |s| eval_arithmetic_expr(s, "3 + 10 * 2"),
                criterion::BatchSize::SmallInput,
            );
        });

        // Benchmark: running the echo built-in command.
        let shell = rt.block_on(instantiate_shell());
        c.bench_function("run_echo_builtin_command", |b| {
            b.iter_batched_ref(
                || shell.clone(),
                |s| rt.block_on(run_one_command(s, "echo 'Hello, world!' >/dev/null")),
                criterion::BatchSize::SmallInput,
            );
        });

        // Benchmark: running an external command.
        // let shell = rt.block_on(instantiate_shell());
        // c.bench_function("run_one_external_command", |b| {
        //     b.iter_batched_ref(
        //         || shell.clone(),
        //         |s| {
        //             rt.block_on(run_one_command(
        //                 s,
        //                 "/usr/bin/echo 'Hello, world!' >/dev/null",
        //             ));
        //         },
        //         criterion::BatchSize::SmallInput,
        //     );
        // });

        // Benchmark: word expansion.
        let shell = rt.block_on(instantiate_shell());
        c.bench_function("expand_one_string", |b| {
            b.iter_batched_ref(
                || shell.clone(),
                |s| rt.block_on(expand_string(s, "My version is ${BASH_VERSINFO[@]}")),
                criterion::BatchSize::SmallInput,
            );
        });

        // Benchmark: function invocation.
        let mut shell = rt.block_on(instantiate_shell());
        shell.define_func(
            String::from("testfunc"),
            brush_parser::ast::FunctionDefinition {
                fname: String::from("testfunc").into(),
                body: brush_parser::ast::FunctionBody(
                    brush_parser::ast::CompoundCommand::BraceGroup(
                        brush_parser::ast::BraceGroupCommand {
                            list: brush_parser::ast::CompoundList(vec![]),
                            loc: SourceSpan::default(),
                        },
                    ),
                    None,
                ),
            },
            &brush_core::SourceInfo::default(),
        );
        c.bench_function("function_call", |b| {
            b.iter_batched_ref(
                || shell.clone(),
                |s| {
                    rt.block_on(run_one_command(s, "testfunc"));
                },
                criterion::BatchSize::SmallInput,
            );
        });

        // Benchmark: for loop.
        let shell = rt.block_on(instantiate_shell());
        c.bench_function("for_loop", |b| {
            b.iter_batched_ref(
                || shell.clone(),
                |s| {
                    rt.block_on(run_one_command(s, "for ((i = 0; i < 10; i++)); do :; done"));
                },
                criterion::BatchSize::SmallInput,
            );
        });

        // Benchmark: glob expansion over a populated directory.
        //
        // Pathname expansion is one directory listing plus one probe per
        // candidate, and both go through the namespace now. It is tracked
        // because a per-entry cost here is multiplied by the size of whatever
        // directory the user happens to be standing in.
        let glob_dir = tempfile::tempdir().unwrap();
        for i in 0..GLOB_ENTRY_COUNT {
            std::fs::write(glob_dir.path().join(format!("entry-{i:03}.txt")), "").unwrap();
        }
        let mut shell = rt.block_on(instantiate_shell());
        shell.set_working_dir(glob_dir.path()).unwrap();
        c.bench_function("glob_expansion", |b| {
            b.iter_batched_ref(
                || shell.clone(),
                |s| {
                    let matched = rt.block_on(expand_and_split_string(s, "*.txt"));
                    // A glob that matched nothing would make this benchmark
                    // measure the parser rather than the namespace.
                    assert_eq!(matched, GLOB_ENTRY_COUNT);
                },
                criterion::BatchSize::SmallInput,
            );
        });

        // Benchmark: resolving a command name against PATH.
        //
        // The largest predicted cost of routing the shell through the
        // namespace: executability used to be a mode-bit check on a
        // `Metadata`, and is an `access(2)` through the namespace now -- once
        // per PATH entry, per command, on the miss path. Tracked separately
        // from `run_echo_builtin_command` because a builtin never reaches it.
        let shell = rt.block_on(instantiate_shell());
        c.bench_function("find_first_executable_in_path", |b| {
            b.iter(|| black_box(shell.find_first_executable_in_path("sh")));
        });

        // The same lookup for a name that is in no PATH entry, which is the
        // expensive direction: a hit stops at the first match, a miss pays for
        // every entry.
        let shell = rt.block_on(instantiate_shell());
        c.bench_function("find_first_executable_in_path_miss", |b| {
            b.iter(|| black_box(shell.find_first_executable_in_path("brush-no-such-command")));
        });
    }
}

#[cfg(unix)]
criterion::criterion_group! {
    name = benches;
    config = criterion::Criterion::default()
                .measurement_time(std::time::Duration::from_secs(10));
    targets = unix::criterion_benchmark
}

#[cfg(unix)]
criterion::criterion_main!(benches);

#[cfg(not(unix))]
fn main() {}
