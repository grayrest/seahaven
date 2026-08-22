//! Gate 1: this crate is a workspace member, so `clippy --workspace` and the
//! filesystem ban reach it.
//!
//! `check ban` cannot assert this — it lints one fixture crate and never
//! compiles a workspace member (its own doc comment says so). And workspace
//! membership is an explicit list with no globs, so a crate that is built by
//! some other means — a `staticlib` linked by `roc`, say — can sit in the tree
//! excluded, exactly as `forks/` does, and every `--workspace` gate would pass
//! while never seeing it. The Roc host this milestone builds toward is that
//! shape, so the risk is not hypothetical.
//!
//! This reads the root `Cargo.toml` and fails if `brush-platform` is not a
//! member. It is a test rather than a lint because the fact it guards is a line
//! in a manifest, which no compiler checks.

#![cfg(test)]
#![allow(
    clippy::disallowed_methods,
    clippy::expect_used,
    reason = "the test reads the workspace manifest on the host, which is the point"
)]

#[test]
fn brush_platform_is_a_workspace_member() {
    // The crate dir is `<root>/brush-platform`; the workspace root is its
    // parent. Resolved at compile time, so this does not itself depend on a cwd.
    let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let root = manifest_dir
        .parent()
        .expect("brush-platform has a parent directory");
    let workspace_manifest = std::fs::read_to_string(root.join("Cargo.toml"))
        .expect("the workspace Cargo.toml is readable");

    // A deliberately dumb scan rather than a TOML parse: the property is "the
    // string `brush-platform` appears as a member entry", and a parser would be
    // a dependency this gate does not need. The quotes pin it to a list entry
    // rather than a stray mention in a comment.
    assert!(
        workspace_manifest.contains("\"brush-platform\""),
        "brush-platform is not in the workspace `members` list, so \
         `clippy --workspace` and the filesystem ban do not see it"
    );
}
