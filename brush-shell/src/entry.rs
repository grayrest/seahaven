//! Implements the command-line interface for the `brush` shell.

use crate::args::CommandLineArgs;
use crate::args::InputBackendType;
use crate::brushctl::ShellBuilderBrushBuiltinExt as _;
use crate::bundled;
use crate::config;
use crate::error_formatter;
use crate::events;
use crate::productinfo;
use brush_builtins::ShellBuilderExt as _;
#[cfg(feature = "experimental-builtins")]
use brush_experimental_builtins::ShellBuilderExt as _;
use clap::CommandFactory;
use std::sync::LazyLock;
use std::{path::Path, sync::Arc};
use tokio::sync::Mutex;

#[allow(unused_imports, reason = "only used in some configs")]
use std::io::IsTerminal;

static TRACE_EVENT_CONFIG: LazyLock<Arc<tokio::sync::Mutex<Option<events::TraceEventConfig>>>> =
    LazyLock::new(|| Arc::new(tokio::sync::Mutex::new(None)));

type BrushShellExtensions = brush_core::extensions::ShellExtensionsImpl<error_formatter::Formatter>;
type BrushShell = brush_core::Shell<BrushShellExtensions>;

// WARN: this implementation shadows `clap::Parser::parse_from` one so it must be defined
// after the `use clap::Parser`
impl CommandLineArgs {
    // Work around clap's limitation handling `--` like a regular value
    // TODO(cmdline): We can safely remove this `impl` after the issue is resolved
    // https://github.com/clap-rs/clap/issues/5055
    // This function takes precedence over [`clap::Parser::parse_from`]
    fn try_parse_from(itr: impl IntoIterator<Item = String>) -> Result<Self, clap::Error> {
        let mut args: Vec<String> = itr.into_iter().collect();

        // In bash, `-c` treats `--` as an option terminator and takes its
        // command string from the first argument *after* `--`. (Other
        // value-taking flags like `-o` and `-O` instead consume `--` as their
        // literal value in bash, rejecting it as an invalid option name.)
        //
        // Remove the `--` so that `-c` naturally consumes the next token as its
        // value via clap. Other value-taking flags are unaffected: for them
        // try_parse_known splits at `--` before clap sees it, so they still
        // produce an error for invocations like `-o --`/`-O --` (via a missing
        // value rather than an invalid option name). In both cases, we
        // intentionally do not treat `--` as an option terminator for those
        // flags.
        if let Some(dd_idx) = args.iter().position(|a| a == "--") {
            if let Some(flag_idx) = dd_idx
                .checked_sub(1)
                .filter(|&i| Self::has_pending_c_flag(&args[i]))
            {
                // Remove the option-terminating `--`.
                args.remove(dd_idx);

                // If the command value (now at dd_idx) is itself `--`, merge it
                // into the flag as an attached value (e.g., "-c" + "--" → "-c--").
                // Clap parses `-c--` as `-c` with value `"--"` (standard POSIX
                // short-option-with-attached-value syntax). This prevents
                // try_parse_known from splitting at it again.
                if args.get(dd_idx).map(String::as_str) == Some("--") {
                    let value = args.remove(dd_idx);
                    args[flag_idx].push_str(&value);
                }
            }
        }

        let (mut this, script_args) = brush_core::builtins::try_parse_known::<Self>(args)?;

        // Collect any args from after `--` (handled by try_parse_known) into
        // script_args, which become positional parameters ($0, $1, ...).
        if let Some(args) = script_args {
            this.script_args.extend(args);
        }

        Ok(this)
    }

    /// Returns true if `arg` is `-c` or a combined short-flag group ending in
    /// `c` (like `-ec`) where all preceding characters are boolean flags.
    ///
    /// This specifically targets `-c` because it is the only short flag with
    /// special `--` option-terminator behavior in bash. Other value-taking flags
    /// (`-o`, `-O`) consume `--` as their literal value instead.
    ///
    /// Uses clap's argument definitions to validate preceding flags, avoiding
    /// a hardcoded list of boolean flag characters.
    fn has_pending_c_flag(arg: &str) -> bool {
        // Must be a short flag group ending in 'c': "-c", "-ec", "-xec", etc.
        let Some(flags) = arg.strip_prefix('-') else {
            return false;
        };
        let Some(preceding) = flags.strip_suffix('c') else {
            return false;
        };
        // Reject long-option-like args (e.g., "--c").
        if preceding.starts_with('-') {
            return false;
        }

        // For "-c" alone, preceding is empty and the check below is vacuously
        // true. For combined flags like `-ec`, verify all chars before the
        // trailing `c` are boolean flags. If any preceding char takes a value
        // (like `o`), then `c` is consumed as that flag's value, not as `-c`.
        let cmd = Self::command();
        preceding.chars().all(|ch| {
            cmd.get_arguments().any(|a| {
                a.get_short() == Some(ch)
                    && !matches!(
                        a.get_action(),
                        clap::ArgAction::Set | clap::ArgAction::Append
                    )
            })
        })
    }
}

