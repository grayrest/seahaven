//! Check commands for code quality validation.
//!
//! This module provides various code quality checks that can be run individually
//! or as part of a CI workflow. Each check wraps an external tool and provides
//! consistent error handling and verbose output.
//!
//! Some checks require additional tools to be installed:
//! - `cargo-deny`: Security/license auditing (`cargo install cargo-deny`)
//! - `cargo-udeps`: Unused dependency detection (`cargo install cargo-udeps`, requires nightly)
//! - `cargo-public-api`: Public API analysis (`cargo install cargo-public-api`, requires nightly)
//! - `typos`: Spelling checker (`cargo install typos-cli`)
//! - `zizmor`: GitHub workflow security scanner (`pip install zizmor`)
//! - `lychee`: Link checker (`cargo install lychee`)

use anyhow::{Context, Result};
use clap::Parser;
use xshell::{Shell, cmd};

/// Run code quality checks.
#[derive(Parser)]
pub enum CheckCommand {
    /// Check that the filesystem ban is switched on and complete.
    Ban,
    /// Check that the code compiles.
    Build,
    /// Check dependencies for security vulnerabilities and license compliance.
    Deps,
    /// Check code formatting.
    Fmt,
    /// Check for broken links in documentation.
    Links,
    /// Run clippy lints.
    Lint,
    /// Analyze public API for breaking changes (requires nightly).
    PublicApi,
    /// Check that generated schemas are up-to-date.
    Schemas,
    /// Check for spelling errors.
    Spelling,
    /// Check for unused dependencies (requires nightly).
    UnusedDeps,
    /// Check GitHub workflow files for security issues.
    Workflows,
}

/// Run a check command.
pub fn run(cmd: &CheckCommand, verbose: bool) -> Result<()> {
    let sh = Shell::new()?;

    match cmd {
        CheckCommand::Ban => check_ban(&sh, verbose),
        CheckCommand::Fmt => check_fmt(&sh, verbose),
        CheckCommand::Lint => check_lint(&sh, verbose),
        CheckCommand::Deps => check_deps(&sh, verbose),
        CheckCommand::UnusedDeps => check_unused_deps(&sh, verbose),
        CheckCommand::Build => check_build(&sh, verbose),
        CheckCommand::Schemas => check_schemas(&sh, verbose),
        CheckCommand::PublicApi => check_public_api(&sh, verbose),
        CheckCommand::Spelling => check_spelling(&sh, verbose),
        CheckCommand::Workflows => check_workflows(&sh, verbose),
        CheckCommand::Links => check_links(&sh, verbose),
    }
}

fn check_fmt(sh: &Shell, verbose: bool) -> Result<()> {
    eprintln!("Checking code formatting...");
    if verbose {
        eprintln!("Running: cargo fmt --check --all");
    }
    cmd!(sh, "cargo fmt --check --all")
        .run()
        .context("Format check failed")?;
    eprintln!("Format check passed.");
    Ok(())
}

fn check_lint(sh: &Shell, verbose: bool) -> Result<()> {
    eprintln!("Running clippy...");
    let args = lint_args(verbose);

    // Checked here, on the argv about to be used, rather than only from
    // `check_ban`. A guard that inspects some *other* assembly of the arguments
    // is defeated by shadowing whatever it inspects; this one cannot be, because
    // it is the same `args` that reaches `cargo` on the next line.
    assert_denies_warnings(&args)?;

    if verbose {
        eprintln!("Running: cargo {}", args.join(" "));
    }
    cmd!(sh, "cargo {args...}")
        .run()
        .context("Clippy check failed")?;
    eprintln!("Clippy check passed.");
    Ok(())
}

/// The exact argv the workspace lint runs, and the only place it is assembled.
///
/// The trailing `-D warnings` is load-bearing: `disallowed_methods` is
/// warn-by-default, so without it the filesystem ban reports every violation
/// and exits 0. Earlier guards over this were defeated four ways in review --
/// a comment naming the flags, a constant shadowed inside `check_lint`, a
/// second `fn check_lint` that the text search found first, and a splice
/// wrapped in `if env::var("CI").is_err()`. All four worked because the guard
/// read source text; [`check_lint_denies_warnings`] reads this function's
/// return value instead.
fn lint_args(verbose: bool) -> Vec<&'static str> {
    let mut args = vec!["clippy", "--workspace", "--all-features", "--all-targets"];
    if verbose {
        args.push("--verbose");
    }
    args.extend_from_slice(&["--", "-D", "warnings"]);
    args
}

/// Path of the crate that must fail to lint, relative to the workspace root.
const BAN_FIXTURE: &str = "xtask/fixtures/banned-fs-access";

