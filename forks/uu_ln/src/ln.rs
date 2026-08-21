// This file is part of the uutils coreutils package.
//
// For the full copyright and license information, please view the LICENSE
// file that was distributed with this source code.

// spell-checker:ignore (ToDO) srcpath targetpath EEXIST


// FLATLAND DIVERGENCE: identity vfs session for this crate's own tests.
#[cfg(test)]
mod flatland_test_session;
use clap::{Arg, ArgAction, Command};
use std::io::{self, Write, stdout};
use uucore::display::Quotable;
use uucore::error::{UError, UIoError, UResult};

use uucore::fs::{make_path_relative_to, paths_refer_to_same_file};
use uucore::translate;
use uucore::{format_usage, prompt_yes, show_error};

use std::borrow::Cow;
use std::collections::HashSet;
use std::ffi::OsString;
use std::fs;
use thiserror::Error;

// FLATLAND DIVERGENCE: routed. `std::os::unix::fs::symlink` writes to the
// *host*, and the link name is resolved against the host process's working
// directory -- so `cd /work && ln -s f.txt newlink` under a mount exited 0 and
// created `newlink` outside the namespace entirely. The facade takes the same
// two arguments in the same order, so the call site below is unchanged, and it
// validates that the stored target stays inside the mount, which the raw
// syscall cannot.
#[cfg(any(unix, target_os = "redox"))]
use brush_vfs::ambient::symlink;
#[cfg(windows)]
use std::os::windows::fs::{symlink_dir, symlink_file};
use std::path::{Path, PathBuf};
use uucore::backup_control::{self, BackupMode};
use uucore::fs::{MissingHandling, ResolveMode, canonicalize};

/// Public visibility allows other apps to integrate with our
/// `ln` utility by calling `exec` directly with their `Settings`.
pub struct Settings {
    pub overwrite: OverwriteMode,
    pub backup: BackupMode,
    pub suffix: OsString,
    pub symbolic: bool,
    pub relative: bool,
    pub logical: bool,
    pub target_dir: Option<PathBuf>,
    pub no_target_dir: bool,
    pub no_dereference: bool,
    pub verbose: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OverwriteMode {
    NoClobber,
    Interactive,
    Force,
}

#[derive(Error, Debug)]
pub enum LnError {
    #[error("{}", translate!("ln-error-target-is-not-directory", "target" => _0.quote()))]
    TargetIsNotADirectory(PathBuf),