/// Main entry point for the `brush` shell.
pub fn run() {
    //
    // Install the bundled-command registry so it's available both for
    // bundled dispatch (handled next) and for builtin shim registration
    // during shell construction. With no bundled-providing features enabled
    // the registry is empty and both code paths become no-ops.
    //
    bundled::install_default_providers();

    //
    // If we were invoked as `brush <DISPATCH_FLAG> <name> [args...]`, run the
    // bundled command and exit before doing any shell setup. This is the
    // hot path for in-binary coreutils invocations.
    //
    if let Some(code) = bundled::maybe_dispatch() {
        std::process::exit(code);
    }

    //
    // Install panic handlers to clean up on panic.
    //
    install_panic_handlers();

    //
    // Parse args.
    //
    let mut args: Vec<_> = std::env::args().collect();

    // Work around clap's limitations handling +O options.
    for arg in &mut args {
        if arg.starts_with("+O") {
            arg.insert_str(0, "--");
        }
    }

    let parsed_args = match CommandLineArgs::try_parse_from(args.iter().cloned()) {
        Ok(parsed_args) => parsed_args,
        Err(e) => {
            let _ = e.print();

            // Check for whether this is something we'd truly consider fatal. clap returns
            // errors for `--help`, `--version`, etc.
            let exit_code = match e.kind() {
                clap::error::ErrorKind::DisplayVersion => 0,
                clap::error::ErrorKind::DisplayHelp => 0,
                _ => 2,
            };

            std::process::exit(exit_code);
        }
    };

    //
    // Run.
    //
    #[cfg(any(unix, windows))]
    let mut builder = tokio::runtime::Builder::new_multi_thread();
    #[cfg(not(any(unix, windows)))]
    let mut builder = tokio::runtime::Builder::new_current_thread();

    let Ok(runtime) = builder.enable_all().build() else {
        tracing::error!("error: failed to create Tokio runtime");
        std::process::exit(1);
    };

    let result = runtime.block_on(run_async(&args, parsed_args));

    let exit_code = match result {
        Ok(code) => code,
        Err(err) => {
            tracing::error!("error: {err:#}");
            1
        }
    };

    std::process::exit(i32::from(exit_code));
}

/// Installs panic handlers to report our panic and cleanly exit on panic.
fn install_panic_handlers() {
    //
    // Set up panic handler. On release builds, it will capture panic details to a
    // temporary .toml file and report a human-readable message to the screen.
    //
    human_panic::setup_panic!(
        human_panic::Metadata::new(productinfo::PRODUCT_NAME, productinfo::PRODUCT_VERSION)
            .homepage(env!("CARGO_PKG_HOMEPAGE"))
            .support("please post a GitHub issue at https://github.com/reubeno/brush/issues/new")
    );

    //
    // If stdout is connected to a terminal, then register a new panic handler that
    // resets the terminal and then invokes the previously registered handler. In
    // dev/debug builds, the previously registered handler will be the default
    // handler; in release builds, it will be the one registered by `human_panic`.
    //
    if std::io::stdout().is_terminal() {
        let original_panic_handler = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |panic_info| {
            // Best-effort attempt to reset the terminal to defaults.
            let _ = try_reset_terminal_to_defaults();

            // Invoke the original handler
            original_panic_handler(panic_info);
        }));
    }
}

#[cfg(feature = "experimental")]
pub(crate) const DEFAULT_ENABLE_HIGHLIGHTING: bool = true;
#[cfg(not(feature = "experimental"))]
pub(crate) const DEFAULT_ENABLE_HIGHLIGHTING: bool = false;

/// Run the brush shell. Returns the exit code.
///
/// # Arguments
///
/// * `cli_args` - The command-line arguments to the shell, in string form.
/// * `args` - The already-parsed command-line arguments.
#[doc(hidden)]
async fn run_async(
    cli_args: &[String],
    args: CommandLineArgs,
) -> Result<u8, brush_interactive::ShellError> {
    // Initializing tracing.
    let mut event_config = TRACE_EVENT_CONFIG.lock().await;
    *event_config = Some(events::TraceEventConfig::init(
        &args.enabled_debug_events,
        &args.disabled_events,
    ));
    drop(event_config);

    // Load configuration file.
    let file_config = config::load_config(args.no_config, args.config_file.as_deref())
        .into_config_or_log()
        .map_err(|e| brush_interactive::ShellError::IoError(std::io::Error::other(e)))?;

    // Instantiate an appropriately configured shell and wrap it in an `Arc`. Note that we do
    // *not* run any code in the shell yet. We'll delay loading profiles and such until after
    // we've set up everything else (in `run_in_shell`).
    let shell: BrushShell = instantiate_shell(&args, cli_args).await?;
    let shell = Arc::new(Mutex::new(shell));

    // Run with the selected input backend. Each branch instantiates the concrete
    // backend type and calls `run_in_shell`, preserving static dispatch.
    let default_backend = get_default_input_backend_type(&args);
    let selected_backend = args.input_backend.unwrap_or(default_backend);

    // Build UI options by merging config file with CLI args.
    #[allow(unused_variables, reason = "not used when no backend features enabled")]
    let ui_options = file_config.to_ui_options(&args);

    let result = match selected_backend {
        #[cfg(all(feature = "reedline", any(unix, windows)))]
        InputBackendType::Reedline => {
            let mut input_backend =
                brush_interactive::ReedlineInputBackend::new(&ui_options, &shell)?;
            run_in_shell(&shell, args, &mut input_backend, &ui_options).await
        }
        #[cfg(any(not(feature = "reedline"), not(any(unix, windows))))]
        InputBackendType::Reedline => Err(brush_interactive::ShellError::InputBackendNotSupported),

        #[cfg(feature = "basic")]
        InputBackendType::Basic => {
            let mut input_backend = brush_interactive::BasicInputBackend;
            run_in_shell(&shell, args, &mut input_backend, &ui_options).await
        }
        #[cfg(not(feature = "basic"))]
        InputBackendType::Basic => Err(brush_interactive::ShellError::InputBackendNotSupported),

        #[cfg(feature = "minimal")]
        InputBackendType::Minimal => {
            let mut input_backend = brush_interactive::MinimalInputBackend;
            run_in_shell(&shell, args, &mut input_backend, &ui_options).await
        }
        #[cfg(not(feature = "minimal"))]
        InputBackendType::Minimal => Err(brush_interactive::ShellError::InputBackendNotSupported),
    };

    // Display any error that percolated up.
    let exit_code = match result {
        Ok(code) => code,
        Err(brush_interactive::ShellError::ShellError(e)) => {
            let shell = shell.lock().await;
            let mut stderr = shell.stderr();
            let _ = shell.display_error(&mut stderr, &e);
            drop(shell);
            1
        }
        Err(err) => {
            tracing::error!("error: {err:#}");
            1
        }
    };

    Ok(exit_code)
}

