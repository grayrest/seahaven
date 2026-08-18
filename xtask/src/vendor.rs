//! `xtask vendor-fork <name>`: vendor a uutils crate and route it (D4/D13).
//!
//! Turns "fork the coreutils" into a repeatable operation: copy the pristine
//! upstream lib out of the cargo registry cache into `forks/<name>`, generate a
//! fork manifest (drop the binary and benches, add the `brush-vfs` dependency),
//! and run the codemod over it. The result is the same shape the hand-built
//! `uu_cat` fork has.
//!
//! It does *not* wire the fork into `brush-coreutils-builtins` -- that is a
//! one-line dependency repoint per utility, left as a deliberate step so a fork
//! is reviewed before it ships.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::codemod::{self, CodemodCommand};

/// The `vendor-fork` subcommand.
#[derive(clap::Parser)]
pub struct VendorCommand {
    /// The crate name to vendor, e.g. `uu_head`.
    pub crate_name: String,

    /// Vendor this exact version rather than the newest cached one.
    #[clap(long)]
    pub version: Option<String>,

    /// Vendor and generate the manifest but do not run the codemod.
    #[clap(long)]
    pub no_codemod: bool,
}

/// The `brush-vfs` version the forks pin, matching the workspace crate.
const BRUSH_VFS_PATH: &str = "../../brush-vfs";

/// Runs the vendor-fork command.
pub fn run(cmd: &VendorCommand, verbose: bool) -> Result<()> {
    let src = locate_crate(&cmd.crate_name, cmd.version.as_deref())?;
    eprintln!("vendoring {} from {}", cmd.crate_name, src.display());

    let dest = PathBuf::from("forks").join(&cmd.crate_name);
    if dest.exists() {
        anyhow::bail!(
            "{} already exists; remove it to re-vendor",
            dest.display()
        );
    }

    copy_lib_sources(&src, &dest)?;
    write_manifest(&src, &dest, &cmd.crate_name)?;
    ensure_lock_ignored(&dest)?;

    if cmd.no_codemod {
        eprintln!("vendored (codemod skipped). Run `cargo xtask codemod {}`", dest.join("src").display());
        return Ok(());
    }

    eprintln!("routing filesystem access...");
    codemod::run(
        &CodemodCommand {
            path: dest.join("src"),
            check: false,
        },
        verbose,
    )?;

    eprintln!(
        "\nvendored {} into {}. Repoint its dependency in \
         brush-coreutils-builtins/Cargo.toml to `path = \"../forks/{}\"`.",
        cmd.crate_name,
        dest.display(),
        cmd.crate_name
    );
    Ok(())
}

/// Finds a crate's extracted source in the cargo registry cache.
fn locate_crate(name: &str, version: Option<&str>) -> Result<PathBuf> {
    let home = std::env::var_os("CARGO_HOME")
        .map_or_else(|| dirs_home().join(".cargo"), PathBuf::from);
    let mut roots = Vec::new();
    let registry_src = home.join("registry/src");
    if let Ok(entries) = std::fs::read_dir(&registry_src) {
        for entry in entries.flatten() {
            if entry.path().is_dir() {
                roots.push(entry.path());
            }
        }
    }
    anyhow::ensure!(
        !roots.is_empty(),
        "no registry source cache under {}; run a build that depends on {name} first",
        registry_src.display()
    );

    let mut best: Option<(String, PathBuf)> = None;
    for root in roots {
        for entry in std::fs::read_dir(&root).with_context(|| format!("reading {}", root.display()))? {
            let entry = entry?;
            let file_name = entry.file_name();
            let dir_name = file_name.to_string_lossy();
            let Some(ver) = dir_name.strip_prefix(&format!("{name}-")) else {
                continue;
            };
            if let Some(want) = version {
                if ver == want {
                    return Ok(entry.path());
                }
                continue;
            }
            // Newest by string is good enough for the 0.x line the forks pin;
            // an exact version can always be given with --version.
            let is_better = best.as_ref().is_none_or(|(b, _)| ver > b.as_str());
            if is_better {
                best = Some((ver.to_string(), entry.path()));
            }
        }
    }

    best.map(|(_, p)| p).with_context(|| {
        format!("{name} is not in the registry cache; add it as a dependency and build first")
    })
}

/// A best-effort home directory for locating the default cargo cache.
fn dirs_home() -> PathBuf {
    std::env::var_os("HOME").map_or_else(|| PathBuf::from("/"), PathBuf::from)
}