    #[error("{0}")]
    Io(#[from] UIoError),

    #[error("{1}: {0}")]
    IoContext(UIoError, String),

    #[error("")]
    SomeLinksFailed,

    #[error("{}", translate!("ln-error-same-file", "file1" => _0.quote(), "file2" => _1.quote()))]
    SameFile(PathBuf, PathBuf),

    #[error("{}", translate!("ln-error-missing-destination", "operand" => _0.quote()))]
    MissingDestination(PathBuf),

    #[error("{}", translate!("ln-error-extra-operand", "operand" => _0.quote(), "program" => _1.clone()))]
    ExtraOperand(OsString, String),

    #[error("{}", translate!("ln-failed-to-create-hard-link-dir", "source" => _0.to_string_lossy()))]
    FailedToCreateHardLinkDir(PathBuf),
}

impl UError for LnError {
    fn code(&self) -> i32 {
        1
    }
}
pub type LnResult<T> = Result<T, LnError>;

impl From<io::Error> for LnError {
    fn from(err: io::Error) -> Self {
        Self::Io(UIoError::from(err))
    }
}

mod options {
    pub const FORCE: &str = "force";
    //pub const DIRECTORY: &str = "directory";
    pub const INTERACTIVE: &str = "interactive";
    pub const NO_DEREFERENCE: &str = "no-dereference";
    pub const SYMBOLIC: &str = "symbolic";
    pub const LOGICAL: &str = "logical";
    pub const PHYSICAL: &str = "physical";
    pub const TARGET_DIRECTORY: &str = "target-directory";
    pub const NO_TARGET_DIRECTORY: &str = "no-target-directory";
    pub const RELATIVE: &str = "relative";
    pub const VERBOSE: &str = "verbose";
}

static ARG_FILES: &str = "files";

#[uucore::main]
pub fn uumain(args: impl uucore::Args) -> UResult<()> {
    let matches = uucore::clap_localization::handle_clap_result(uu_app(), args)?;

    /* the list of files */

    let paths: Vec<PathBuf> = matches
        .get_many::<OsString>(ARG_FILES)
        .unwrap()
        .map(PathBuf::from)
        .collect();

    let symbolic = matches.get_flag(options::SYMBOLIC);

    let overwrite_mode = if matches.get_flag(options::FORCE) {
        OverwriteMode::Force
    } else if matches.get_flag(options::INTERACTIVE) {
        OverwriteMode::Interactive
    } else {
        OverwriteMode::NoClobber
    };

    let backup_mode =
        backup_control::determine_backup_mode(std::env::var("VERSION_CONTROL").ok(), &matches)?;
    let backup_suffix = backup_control::determine_backup_suffix(&matches);

    // When we have "-L" or "-L -P", false otherwise
    let logical = matches.get_flag(options::LOGICAL);

    let settings = Settings {
        overwrite: overwrite_mode,
        backup: backup_mode,
        suffix: OsString::from(backup_suffix),
        symbolic,
        logical,
        relative: matches.get_flag(options::RELATIVE),
        target_dir: matches
            .get_one::<OsString>(options::TARGET_DIRECTORY)
            .map(PathBuf::from),
        no_target_dir: matches.get_flag(options::NO_TARGET_DIRECTORY),
        no_dereference: matches.get_flag(options::NO_DEREFERENCE),
        verbose: matches.get_flag(options::VERBOSE),
    };

    exec(&paths[..], &settings)?;
    Ok(())
}

pub fn uu_app() -> Command {
    let after_help = format!(
        "{}\n\n{}",
        translate!("ln-after-help"),
        backup_control::BACKUP_CONTROL_LONG_HELP
    );

    Command::new("ln")
        .version(uucore::crate_version!())
        .help_template(uucore::localized_help_template("ln"))
        .about(translate!("ln-about"))
        .override_usage(format_usage(&translate!("ln-usage")))
        .infer_long_args(true)
        .after_help(after_help)
        .arg(backup_control::arguments::backup())
        .arg(backup_control::arguments::backup_no_args())
        /*.arg(
            Arg::new(options::DIRECTORY)
                .short('d')
                .long(options::DIRECTORY)
                .help("allow users with appropriate privileges to attempt to make hard links to directories")
        )*/
        .arg(
            Arg::new(options::FORCE)
                .short('f')
                .long(options::FORCE)
                .help(translate!("ln-help-force"))
                .overrides_with(options::INTERACTIVE)
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new(options::INTERACTIVE)
                .short('i')
                .long(options::INTERACTIVE)
                .help(translate!("ln-help-interactive"))
                .overrides_with(options::FORCE)
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new(options::NO_DEREFERENCE)
                .short('n')
                .long(options::NO_DEREFERENCE)
                .help(translate!("ln-help-no-dereference"))
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new(options::LOGICAL)
                .short('L')
                .long(options::LOGICAL)
                .help(translate!("ln-help-logical"))
                .overrides_with(options::PHYSICAL)
                .action(ArgAction::SetTrue),
        )
        .arg(
            // Not implemented yet
            Arg::new(options::PHYSICAL)
                .short('P')
                .long(options::PHYSICAL)
                .help(translate!("ln-help-physical"))
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new(options::SYMBOLIC)
                .short('s')
                .long(options::SYMBOLIC)
                .help(translate!("ln-help-symbolic"))
                // override added for https://github.com/uutils/coreutils/issues/2359
                .overrides_with(options::SYMBOLIC)
                .action(ArgAction::SetTrue),
        )
        .arg(backup_control::arguments::suffix())
        .arg(
            Arg::new(options::TARGET_DIRECTORY)
                .short('t')
                .long(options::TARGET_DIRECTORY)
                .help(translate!("ln-help-target-directory"))
                .value_name("DIRECTORY")
                .value_hint(clap::ValueHint::DirPath)
                .value_parser(clap::value_parser!(OsString))
                .conflicts_with(options::NO_TARGET_DIRECTORY),
        )
        .arg(
            Arg::new(options::NO_TARGET_DIRECTORY)
                .short('T')
                .long(options::NO_TARGET_DIRECTORY)
                .help(translate!("ln-help-no-target-directory"))
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new(options::RELATIVE)
                .short('r')
                .long(options::RELATIVE)
                .help(translate!("ln-help-relative"))
                .requires(options::SYMBOLIC)
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new(options::VERBOSE)
                .short('v')
                .long(options::VERBOSE)
                .help(translate!("ln-help-verbose"))
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new(ARG_FILES)
                .action(ArgAction::Append)
                .value_hint(clap::ValueHint::AnyPath)
                .value_parser(clap::value_parser!(OsString))
                .required(true)
                .num_args(1..),
        )
}

/// Executes the `ln` utility with the given paths and settings.
///
/// This is made public to allow other apps to use `ln` as a library.
pub fn exec(files: &[PathBuf], settings: &Settings) -> LnResult<()> {
    // Handle cases where we create links in a directory first.
    if let Some(ref target_path) = settings.target_dir {
        // 4th form: a directory is specified by -t.
        return link_files_in_dir(files, target_path, settings);
    }
    if !settings.no_target_dir {
        if files.len() == 1 {
            // 2nd form: the target directory is the current directory.
            return link_files_in_dir(files, &PathBuf::from("."), settings);
        }
        let last_file = &PathBuf::from(files.last().unwrap());
        // FLATLAND DIVERGENCE: routed, as every `is_dir`/`is_symlink` below.
        // These are inherent `Path` methods, outside what the codemod can see
        // (D34 bounds it to free functions), so they asked the host -- where
        // every virtual path answers "no". That silently changed which *form*
        // of the command was taken: `ln -s f.txt dir/` stopped meaning "link
        // into the directory".
        if files.len() > 2 || brush_vfs::ambient::is_dir(last_file) {
            // 3rd form: create links in the last argument.
            return link_files_in_dir(&files[0..files.len() - 1], last_file, settings);
        }
    }

    // 1st form. Now there should be only two operands, but if -T is
    // specified we may have a wrong number of operands.
    if files.len() == 1 {
        return Err(LnError::MissingDestination(files[0].clone()));
    }
    if files.len() > 2 {
        return Err(LnError::ExtraOperand(
            files[2].clone().into(),
            uucore::execution_phrase().to_string(),
        ));
    }
    assert!(!files.is_empty());

    link(&files[0], &files[1], settings)
}

#[allow(clippy::cognitive_complexity)]
fn link_files_in_dir(files: &[PathBuf], target_dir: &Path, settings: &Settings) -> LnResult<()> {
    if !brush_vfs::ambient::is_dir(target_dir) {
        return Err(LnError::TargetIsNotADirectory(target_dir.to_owned()));
    }
    // remember the linked destinations for further usage
    let mut linked_destinations: HashSet<PathBuf> = HashSet::with_capacity(files.len());

    let mut all_successful = true;
    for srcpath in files {
        let targetpath = if settings.no_dereference && brush_vfs::ambient::is_symlink(target_dir) {
            let remove_target = || {
                // Not sure why but on Windows, the symlink can be
                // considered as a dir
                // See test_ln::test_symlink_no_deref_dir
                #[cfg(windows)]
                if let Err(e) = brush_vfs::ambient::remove_dir(target_dir) {
                    show_error!(
                        "{}",
                        translate!("ln-error-could-not-update", "target" => target_dir.quote(), "error" => e)
                    );
                }
            };
            match settings.overwrite {
                OverwriteMode::NoClobber => {}
                OverwriteMode::Interactive => {
                    if prompt_yes!(
                        "{}",
                        translate!("ln-prompt-replace", "file" => target_dir.quote())
                    ) {
                        remove_target();
                    }
                }
                OverwriteMode::Force => {
                    remove_target();
                }
            }
            target_dir.to_path_buf()
        } else {
            match srcpath.file_name() {
                Some(basename) => target_dir.join(basename),
                // This can be None only for "." or "..". Trying
                // to create a link with such name will fail with
                // EEXIST, which agrees with the behavior of GNU
                // coreutils.
                None => target_dir.join(srcpath),
            }
        };

        if linked_destinations.contains(&targetpath) {
            // If the target file was already created in this ln call, do not overwrite
            show_error!(
                "{}",
                translate!("ln-error-will-not-overwrite", "target" => targetpath.quote(), "source" => srcpath.quote())
            );
            all_successful = false;
        } else if let Err(e) = link(srcpath, &targetpath, settings) {
            show_error!("{e}");
            all_successful = false;
        }

        linked_destinations.insert(targetpath.clone());
    }
    if all_successful {
        Ok(())
    } else {
        Err(LnError::SomeLinksFailed)
    }
}

fn relative_path<'a>(src: &'a Path, dst: &Path) -> Cow<'a, Path> {
    // `dst.parent()` is None for a destination with no parent (`/`, `""`, or a
    // bare Windows prefix). Fall through to the non-relative `src` rather than
    // unwrapping it; the caller then reports the usual error.
    let Some(dst_parent) = dst.parent() else {
        return src.into();
    };
    let (Ok(src_abs), Ok(dst_abs)) = (
        canonicalize(src, MissingHandling::Missing, ResolveMode::Physical),
        canonicalize(dst_parent, MissingHandling::Missing, ResolveMode::Physical),
    ) else {
        return src.into();
    };