/// Determines whether `run_in_shell` will run the shell interactively. Must be sync'd with it.
const fn will_run_interactively(args: &CommandLineArgs) -> bool {
    if args.command.is_some() {
        false
    } else if args.read_commands_from_stdin {
        true
    } else {
        args.script_args.is_empty()
    }
}

/// Runs the shell according to the provided command-line arguments.
/// Also responsible for loading profiles and rc files as appropriate.
///
/// # Arguments
///
/// * `shell_ref` - A reference to the shell to run.
/// * `args` - The parsed command-line arguments.
/// * `input_backend` - The input backend to use.
/// * `ui_options` - The user interface options to use.
async fn run_in_shell(
    shell_ref: &brush_interactive::ShellRef<impl brush_core::ShellExtensions>,
    args: CommandLineArgs,
    input_backend: &mut impl brush_interactive::InputBackend,
    ui_options: &brush_interactive::UIOptions,
) -> Result<u8, brush_interactive::ShellError> {
    // First load profile and rc files as appropriate.
    initialize_shell(shell_ref, &args).await?;

    // If a command was specified via -c, then run that command and then exit.
    if let Some(command) = args.command {
        shell_ref.lock().await.run_dash_c_command(command).await?;

    // If -s was provided, then read commands from stdin. If there was a script (and optionally
    // args) passed on the command line via positional arguments, then we copy over the
    // parameters but do *not* execute it.
    } else if args.read_commands_from_stdin {
        let interactive_options = ui_options.into();
        brush_interactive::InteractiveShell::new(shell_ref, input_backend, &interactive_options)?
            .run_interactively()
            .await?;

    // If a script path was provided, then run the script.
    } else if !args.script_args.is_empty() {
        // The path to a script was provided on the command line; run the script.
        shell_ref
            .lock()
            .await
            .run_script(
                Path::new(&args.script_args[0]),
                args.script_args.iter().skip(1),
            )
            .await?;

    // If we got down here, then we don't have any commands to run. We'll be reading
    // them in from stdin one way or the other.
    } else {
        let interactive_options = ui_options.into();
        brush_interactive::InteractiveShell::new(shell_ref, input_backend, &interactive_options)?
            .run_interactively()
            .await?;
    }

    // Make sure to return the last result observed in the shell.
    let result = shell_ref.lock().await.last_exit_status();

    Ok(result)
}

/// Initializes a shell by loading profile and rc files as appropriate.
///
/// # Arguments
///
/// * `shell_ref` - A reference to the shell to initialize.
/// * `args` - The parsed command-line arguments.
async fn initialize_shell(
    shell_ref: &brush_interactive::ShellRef<impl brush_core::ShellExtensions>,
    args: &CommandLineArgs,
) -> Result<(), brush_interactive::ShellError> {
    // Install the sandbox namespace before anything runs. Doing it here rather
    // than after config loading matters: profile and rc scripts are the first
    // filesystem access a shell performs, and they must be subject to the same
    // policy as everything after them.
    if args.project.is_some() && !args.mounts.is_empty() {
        return Err(brush_interactive::ShellError::from(std::io::Error::other(
            "--project derives a namespace and --mount names one; pick one",
        )));
    }

    let derived = match &args.project {
        Some(dir) => Some(
            derive_project_namespace(dir, args)
                .map_err(|e| brush_interactive::ShellError::from(std::io::Error::other(e)))?,
        ),
        None => None,
    };

    let mounts = match derived {
        Some(mounts) => Some(mounts),
        None if args.mounts.is_empty() => None,
        None => Some(
            build_mount_table(&args.mounts)
                .map_err(|e| brush_interactive::ShellError::from(std::io::Error::other(e)))?,
        ),
    };

    if let Some(mounts) = mounts {
        let mut shell = shell_ref.lock().await;
        shell.set_mounts(mounts);
        strip_host_environment(&mut shell);
        drop(shell);
    }

    // Seal external execution independently of the namespace (D2). Still
    // separate axes -- `--mount` decides what a running process can name, this
    // decides what may be run -- but `--project` now turns both on, so the
    // question is no longer "was the flag passed". See `closed_world`.
    if closed_world(args) {
        let launcher = bundled::trusted_launcher().ok_or_else(|| {
            brush_interactive::ShellError::from(std::io::Error::other(
                "--closed-world: cannot determine this executable's path, so bundled \
                 commands could not run",
            ))
        })?;
        shell_ref
            .lock()
            .await
            .set_external_execution(brush_core::ExternalExecution::Bundled(launcher));
    }

    // Compute desired profile-loading behavior.
    let profile = if args.no_profile {
        brush_core::ProfileLoadBehavior::Skip
    } else {
        brush_core::ProfileLoadBehavior::LoadDefault
    };

    // Compute desired rc-loading behavior.
    //
    // Off by default under a policy (D31). `/home/user` is persistent and
    // writable, so a hostile repository that drops `~/.bashrc` once gets
    // execution on every later run -- which converts a host-scoped persistence
    // vector into a project-scoped one unless rc loading is off. `--rcfile`
    // still opts back in, because that is the user naming a file rather than
    // the project supplying one.
    let rc = if args.no_rc || (is_confined(args) && args.rc_file.is_none()) {
        brush_core::RcLoadBehavior::Skip
    } else if let Some(rc_file) = &args.rc_file {
        brush_core::RcLoadBehavior::LoadCustom(rc_file.clone())
    } else {
        brush_core::RcLoadBehavior::LoadDefault
    };

    shell_ref.lock().await.load_config(&profile, &rc).await?;

    Ok(())
}