/// Verify that the filesystem ban in `clippy.toml` is switched on and complete.
///
/// The ban has three ways of quietly ceasing to exist, and this checks all
/// three:
///
/// - An entry naming a path that does not resolve disables one ban almost
///   invisibly: clippy emits a plain warning rather than a lint, so
///   `-D warnings` does not escalate it and the build stays green. The fixture
///   uses every entry exactly once and must produce exactly one diagnostic per
///   entry.
///
///   The fixture is its own workspace, so its dependency graph is not the
///   workspace's: an entry can fire here and be inert in `brush-core` if the
///   crate or feature it names is absent there. That gap is not covered.
/// - A `clippy.toml` in a member crate shadows the root one outright rather
///   than merging with it, so there must be exactly one in the tree.
/// - `disallowed_methods` is a warn-by-default lint, so a CI invocation
///   without `-D warnings` reports it and exits 0 regardless.
fn check_ban(sh: &Shell, verbose: bool) -> Result<()> {
    eprintln!("Checking the filesystem ban...");
    let root = crate::common::find_workspace_root()?;

    // Exactly one clippy.toml: a member crate's would shadow the root's.
    let found = find_clippy_configs(&root)?;
    if found.len() != 1 {
        anyhow::bail!(
            "expected exactly one clippy.toml in the tree, found {}: {}",
            found.len(),
            found
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

    let expected = banned_paths(&found[0])?;
    if expected.is_empty() {
        anyhow::bail!("clippy.toml declares no disallowed-methods; the ban is not switched on");
    }

    // Lint the positive control. It must fail, and it must name every entry.
    let fixture = root.join(BAN_FIXTURE);
    let manifest = fixture.join("Cargo.toml");
    if verbose {
        eprintln!("Running: cargo clippy on {}", fixture.display());
    }

    // Clippy caches by crate fingerprint, and the fixture's source rarely
    // changes even when clippy.toml does, so a stale result would pass the
    // check without linting anything.
    cmd!(sh, "cargo clean --manifest-path {manifest}")
        .quiet()
        .run()
        .context("Failed to clean the ban fixture")?;

    let output = cmd!(
        sh,
        "cargo clippy --manifest-path {manifest} --message-format short -- -D warnings"
    )
    .env("CLIPPY_CONF_DIR", &root)
    .ignore_status()
    .quiet()
    .read_stderr()
    .context("Failed to run clippy on the ban fixture")?;

    let mut counts: std::collections::BTreeMap<&str, usize> = expected
        .iter()
        .filter(|path| resolves_on_this_platform(path))
        .map(|path| (path.as_str(), 0usize))
        .collect();
    let mut unexpected = Vec::new();
    for reported in reported_paths(&output) {
        match counts.get_mut(reported) {
            Some(count) => *count += 1,
            None => unexpected.push(reported.to_owned()),
        }
    }

    let never_fired: Vec<&str> = counts
        .iter()
        .filter(|(_, count)| **count == 0)
        .map(|(path, _)| *path)
        .collect();
    if !never_fired.is_empty() {
        anyhow::bail!(
            "these bans produced no diagnostic -- either the path no longer resolves (clippy \
             ignores unresolvable entries silently) or {BAN_FIXTURE} does not use them:\n  {}",
            never_fired.join("\n  ")
        );
    }
    if !unexpected.is_empty() {
        anyhow::bail!(
            "{BAN_FIXTURE} tripped bans it does not declare a use for:\n  {}",
            unexpected.join("\n  ")
        );
    }

    // Every entry fired, so the fixture must have failed to lint. If it did
    // not, `-D warnings` is not reaching the invocation.
    if !output.contains("error") {
        anyhow::bail!(
            "the ban fixture reported diagnostics but clippy did not fail; \
             `-D warnings` is not taking effect"
        );
    }

    check_lint_denies_warnings()?;

    eprintln!("Ban check passed ({} entries, all firing).", counts.len());
    Ok(())
}

/// Verify that the workspace lint invocation actually denies warnings.
///
/// Inspects the argv [`lint_args`] returns. Reading the value rather than the
/// source is the point: a textual guard cannot tell an argument from a comment
/// about one, cannot see a shadowed constant, takes the first of two
/// identically-named functions, and is satisfied by a splice a condition skips.
fn check_lint_denies_warnings() -> Result<()> {
    for verbose in [false, true] {
        assert_denies_warnings(&lint_args(verbose))?;
    }
    Ok(())
}

/// The predicate itself: this argv must deny warnings and must not take the
/// denial back.
fn assert_denies_warnings(args: &[&str]) -> Result<()> {
    {
        let Some(separator) = args.iter().position(|a| *a == "--") else {
            anyhow::bail!("the lint argv has no `--` separator: {args:?}");
        };
        let rustc_args = &args[separator + 1..];

        if !rustc_args.windows(2).any(|w| w == ["-D", "warnings"]) {
            anyhow::bail!(
                "the lint argv does not deny warnings: {args:?}; the filesystem ban would \
                 report violations and still exit 0"
            );
        }

        // Nothing after it may take the ban back: a later `-A` overrides an
        // earlier `-D`, so a trailing allow silently undoes the whole list.
        if let Some(allow) = rustc_args
            .iter()
            .position(|a| *a == "-A" || a.starts_with("--allow"))
        {
            anyhow::bail!(
                "the lint argv allows lints back at position {allow}: {args:?}; a later `-A` \
                 overrides `-D warnings`"
            );
        }
    }

    Ok(())
}

/// Whether a banned path can resolve on the platform running the check.
///
/// An entry that cannot resolve produces no diagnostic, which is normally the
/// failure this check exists to catch -- but a `nix::` path on Windows is
/// absent by design rather than by mistake. The alternative, one ban list per
/// platform, would put the Unix surface out of a Windows reader's sight.
fn resolves_on_this_platform(path: &str) -> bool {
    const UNIX_ONLY: [&str; 3] = ["std::os::unix::", "nix::", "libc::"];
    const WINDOWS_ONLY: [&str; 1] = ["std::os::windows::"];

    if UNIX_ONLY.iter().any(|prefix| path.starts_with(prefix)) {
        return cfg!(unix);
    }
    if WINDOWS_ONLY.iter().any(|prefix| path.starts_with(prefix)) {
        return cfg!(windows);
    }
    true
}

/// Returns every `clippy.toml` in the tree, ignoring build output.
fn find_clippy_configs(root: &std::path::Path) -> Result<Vec<std::path::PathBuf>> {
    fn walk(dir: &std::path::Path, found: &mut Vec<std::path::PathBuf>) -> Result<()> {
        for entry in dir.read_dir()? {
            let entry = entry?;
            let path = entry.path();
            let name = entry.file_name();
            if entry.file_type()?.is_dir() {
                if name == "target" || name == ".git" {
                    continue;
                }
                walk(&path, found)?;
            } else if name == "clippy.toml" || name == ".clippy.toml" {
                found.push(path);
            }
        }
        Ok(())
    }

    let mut found = Vec::new();
    walk(root, &mut found)?;
    found.sort();
    Ok(found)
}

/// Reads the banned method paths out of a `clippy.toml`.
fn banned_paths(config: &std::path::Path) -> Result<Vec<String>> {
    let text =
        std::fs::read_to_string(config).with_context(|| format!("reading {}", config.display()))?;
    let parsed: toml::Value =
        toml::from_str(&text).with_context(|| format!("parsing {}", config.display()))?;

    let Some(entries) = parsed
        .get("disallowed-methods")
        .and_then(toml::Value::as_array)
    else {
        return Ok(Vec::new());
    };

    Ok(entries
        .iter()
        .filter_map(|entry| match entry {
            toml::Value::String(path) => Some(path.clone()),
            other => other
                .get("path")
                .and_then(toml::Value::as_str)
                .map(ToOwned::to_owned),
        })
        .collect())
}

/// Extracts the method paths clippy named in its diagnostics.
fn reported_paths(output: &str) -> Vec<&str> {
    const MARKER: &str = "use of a disallowed method `";
    output
        .split(MARKER)
        .skip(1)
        .filter_map(|rest| rest.split('`').next())
        .collect()
}

fn check_deps(sh: &Shell, verbose: bool) -> Result<()> {
    eprintln!("Checking dependencies...");
    if verbose {
        eprintln!("Running: cargo deny --all-features check all");
    }
    cmd!(sh, "cargo deny --all-features check all")
        .run()
        .context("Dependency check failed")?;

    check_vfs_has_no_features()?;

    eprintln!("Dependency check passed.");
    Ok(())
}

/// Verify that `brush-vfs` declares no cargo features.
///
/// A feature on the crate that decides what a path means is a second answer to
/// the same question: two builds of the shell would resolve differently, and
/// only one of them would be the one under test. "No features that alter
/// resolution" is only checkable as "no features", since nothing stops a
/// feature added for another reason from reaching resolution later.
fn check_vfs_has_no_features() -> Result<()> {
    let root = crate::common::find_workspace_root()?;
    let manifest = root.join("brush-vfs/Cargo.toml");
    let text = std::fs::read_to_string(&manifest)
        .with_context(|| format!("reading {}", manifest.display()))?;
    let parsed: toml::Value =
        toml::from_str(&text).with_context(|| format!("parsing {}", manifest.display()))?;

    let declared: Vec<&str> = parsed
        .get("features")
        .and_then(toml::Value::as_table)
        .map(|table| table.keys().map(String::as_str).collect())
        .unwrap_or_default();

    if declared.is_empty() {
        Ok(())
    } else {
        anyhow::bail!(
            "brush-vfs declares cargo features ({}); path resolution must not have build-time \
             variants",
            declared.join(", ")
        )
    }
}

fn check_unused_deps(sh: &Shell, verbose: bool) -> Result<()> {
    eprintln!("Checking for unused dependencies (requires nightly)...");
    if verbose {
        eprintln!("Running: cargo +nightly udeps --workspace --all-targets --all-features");
    }
    cmd!(
        sh,
        "cargo +nightly udeps --workspace --all-targets --all-features"
    )
    .run()
    .context("Unused dependency check failed")?;
    eprintln!("Unused dependency check passed.");
    Ok(())
}

fn check_build(sh: &Shell, verbose: bool) -> Result<()> {
    eprintln!("Checking that code compiles...");
    let mut args = vec!["check", "--all-features", "--all-targets", "--workspace"];
    if verbose {
        args.push("--verbose");
        eprintln!("Running: cargo {}", args.join(" "));
    }
    cmd!(sh, "cargo {args...}")
        .run()
        .context("Build check failed")?;
    eprintln!("Build check passed.");
    Ok(())
}

fn check_schemas(sh: &Shell, verbose: bool) -> Result<()> {
    eprintln!("Checking generated schemas...");

    // Regenerate schemas to a temporary state to compare against committed versions.
    if verbose {
        eprintln!(
            "Running: cargo run --package xtask -- gen schema config --out schemas/config.schema.json"
        );
    }
    cmd!(
        sh,
        "cargo run --package xtask -- gen schema config --out schemas/config.schema.json"
    )
    .run()
    .context("Failed to regenerate schemas")?;

    // Check for drift by capturing the diff output.
    // We don't use --exit-code here because we want to capture and display the
    // actual differences to help the user understand what changed.
    if verbose {
        eprintln!("Running: git diff schemas/");
    }
    let diff_output = cmd!(sh, "git diff schemas/")
        .read()
        .context("Failed to run git diff on schemas directory")?;

    if !diff_output.is_empty() {
        // Show the user exactly what changed so they can understand the drift.
        eprintln!("\nSchema drift detected. The following changes were found:\n");
        eprintln!("{diff_output}");
        anyhow::bail!(
            "Generated schemas are out of date. Please run 'cargo xtask gen schema config --out schemas/config.schema.json' and commit the changes."
        );
    }

    eprintln!("Schema check passed.");
    Ok(())
}

fn check_public_api(sh: &Shell, verbose: bool) -> Result<()> {
    eprintln!("Analyzing public API (requires nightly and cargo-public-api)...");

    // This is typically only useful for PRs comparing against main
    if verbose {
        eprintln!("Running: cargo +nightly public-api --version");
    }
    cmd!(sh, "cargo +nightly public-api --version")
        .run()
        .context("cargo-public-api not installed. Install with: cargo install cargo-public-api")?;

    eprintln!("Public API analysis complete. For PR diffs, compare against main branch.");
    Ok(())
}

fn check_spelling(sh: &Shell, verbose: bool) -> Result<()> {
    eprintln!("Checking spelling...");
    if verbose {
        eprintln!("Running: typos");
    }
    cmd!(sh, "typos")
        .run()
        .context("Spelling check failed. Install typos with: cargo install typos-cli")?;
    eprintln!("Spelling check passed.");
    Ok(())
}

fn check_workflows(sh: &Shell, verbose: bool) -> Result<()> {
    eprintln!("Checking GitHub workflows for security issues...");
    if verbose {
        eprintln!("Running: zizmor .github/workflows/");
    }
    cmd!(sh, "zizmor .github/workflows/")
        .run()
        .context("Workflow check failed. Install zizmor with: pip install zizmor")?;
    eprintln!("Workflow check passed.");
    Ok(())
}

fn check_links(sh: &Shell, verbose: bool) -> Result<()> {
    eprintln!("Checking for broken links...");
    if verbose {
        eprintln!("Running: lychee --offline docs/");
    }
    cmd!(sh, "lychee --offline docs/")
        .run()
        .context("Link check failed. Install lychee with: cargo install lychee")?;
    eprintln!("Link check passed.");
    Ok(())
}
