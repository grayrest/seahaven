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
    // `-D warnings` is load-bearing, not belt-and-braces: `disallowed_methods`
    // is warn-by-default, so without it the filesystem ban reports violations
    // and exits 0.
    let mut args = vec!["clippy", "--workspace", "--all-features", "--all-targets"];
    if verbose {
        args.push("--verbose");
        eprintln!("Running: cargo {} -- -D warnings", args.join(" "));
    }
    args.push("--");
    args.push("-D");
    args.push("warnings");
    cmd!(sh, "cargo {args...}")
        .run()
        .context("Clippy check failed")?;
    eprintln!("Clippy check passed.");
    Ok(())
}

/// Path of the crate that must fail to lint, relative to the workspace root.
const BAN_FIXTURE: &str = "xtask/fixtures/banned-fs-access";

/// Verify that the filesystem ban in `clippy.toml` is switched on and complete.
///
/// The ban has three ways of quietly ceasing to exist, and this checks all
/// three:
///
/// - An entry naming a path that does not resolve is *silently* ignored --
///   clippy emits nothing at all and exits 0 -- so a typo or a rename in std
///   disables one ban invisibly. The fixture uses every entry exactly once and
///   must produce exactly one diagnostic per entry.
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
/// `disallowed_methods` is warn-by-default, so a `cargo clippy` without
/// `-D warnings` reports every violation and exits 0. The fixture above proves
/// the *entries* are live, but it passes its own `-D warnings` -- so on its own
/// it only tests its own literal. This reads the real invocation instead.
///
/// Textual rather than executed, because running the workspace lint from inside
/// a check that the workspace lint runs would recurse.
fn check_lint_denies_warnings() -> Result<()> {
    let root = crate::common::find_workspace_root()?;
    let this_file = root.join("xtask/src/check.rs");
    let source = std::fs::read_to_string(&this_file)
        .with_context(|| format!("reading {}", this_file.display()))?;

    let body = source
        .split_once("fn check_lint(")
        .map(|(_, rest)| rest)
        .and_then(|rest| rest.split_once("\nfn "))
        .map(|(body, _)| body)
        .context("could not find check_lint's body in xtask/src/check.rs")?;

    if !(body.contains("\"-D\"") && body.contains("\"warnings\"")) {
        anyhow::bail!(
            "check_lint no longer passes `-D warnings`; the filesystem ban would report \
             violations and still exit 0"
        );
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