/// Parses `--mount` specifications into a mount table.
///
/// The syntax is `VIRTUAL:HOST[:ro|:rw]`. An access suffix is stripped from the
/// right before the remainder is split, because a host path may legitimately
/// contain a colon while a virtual path may not -- the grammar forbids it.
fn build_mount_table(specs: &[String]) -> Result<brush_vfs::MountTable, String> {
    let mut builder = brush_vfs::MountTable::builder();

    for spec in specs {
        let (rest, access) = match spec {
            s if s.ends_with(":ro") => (s.trim_end_matches(":ro"), brush_vfs::Access::ReadOnly),
            s if s.ends_with(":rw") => (s.trim_end_matches(":rw"), brush_vfs::Access::ReadWrite),
            s => (s.as_str(), brush_vfs::Access::ReadWrite),
        };

        let (at, host) = rest
            .split_once(':')
            .ok_or_else(|| format!("--mount needs VIRTUAL:HOST[:ro|:rw], got '{spec}'"))?;

        builder = builder
            .mount(at, host, access)
            .map_err(|e| format!("--mount '{spec}': {e}"))?;
    }

    builder.build().map_err(|e| format!("--mount: {e}"))
}

/// Whether the command line asks for a namespace policy.
///
/// Read before the shell exists, so it cannot ask `Shell::is_confined`.
const fn is_confined(args: &CommandLineArgs) -> bool {
    args.project.is_some() || !args.mounts.is_empty()
}

/// Whether external execution is sealed (D2).
///
/// `--project` implies it and `--mount` does not, which looks inconsistent and
/// is the point: `--mount` is a caller composing the three axes deliberately,
/// and defaulting one of them would change a namespace they wrote out by hand.
/// `--project` derives the whole policy from a directory, so the same default
/// there is the difference between the safe spelling and the long one.
const fn closed_world(args: &CommandLineArgs) -> bool {
    !args.open_world && (args.closed_world || args.project.is_some())
}

/// Whether the builtin allowlist is in force (D11).
///
/// Implied by `--project` for the reason [`closed_world`] gives, and opted out
/// of separately from it: needing the host's compiler is not a reason to get
/// `enable` and unrestricted `kill` back.
const fn restrict_builtins(args: &CommandLineArgs) -> bool {
    !args.unrestricted_builtins && (args.restrict_builtins || args.project.is_some())
}

/// Discovers a project and derives the namespace it is granted (D44, D31, D29).
///
/// Four steps that are each somebody's decision: find the ceiling, check the
/// answer against it, derive the grant, and ask consent for anything not
/// already granted.
fn derive_project_namespace(
    dir: &Path,
    args: &CommandLineArgs,
) -> Result<brush_vfs::MountTable, String> {
    let bound = crate::discovery::Bound::platform_default();
    let ceiling = bound.permits(dir).map_err(|e| e.to_string())?;

    let grant = crate::grant::Grant::derive(&ceiling.dir, &ceiling).map_err(|e| e.to_string())?;
    let mounts = grant.mount_table().map_err(|e| e.to_string())?;

    let store_path =
        crate::grant::trust_store_path().ok_or("no state directory for the trust store")?;
    let mut store = crate::trust::TrustStore::load(&store_path);
    let request = crate::trust::granted_set(&mounts);

    match store.decide(&request) {
        crate::trust::Decision::Accept => {}
        crate::trust::Decision::Ask(excess) => {
            // D29: no approve-all flag exists, so a run with nowhere to ask
            // fails and names the flags that would grant it.
            if !args.is_interactive() {
                return Err(crate::trust::refusal(&excess));
            }
            if !prompt_for_grant(&excess)? {
                return Err("refused".to_owned());
            }
            store.record(request);
            store
                .save(&store_path)
                .map_err(|e| format!("cannot record consent: {e}"))?;
        }
    }

    Ok(mounts)
}