    make_path_relative_to(src_abs, dst_abs).into()
}

/// Decide whether `src` and `dst` are actually the same directory entry.
fn is_same_entry(src: &Path, dst: &Path) -> bool {
    match (
        canonicalize(src, MissingHandling::Missing, ResolveMode::Physical),
        canonicalize(dst, MissingHandling::Missing, ResolveMode::Physical),
    ) {
        (Ok(src), Ok(dst)) => src == dst,
        _ => true,
    }
}

#[allow(clippy::cognitive_complexity)]
fn link(src: &Path, dst: &Path, settings: &Settings) -> LnResult<()> {
    let mut backup_path = None;
    let source: Cow<'_, Path> = if settings.relative {
        relative_path(src, dst)
    } else {
        src.into()
    };

    // FLATLAND DIVERGENCE: routed. Worth naming separately: with `exists`
    // routed and `is_symlink` not, `ln -sf` over an existing link in the mount
    // took the overwrite branch, removed the real link through the facade, and
    // then wrote the replacement to the host -- losing the file and escaping in
    // one step.
    if brush_vfs::ambient::is_symlink(dst) || brush_vfs::ambient::exists(&(dst)) {
        backup_path = backup_control::get_backup_path(settings.backup, dst, &settings.suffix);
        if settings.backup == BackupMode::Existing && !settings.symbolic {
            // when ln --backup f f, it should detect that it is the same file
            if paths_refer_to_same_file(src, dst, true) && is_same_entry(src, dst) {
                return Err(LnError::SameFile(src.to_owned(), dst.to_owned()));
            }
        }
        if let Some(ref p) = backup_path {
            brush_vfs::ambient::rename(dst, p).map_err(|e| {
                LnError::IoContext(
                    UIoError::from(e),
                    translate!("ln-cannot-backup", "file" => dst.quote()),
                )
            })?;
        }
        match settings.overwrite {
            OverwriteMode::NoClobber => {}
            OverwriteMode::Interactive => {
                if !prompt_yes!("{}", translate!("ln-prompt-replace", "file" => dst.quote())) {
                    return Err(LnError::SomeLinksFailed);
                }

                let _ = brush_vfs::ambient::remove_file(dst);
                // In case of error, don't do anything
            }
            OverwriteMode::Force => {
                if !brush_vfs::ambient::is_symlink(dst)
                    && paths_refer_to_same_file(src, dst, true)
                    && is_same_entry(src, dst)
                {
                    // Even in force overwrite mode, verify we are not targeting the same entry and return a SameFile error if so
                    return Err(LnError::SameFile(src.to_owned(), dst.to_owned()));
                }
                let _ = brush_vfs::ambient::remove_file(dst);
                // In case of error, don't do anything
            }
        }
    }

    let res = if settings.symbolic {
        symlink(&source, dst).map_err(|e| {
            LnError::IoContext(
                UIoError::from(e),
                translate!(
                    "ln-failed-to-create-symbolic-link",
                    "dest" => dst.quote()
                ),
            )
        })
    } else {
        let p = if settings.logical && brush_vfs::ambient::is_symlink(&source) {
            brush_vfs::ambient::canonicalize(&source).map_err(|e| {
                LnError::IoContext(
                    UIoError::from(e),
                    translate!("ln-failed-to-access", "file" => source.quote()),
                )
            })?
        } else {
            source.to_path_buf()
        };
        match brush_vfs::ambient::hard_link(&p, dst) {
            Ok(()) => Ok(()),
            Err(_) if brush_vfs::ambient::is_dir(&p) => {
                Err(LnError::FailedToCreateHardLinkDir(source.to_path_buf()))
            }
            Err(e) => Err(LnError::IoContext(
                UIoError::from(e),
                translate!(
                    "ln-failed-to-create-hard-link",
                    "source" => source.quote(),
                    "dest" => dst.quote()
                ),
            )),
        }
    };

    if let Err(e) = res {
        if let Some(ref p) = backup_path {
            brush_vfs::ambient::rename(p, dst).map_err(|e| {
                LnError::IoContext(
                    UIoError::from(e),
                    translate!("ln-cannot-backup", "file" => dst.quote()),
                )
            })?;
        }
        return Err(e);
    }

    if settings.verbose {
        let mut out = stdout();
        write!(out, "{} -> {}", dst.quote(), source.quote())?;
        match backup_path {
            Some(path) => writeln!(
                out,
                " ({})",
                translate!("ln-backup", "backup" => path.quote())
            )?,
            None => writeln!(out)?,
        }
    }
    Ok(())
}

#[cfg(windows)]
pub fn symlink<P1: AsRef<Path>, P2: AsRef<Path>>(src: P1, dst: P2) -> io::Result<()> {
    if src.as_ref().is_dir() {
        symlink_dir(src, dst)
    } else {
        symlink_file(src, dst)
    }
}

// FLATLAND DIVERGENCE: routed, as the Unix import above. `rustix::fs::symlink`
// is the same host write in a third spelling.
#[cfg(target_os = "wasi")]
pub fn symlink<P1: AsRef<Path>, P2: AsRef<Path>>(src: P1, dst: P2) -> io::Result<()> {
    brush_vfs::ambient::symlink(src, dst)
}