/// Copies the crate's library sources, locales and license into the fork.
///
/// Deliberately omits `src/main.rs` (the binary), `benches/`, and tests: the
/// fork ships as a library, and a binary target would pull in bench/dev deps
/// the fork does not need.
fn copy_lib_sources(src: &Path, dest: &Path) -> Result<()> {
    let src_dir = src.join("src");
    anyhow::ensure!(src_dir.is_dir(), "{} has no src/", src.display());
    copy_tree(&src_dir, &dest.join("src"), &|p| {
        // Skip the binary entrypoint; the lib is what we build.
        p.file_name().is_some_and(|n| n == "main.rs")
    })?;

    let locales = src.join("locales");
    if locales.is_dir() {
        copy_tree(&locales, &dest.join("locales"), &|_| false)?;
    }
    for aux in ["LICENSE", "LICENSE-MIT", "COPYING"] {
        let from = src.join(aux);
        if from.is_file() {
            std::fs::create_dir_all(dest)?;
            std::fs::copy(&from, dest.join(aux))?;
        }
    }
    Ok(())
}

/// Recursively copies `.rs` and asset files, applying a skip predicate.
fn copy_tree(from: &Path, to: &Path, skip: &dyn Fn(&Path) -> bool) -> Result<()> {
    std::fs::create_dir_all(to).with_context(|| format!("creating {}", to.display()))?;
    for entry in std::fs::read_dir(from).with_context(|| format!("reading {}", from.display()))? {
        let entry = entry?;
        let path = entry.path();
        if skip(&path) {
            continue;
        }
        let target = to.join(entry.file_name());
        if path.is_dir() {
            copy_tree(&path, &target, skip)?;
        } else {
            std::fs::copy(&path, &target)
                .with_context(|| format!("copying {}", path.display()))?;
        }
    }
    Ok(())
}

/// Generates the fork manifest from the registry manifest: drops the binary,
/// benches, tests, dev-dependencies and upstream lint config, and adds the
/// `brush-vfs` dependency.
fn write_manifest(src: &Path, dest: &Path, name: &str) -> Result<()> {
    let original = std::fs::read_to_string(src.join("Cargo.toml"))
        .with_context(|| format!("reading {}/Cargo.toml", src.display()))?;
    let mut manifest: toml::Table = original.parse().context("parsing upstream Cargo.toml")?;

    // A library fork: no binary, no benches, no upstream test/bench deps, and no
    // upstream lint table (the fork is excluded from the workspace and its code
    // is not held to brush's lints -- see the root `[workspace] exclude`).
    for key in ["bin", "bench", "example", "test", "dev-dependencies", "lints", "badges"] {
        manifest.remove(key);
    }

    if let Some(toml::Value::Table(package)) = manifest.get_mut("package") {
        // The README is not vendored, so a reference to it would fail to package.
        package.remove("readme");
        package.insert("autobins".into(), toml::Value::Boolean(false));
        package.insert("autobenches".into(), toml::Value::Boolean(false));
        package.insert("autoexamples".into(), toml::Value::Boolean(false));
        package.insert("autotests".into(), toml::Value::Boolean(false));
    }

    // Add the facade the codemod's output depends on.
    let mut vfs_dep = toml::Table::new();
    vfs_dep.insert("path".into(), toml::Value::String(BRUSH_VFS_PATH.into()));
    if let Some(toml::Value::Table(deps)) = manifest.get_mut("dependencies") {
        deps.insert("brush-vfs".into(), toml::Value::Table(vfs_dep));
    } else {
        let mut deps = toml::Table::new();
        deps.insert("brush-vfs".into(), toml::Value::Table(vfs_dep));
        manifest.insert("dependencies".into(), toml::Value::Table(deps));
    }

    let header = format!(
        "# Fork of {name}, vendored from crates.io by `cargo xtask vendor-fork`.\n\
         #\n\
         # Generated, not maintained (D13): pristine upstream is the baseline and the\n\
         # filesystem-routing diff is applied by the codemod. Not a workspace member\n\
         # (see the root `[workspace] exclude`); its routing is proven by the Landlock\n\
         # test (D41), not the ban (D3).\n\n"
    );
    let body = toml::to_string_pretty(&manifest).context("serializing fork Cargo.toml")?;
    std::fs::write(dest.join("Cargo.toml"), format!("{header}{body}"))
        .with_context(|| format!("writing {}/Cargo.toml", dest.display()))?;
    Ok(())
}

/// Writes a `.gitignore` so the fork's standalone `Cargo.lock` is not committed.
fn ensure_lock_ignored(dest: &Path) -> Result<()> {
    std::fs::write(dest.join(".gitignore"), "Cargo.lock\n")
        .with_context(|| format!("writing {}/.gitignore", dest.display()))?;
    Ok(())
}