/// Asks the user, on the terminal the launcher still has.
///
/// The launcher runs before the sandbox, so it has a terminal where D36 says
/// the sandbox has none.
fn prompt_for_grant(excess: &[crate::trust::GrantedMount]) -> Result<bool, String> {
    use std::io::Write as _;

    let mut err = std::io::stderr();
    writeln!(err, "This project asks for access you have not granted:")
        .map_err(|e| e.to_string())?;
    for mount in excess {
        writeln!(err, "    {}", mount.as_flag()).map_err(|e| e.to_string())?;
    }
    write!(err, "Grant it? [y/N] ").map_err(|e| e.to_string())?;
    err.flush().map_err(|e| e.to_string())?;

    let mut answer = String::new();
    std::io::stdin()
        .read_line(&mut answer)
        .map_err(|e| e.to_string())?;
    Ok(matches!(answer.trim(), "y" | "Y" | "yes"))
}

/// Removes environment variables whose values name the host (D21).
///
/// D21 puts `SHELL` in the *synthesized from policy, never inherited* class,
/// and D6 lists it among the leaks against its own claim that host paths are
/// unnameable. It is inherited from the invoking process before any policy
/// exists, so the removal belongs here, where the policy is installed: this is
/// the launcher's act, not the shell's.
///
/// Only `SHELL` today. The rest of D21's synthesized class -- `PATH`, `HOME`,
/// `TMPDIR`, `HOSTNAME`, the locale variables, `TZ`, `UID` -- needs values to
/// synthesize *from*, which is D21's and D22's own work. Removing them here
/// without that would trade a leak for a shell that cannot find anything.
///
/// Nothing is synthesized in its place: `SHELL` should name this shell as the
/// namespace sees it, and until D22 builds `/bin` from the builtin registry
/// there is no such path. Unset is the truthful interim; a made-up value would
/// name something that does not resolve.
fn strip_host_environment(shell: &mut brush_core::Shell<impl brush_core::ShellExtensions>) {
    shell.env_mut().unset("SHELL").ok();
}

/// The builtin allowlist (D11) selected by the command line.
///
/// A third axis beside `--mount` and `--closed-world`. The bundled utilities
/// are folded in here rather than in the list itself because which of them
/// exist is a build-time question -- the registry is assembled from Cargo
/// features -- and a hard-coded copy of it would rot silently in the direction
/// that leaves a shell unable to run `cat`.
fn builtin_policy(args: &CommandLineArgs) -> brush_core::BuiltinPolicy {
    if !restrict_builtins(args) {
        return brush_core::BuiltinPolicy::Open;
    }
    let bundled: Vec<String> = bundled::registry()
        .map(|r| r.keys().cloned().collect())
        .unwrap_or_default();
    crate::builtinallowlist::policy(&bundled, &args.allowed_builtins, &args.denied_builtins)
}

/// Instantiates a shell from command-line arguments. Does *not* run any code in the shell.
///
/// # Arguments
///
/// * `args` - The parsed command-line arguments.
/// * `cli_args` - The raw command-line arguments.
async fn instantiate_shell(
    args: &CommandLineArgs,
    cli_args: &[String],
) -> Result<BrushShell, brush_interactive::ShellError> {
    #[cfg(feature = "experimental-load")]
    let mut shell = if let Some(load_file) = &args.load_file {
        instantiate_shell_from_file(load_file.as_path(), args)?
    } else {
        instantiate_shell_from_args(args, cli_args).await?
    };

    #[cfg(not(feature = "experimental-load"))]
    let mut shell = instantiate_shell_from_args(args, cli_args).await?;

    // Register shims for any bundled commands in the installed registry.
    // Done here (not inside the inner instantiators) so both paths are
    // covered from a single site.
    bundled::register_shims(&mut shell);

    Ok(shell)
}

#[cfg(feature = "experimental-load")]
fn instantiate_shell_from_file(
    file_path: &Path,
    args: &CommandLineArgs,
) -> Result<BrushShell, brush_interactive::ShellError> {
    // Before any shell exists, so there is no namespace to resolve in: this
    // reads the shell that is about to exist.
    #[expect(
        clippy::disallowed_methods,
        reason = "reconstitutes the shell itself; runs before there is a namespace"
    )]
    let file = std::fs::File::open(file_path)?;
    let mut shell: BrushShell = serde_json::from_reader(file)
        .map_err(|e| brush_interactive::ShellError::IoError(std::io::Error::other(e)))?;

    // Before any registration: a deserialized shell fails closed, so with no
    // policy installed the loop below would register nothing at all. This is
    // the route D11 names -- the registry does not survive serialization, so
    // whatever the saved shell was allowed is re-derived from the command line
    // rather than read back from disk.
    shell.set_builtin_policy(builtin_policy(args));

    // NOTE: We need to manually register builtins because we can't serialize/deserialize them.
    // TODO(serde): we should consider whether we could/should at least track *which* are enabled.
    let builtin_set = if shell.options().sh_mode {
        brush_builtins::BuiltinSet::ShMode
    } else {
        brush_builtins::BuiltinSet::BashMode
    };

    let builtins = brush_builtins::default_builtins(builtin_set);

    for (builtin_name, builtin) in builtins {
        shell.register_builtin(&builtin_name, builtin);
    }

    // Add experimental builtins (if enabled).
    #[cfg(feature = "experimental-builtins")]
    for (builtin_name, builtin) in brush_experimental_builtins::experimental_builtins() {
        shell.register_builtin(&builtin_name, builtin);
    }

    Ok(shell)
}

