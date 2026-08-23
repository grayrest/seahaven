//! Build the macOS platform sysroot the roc linker consumes.
//!
//! roc's legacy linker (`cli/linker.zig findPlatformSysroot`) scans
//! `platform/targets/macos-sysroot` and auto-adds `-framework X` for every
//! `X.framework` that has a TBD stub there -- this is roc's only way to link a
//! native framework, since `roc build` exposes no `-framework` flag. The
//! embedded broker host reaches CoreFoundation (`chrono -> iana-time-zone` on
//! macOS, via `reedline`), so that framework -- and the umbrella set around it
//! -- is declared here.
//!
//! The stubs are symlinks into the local SDK, so they are machine-local
//! (gitignored under `platform/targets/`); this command regenerates them. Rerun
//! after an Xcode/SDK update. Mirrors `roc-solid`'s `just sysroot` recipe.

use std::path::Path;

use anyhow::{Context, Result};
use xshell::{Shell, cmd};

/// The frameworks the platform host links. This list *is* the framework
/// dependency declaration: the linker adds `-framework X` for each one present,
/// so an unneeded entry is harmless and a missing one is an undefined symbol.
const FRAMEWORKS: &[&str] = &[
    "CoreFoundation",
    "Foundation",
    "Security",
    "CoreServices",
    "CFNetwork",
    "SystemConfiguration",
];

/// Builds `platform/targets/macos-sysroot` from the local SDK.
pub fn run(_verbose: bool) -> Result<()> {
    if !cfg!(target_os = "macos") {
        eprintln!("sysroot: only macOS needs a framework sysroot; nothing to do.");
        return Ok(());
    }

    let sh = Shell::new()?;
    let root = crate::common::find_workspace_root()?;
    let sdk = cmd!(sh, "xcrun --show-sdk-path")
        .read()
        .context("locating the macOS SDK with `xcrun --show-sdk-path`")?;
    let sdk = Path::new(sdk.trim());

    let sysroot = root.join("platform").join("targets").join("macos-sysroot");
    let frameworks_dir = sysroot.join("System/Library/Frameworks");

    std::fs::create_dir_all(sysroot.join("System/Library"))?;
    std::fs::create_dir_all(sysroot.join("usr"))?;
    // Rebuild the frameworks dir from scratch so a removed entry does not linger.
    let _ = std::fs::remove_dir_all(&frameworks_dir);
    std::fs::create_dir_all(&frameworks_dir)?;

    force_symlink(&sdk.join("usr/lib"), &sysroot.join("usr/lib"))?;
    force_symlink(
        &sdk.join("System/Library/PrivateFrameworks"),
        &sysroot.join("System/Library/PrivateFrameworks"),
    )?;

    for fw in FRAMEWORKS {
        let dir = frameworks_dir.join(format!("{fw}.framework"));
        std::fs::create_dir_all(&dir)?;
        let sdk_fw = sdk
            .join("System/Library/Frameworks")
            .join(format!("{fw}.framework"));
        // The TBD stub is what the linker reads; Versions carries its neighbours.
        force_symlink(
            &sdk_fw.join(format!("{fw}.tbd")),
            &dir.join(format!("{fw}.tbd")),
        )?;
        force_symlink(&sdk_fw.join("Versions"), &dir.join("Versions"))?;
    }

    eprintln!(
        "sysroot: {} framework stub(s) at {}",
        FRAMEWORKS.len(),
        sysroot.display()
    );
    Ok(())
}

/// Creates a symlink at `link` pointing to `target`, replacing any existing one.
fn force_symlink(target: &Path, link: &Path) -> Result<()> {
    let _ = std::fs::remove_file(link);
    let _ = std::fs::remove_dir_all(link);
    std::os::unix::fs::symlink(target, link)
        .with_context(|| format!("symlinking {} -> {}", link.display(), target.display()))
}