/// Instantiates a shell from command-line arguments. Does *not* run any code in the shell.
///
/// # Arguments
///
/// * `args` - The parsed command-line arguments.
/// * `cli_args` - The raw command-line arguments.
async fn instantiate_shell_from_args(
    args: &CommandLineArgs,
    cli_args: &[String],
) -> Result<BrushShell, brush_interactive::ShellError> {
    // Compute login flag.
    let login = args.login || cli_args.first().is_some_and(|argv0| argv0.starts_with('-'));

    // Compute shell name.
    let shell_name = if args.command.is_some() && !args.script_args.is_empty() {
        Some(args.script_args[0].clone())
    } else if !cli_args.is_empty() {
        Some(cli_args[0].clone())
    } else if args.sh_mode {
        // Simulate having been run as "sh".
        Some(String::from("sh"))
    } else {
        None
    };

    // Compute positional shell arguments.
    let shell_args = if args.command.is_some() {
        Some(args.script_args.iter().skip(1).cloned().collect())
    } else if args.read_commands_from_stdin {
        Some(args.script_args.clone())
    } else {
        None
    };

    // Commands are read from stdin if -s was provided, or if no command was specified (either via
    // -c or as a positional argument).
    let read_commands_from_stdin = (args.read_commands_from_stdin && args.command.is_none())
        || (args.script_args.is_empty() && args.command.is_none());

    let builtin_set = if args.sh_mode {
        brush_builtins::BuiltinSet::ShMode
    } else {
        brush_builtins::BuiltinSet::BashMode
    };

    // Identify the file descriptors to inherit.
    let fds = args
        .inherited_fds
        .iter()
        .filter_map(|&fd| brush_core::sys::fd::try_get_file_for_open_fd(fd).map(|file| (fd, file)))
        .collect();

    // Select parser implementation to use.
    #[cfg(feature = "experimental-parser")]
    let parser_impl = if args.experimental_parser {
        brush_core::parser::ParserImpl::Winnow
    } else {
        brush_core::parser::ParserImpl::Peg
    };

    #[cfg(not(feature = "experimental-parser"))]
    let parser_impl = brush_core::parser::ParserImpl::Peg;

    // Set up the shell builder with the requested options.
    // NOTE: We skip loading profile and rc files here; that will be handled later after we've
    // fully instantiated everything we want set before running any code.
    let shell = brush_core::Shell::builder_with_extensions::<BrushShellExtensions>()
        .disable_options(args.disabled_options.clone())
        .disable_shopt_options(args.disabled_shopt_options.clone())
        .disallow_overwriting_regular_files_via_output_redirection(
            args.disallow_overwriting_regular_files_via_output_redirection,
        )
        .enable_options(args.enabled_options.clone())
        .enable_shopt_options(args.enabled_shopt_options.clone())
        .do_not_execute_commands(args.do_not_execute_commands)
        .exit_after_one_command(args.exit_after_one_command)
        .login(login)
        .interactive(args.is_interactive())
        .command_string_mode(args.command.is_some())
        .no_editing(args.no_editing)
        .profile(brush_core::ProfileLoadBehavior::Skip)
        .rc(brush_core::RcLoadBehavior::Skip)
        .do_not_inherit_env(args.do_not_inherit_env)
        .fds(fds)
        .maybe_shell_args(shell_args)
        .posix(args.posix || args.sh_mode)
        .print_commands_and_arguments(args.print_commands_and_arguments)
        .read_commands_from_stdin(read_commands_from_stdin)
        .maybe_shell_name(shell_name)
        .shell_product_display_str(productinfo::get_product_display_str())
        .sh_mode(args.sh_mode)
        .treat_unset_variables_as_error(args.treat_unset_variables_as_error)
        .exit_on_nonzero_command_exit(args.exit_on_nonzero_command_exit)
        .disable_pathname_expansion(args.disable_pathname_expansion)
        .verbose(args.verbose)
        .parser(parser_impl)
        .builtin_policy(builtin_policy(args))
        .error_formatter(new_error_behavior(args))
        .shell_version(env!("CARGO_PKG_VERSION").to_string());

    // Add builtins.
    let shell = shell.default_builtins(builtin_set).brush_builtins();

    // Add experimental builtins (if enabled).
    #[cfg(feature = "experimental-builtins")]
    let shell = shell.experimental_builtins();

    // Build the shell.
    let mut shell = shell.build().await?;

    // Make adjustments.
    if let Some(xtrace_file_path) = &args.xtrace_file_path {
        enable_xtrace_to_file(&mut shell, xtrace_file_path)?;
    }

    Ok(shell)
}

fn enable_xtrace_to_file(
    shell: &mut brush_core::Shell<impl brush_core::ShellExtensions>,
    file_path: &Path,
) -> Result<(), brush_interactive::ShellError> {
    // Through the shell's namespace: a trace file is named by the user like
    // any other redirect target, and a confined shell must not be able to
    // write outside its mounts by asking for one.
    let file = shell
        .open_file(
            brush_core::vfs::OpenMode::default()
                .with_create(true)
                .with_write(true)
                .with_truncate(true),
            file_path,
            &shell.default_exec_params(),
        )
        .map_err(|e| {
            brush_interactive::ShellError::FailedToCreateXtraceFile(file_path.to_path_buf(), e)
        })?;

    let file_fd = shell.open_files_mut().add(file)?;

    shell.options_mut().print_commands_and_arguments = true;
    shell.set_env_global(
        "BASH_XTRACEFD",
        brush_core::ShellVariable::new(file_fd.to_string()),
    )?;

    Ok(())
}

const fn new_error_behavior(args: &CommandLineArgs) -> error_formatter::Formatter {
    error_formatter::Formatter {
        use_color: !args.disable_color,
    }
}

fn get_default_input_backend_type(args: &CommandLineArgs) -> InputBackendType {
    #[cfg(any(unix, windows))]
    {
        // If stdin isn't a terminal, then `reedline` doesn't do the right thing
        // (reference: https://github.com/nushell/reedline/issues/509). Switch to
        // the minimal input backend instead for that scenario.
        if std::io::stdin().is_terminal() && will_run_interactively(args) {
            InputBackendType::Reedline
        } else {
            InputBackendType::Minimal
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _args = args;
        InputBackendType::Minimal
    }
}

pub(crate) fn get_event_config() -> Arc<tokio::sync::Mutex<Option<events::TraceEventConfig>>> {
    TRACE_EVENT_CONFIG.clone()
}

fn try_reset_terminal_to_defaults() -> Result<(), std::io::Error> {
    #[cfg(any(unix, windows))]
    {
        // Reset the console.
        let exec_result = crossterm::execute!(
            std::io::stdout(),
            crossterm::terminal::LeaveAlternateScreen,
            crossterm::terminal::EnableLineWrap,
            crossterm::style::ResetColor,
            crossterm::event::DisableMouseCapture,
            crossterm::event::DisableBracketedPaste,
            crossterm::cursor::Show,
            crossterm::cursor::MoveToNextLine(1),
        );

        let raw_result = crossterm::terminal::disable_raw_mode();

        exec_result?;
        raw_result?;
    }

    Ok(())
}

#[cfg(test)]
#[allow(clippy::panic_in_result_fn)]
mod tests {
    use super::*;
    use anyhow::Result;
    use pretty_assertions::{assert_eq, assert_matches};

    fn args(strs: &[&str]) -> Vec<String> {
        strs.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parse_empty_args() -> Result<()> {
        let parsed_args = CommandLineArgs::try_parse_from(args(&["brush"]))?;
        assert_matches!(parsed_args.script_args.as_slice(), []);
        Ok(())
    }

    #[test]
    fn parse_script_and_args() -> Result<()> {
        let parsed_args = CommandLineArgs::try_parse_from(args(&[
            "brush",
            "some-script",
            "-x",
            "1",
            "--option",
        ]))?;
        assert_eq!(
            parsed_args.script_args,
            ["some-script", "-x", "1", "--option"]
        );
        Ok(())
    }

    #[test]
    fn parse_script_and_args_with_double_dash_in_script_args() -> Result<()> {
        let parsed_args = CommandLineArgs::try_parse_from(args(&["brush", "some-script", "--"]))?;
        assert_eq!(parsed_args.script_args, ["some-script", "--"]);
        Ok(())
    }

    #[test]
    fn parse_unknown_args() {
        let result = CommandLineArgs::try_parse_from(args(&["brush", "--unknown-option"]));
        assert!(result.is_err());
    }

    #[test]
    fn parse_c_with_double_dash_separator() -> Result<()> {
        let parsed_args =
            CommandLineArgs::try_parse_from(args(&["brush", "-c", "--", "echo hello", "arg0"]))?;
        assert_eq!(parsed_args.command, Some("echo hello".to_string()));
        assert_eq!(parsed_args.script_args, ["arg0"]);
        Ok(())
    }

    #[test]
    fn parse_c_with_double_dash_no_command() {
        assert!(CommandLineArgs::try_parse_from(args(&["brush", "-c", "--"])).is_err());
    }

    #[test]
    fn parse_c_with_double_dash_command_is_double_dash() -> Result<()> {
        let parsed_args =
            CommandLineArgs::try_parse_from(args(&["brush", "-c", "--", "--", "echo", "hi"]))?;
        assert_eq!(parsed_args.command, Some("--".to_string()));
        assert_eq!(parsed_args.script_args, ["echo", "hi"]);
        Ok(())
    }

    #[test]
    fn parse_ec_with_double_dash_separator() -> Result<()> {
        let parsed_args =
            CommandLineArgs::try_parse_from(args(&["brush", "-ec", "--", "echo hello", "arg0"]))?;
        assert_eq!(parsed_args.command, Some("echo hello".to_string()));
        assert!(parsed_args.exit_on_nonzero_command_exit);
        assert_eq!(parsed_args.script_args, ["arg0"]);
        Ok(())
    }

    #[test]
    fn parse_c_with_value_before_double_dash_unchanged() -> Result<()> {
        let parsed_args =
            CommandLineArgs::try_parse_from(args(&["brush", "-c", "echo hi", "--", "arg0"]))?;
        assert_eq!(parsed_args.command, Some("echo hi".to_string()));
        assert_eq!(parsed_args.script_args, ["--", "arg0"]);
        Ok(())
    }

    #[test]
    fn parse_o_with_double_dash_is_not_transformed() {
        // Unlike -c, bash's -o consumes -- as its literal value (invalid option
        // name), not as an option terminator. Verify we don't transform it.
        let result = CommandLineArgs::try_parse_from(args(&["brush", "-o", "--"]));
        // Here, try_parse_from / try_parse_known splits at --, so -o ends up
        // without a value and parsing correctly fails. The key assertion is
        // that we MUST NOT reinterpret -- as an option terminator for -o and
        // then take any later argument as its value.
        assert!(result.is_err());
    }

    #[test]
    fn parse_oc_not_treated_as_pending_c() -> Result<()> {
        // -oc means -o with value "c", not -o flag + -c flag. The --
        // should NOT be treated as an option terminator for -c.
        let parsed_args = CommandLineArgs::try_parse_from(args(&["brush", "-oc", "--", "echo"]))?;
        // -o consumed "c" as its value; -- split the rest; no -c command.
        assert!(parsed_args.command.is_none());
        assert_eq!(parsed_args.script_args, ["--", "echo"]);
        Ok(())
    }

    #[test]
    fn parse_bool_flag_before_double_dash_not_transformed() -> Result<()> {
        // -e is a boolean flag, not -c. The -- should NOT be removed;
        // everything from -- onward becomes positional (including -c).
        let parsed_args =
            CommandLineArgs::try_parse_from(args(&["brush", "-e", "--", "-c", "echo"]))?;
        assert!(parsed_args.command.is_none());
        assert!(parsed_args.exit_on_nonzero_command_exit);
        assert_eq!(parsed_args.script_args, ["--", "-c", "echo"]);
        Ok(())
    }

    #[test]
    fn parse_c_with_double_dash_and_later_double_dash() -> Result<()> {
        // After removing the first --, -c gets "echo". The second -- is
        // handled by try_parse_known and appears in script_args.
        let parsed_args =
            CommandLineArgs::try_parse_from(args(&["brush", "-c", "--", "echo", "--", "more"]))?;
        assert_eq!(parsed_args.command, Some("echo".to_string()));
        assert_eq!(parsed_args.script_args, ["--", "more"]);
        Ok(())
    }

    #[test]
    fn has_pending_c_flag_edge_cases() {
        // Direct tests for the detection function.
        assert!(CommandLineArgs::has_pending_c_flag("-c"));
        assert!(CommandLineArgs::has_pending_c_flag("-ec"));
        assert!(!CommandLineArgs::has_pending_c_flag("-C")); // uppercase, different flag
        assert!(!CommandLineArgs::has_pending_c_flag("-oc")); // -o takes a value
        assert!(!CommandLineArgs::has_pending_c_flag("--c")); // long-option-like
        assert!(!CommandLineArgs::has_pending_c_flag("-")); // bare dash
        assert!(!CommandLineArgs::has_pending_c_flag("c")); // no leading dash
        assert!(!CommandLineArgs::has_pending_c_flag("")); // empty
    }

    /// The three policy axes as `--project` and `--mount` resolve them.
    ///
    /// Table-driven rather than a case per row because the property is the
    /// *difference* between the two ways of naming a namespace, and a reader
    /// checking that difference wants both columns in one place.
    #[test]
    fn project_implies_the_other_two_axes_and_mount_does_not() -> Result<()> {
        // (argv, confined, closed world, restricted builtins)
        let rows: &[(&[&str], bool, bool, bool)] = &[
            (&["brush"], false, false, false),
            // `--mount` is a caller composing axes by hand. Defaulting either
            // of the other two would change a namespace they wrote out.
            (&["brush", "--mount", "/work:/tmp:rw"], true, false, false),
            (
                &["brush", "--mount", "/work:/tmp:rw", "--closed-world"],
                true,
                true,
                false,
            ),
            // `--project` derives the whole policy, so the safe spelling is the
            // short one.
            (&["brush", "--project", "."], true, true, true),
            (
                &["brush", "--project", ".", "--open-world"],
                true,
                false,
                true,
            ),
            (
                &["brush", "--project", ".", "--unrestricted-builtins"],
                true,
                true,
                false,
            ),
            (
                &[
                    "brush",
                    "--project",
                    ".",
                    "--open-world",
                    "--unrestricted-builtins",
                ],
                true,
                false,
                false,
            ),
            // The opt-outs are inert without the implication that produced
            // them, rather than being a way to weaken a policy that was asked
            // for outright.
            (&["brush", "--open-world"], false, false, false),
            (&["brush", "--unrestricted-builtins"], false, false, false),
        ];

        for (argv, confined, closed, restricted) in rows {
            let parsed = CommandLineArgs::try_parse_from(args(argv))?;
            assert_eq!(is_confined(&parsed), *confined, "confined: {argv:?}");
            assert_eq!(closed_world(&parsed), *closed, "closed world: {argv:?}");
            assert_eq!(
                restrict_builtins(&parsed),
                *restricted,
                "restricted builtins: {argv:?}"
            );
        }
        Ok(())
    }

    #[test]
    fn an_opt_out_cannot_contradict_the_flag_it_undoes() {
        // Asking for both is a mistake worth reporting rather than resolving by
        // precedence: whichever way it resolved, one of the two spellings on
        // the command line would be silently doing nothing.
        for argv in [
            ["brush", "--closed-world", "--open-world"],
            ["brush", "--restrict-builtins", "--unrestricted-builtins"],
        ] {
            assert!(
                CommandLineArgs::try_parse_from(args(&argv)).is_err(),
                "{argv:?} must be refused"
            );
        }

        // And the two opt-outs are independent, so asking for one does not
        // conflict with asking for the other axis outright.
        assert!(
            CommandLineArgs::try_parse_from(args(&[
                "brush",
                "--open-world",
                "--restrict-builtins"
            ]))
            .is_ok()
        );
    }
}
