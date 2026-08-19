//! The closed world (D2): a policy on external process execution.
//!
//! Routing brush's filesystem through the vfs confines what a *running* process
//! can name. It says nothing about *which* programs run — an ordinary
//! `cargo build` still reaches `std::process::Command` and runs with the host's
//! ambient authority. D2 closes that second gap: under a closed world the only
//! program that may be spawned is the launcher re-invoking itself to run one
//! bundled utility.
//!
//! This is a predicate, not a deletion of the exec path. The bundled coreutils
//! are themselves delivered by re-invoking the running executable
//! (`<launcher> --invoke-bundled <name> ...` — see `brush-shell`'s `bundled`
//! module), so sealing exec outright would take the shell's own `ls` with it.
//! The predicate is therefore "the running executable, and only when `argv[1]`
//! is the bundled-dispatch flag". **Both halves matter**: the path alone would
//! let `exec <launcher> -c '...'` start a fresh shell — one that begins under
//! the identity policy, with none of the confinement the closed world exists to
//! impose.
//!
//! What D2 does *not* do: confine the child. A bundled `ls` still runs in a
//! freshly launched process that inherits ambient authority, because nothing
//! here re-installs the namespace across the spawn. Carrying the session to the
//! child is D24's job (the broker); D2 only decides whether the spawn happens at
//! all.

use std::path::{Path, PathBuf};

/// The one program permitted to run under a closed world, together with the
/// argument that marks an invocation as bundled-command dispatch rather than
/// anything else.
#[derive(Debug, Clone)]
pub struct TrustedLauncher {
    /// Host path of the running executable. It lives *outside* the namespace —
    /// the launcher's own binary is not something a project tree mounts — so it
    /// is named here rather than resolved through the vfs, which would report it
    /// missing under any restrictive policy.
    path: PathBuf,
    /// The argument that must appear as `argv[1]`. Without it, `<launcher> -c
    /// ...` would re-enter the shell instead of dispatching a single utility,
    /// and the re-entered shell would start unconfined.
    dispatch_flag: String,
}

impl TrustedLauncher {
    /// Names the launcher binary and the dispatch flag that distinguishes a
    /// bundled-command re-invocation from any other use of it.
    #[must_use]
    pub fn new(path: impl Into<PathBuf>, dispatch_flag: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            dispatch_flag: dispatch_flag.into(),
        }
    }
}

/// Policy governing external process execution.
#[derive(Debug, Clone, Default)]
pub enum ExternalExecution {
    /// Refuse every external program. The fail-closed default a shell built
    /// without a policy — or reconstituted from disk, where the launcher path
    /// is not data that survives serialization — lands on. It matches the
    /// session's empty-namespace default: a shell with no policy grants nothing.
    #[default]
    Sealed,
    /// The identity world: any program the namespace can resolve may run. What a
    /// shell running the identity policy uses, so it behaves as an ordinary
    /// bash. This is a no-op predicate; the confinement, if any, is the
    /// namespace's.
    Open,
    /// The closed world (D2): only the trusted launcher's bundled dispatch runs.
    Bundled(TrustedLauncher),
}

/// How a permitted external spawn obtains the host program to run.
#[derive(Debug)]
pub(crate) enum ExecPermit {
    /// Resolve the program through the namespace, as an open world does. The
    /// caller translates the virtual name into a host path via the vfs.
    ViaNamespace,
    /// Run this exact host path. It is the trusted launcher, which the namespace
    /// does not contain, so the caller must *not* route it through the vfs.
    TrustedLauncher(PathBuf),
}

impl ExternalExecution {
    /// Decides whether an external program may be spawned, and if so how its
    /// host path is obtained. `None` is a refusal.
    ///
    /// `command_name` is the program as the caller named it; `argv1` is what
    /// will land in the child's `argv[1]` (the first element after the program,
    /// since `argv[0]` is set separately).
    pub(crate) fn permit(&self, command_name: &str, argv1: Option<&str>) -> Option<ExecPermit> {
        match self {
            Self::Open => Some(ExecPermit::ViaNamespace),
            Self::Sealed => None,
            Self::Bundled(launcher) => {
                let is_launcher = Path::new(command_name) == launcher.path;
                let is_dispatch = argv1 == Some(launcher.dispatch_flag.as_str());
                if is_launcher && is_dispatch {
                    Some(ExecPermit::TrustedLauncher(launcher.path.clone()))
                } else {
                    None
                }
            }
        }
    }

    /// Whether `command_name` is the trusted launcher's own path.
    ///
    /// Used only to keep the executable-lookup guard from rejecting the launcher
    /// for being outside the namespace. It deliberately does *not* check the
    /// dispatch flag: naming the launcher's path is not itself permission to run
    /// it — [`permit`](Self::permit) still enforces the flag before anything is
    /// spawned, so a `<launcher>` invocation without the flag reaches the
    /// refusal rather than being turned away early as "not found".
    pub(crate) fn names_launcher(&self, command_name: &str) -> bool {
        matches!(self, Self::Bundled(l) if Path::new(command_name) == l.path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FLAG: &str = "--invoke-bundled";
    const LAUNCHER: &str = "/opt/brush/bin/brush";

    fn bundled() -> ExternalExecution {
        ExternalExecution::Bundled(TrustedLauncher::new(LAUNCHER, FLAG))
    }

    #[test]
    fn open_world_permits_anything_via_the_namespace() {
        let p = ExternalExecution::Open;
        assert!(matches!(
            p.permit("/bin/cargo", Some("build")),
            Some(ExecPermit::ViaNamespace)
        ));
        assert!(matches!(
            p.permit("ls", None),
            Some(ExecPermit::ViaNamespace)
        ));
    }

    #[test]
    fn a_sealed_world_refuses_everything() {
        let p = ExternalExecution::Sealed;
        assert!(p.permit("/bin/ls", None).is_none());
        assert!(p.permit(LAUNCHER, Some(FLAG)).is_none());
    }

    #[test]
    fn the_closed_world_runs_the_launchers_dispatch_by_its_host_path() {
        let permit = bundled().permit(LAUNCHER, Some(FLAG));
        assert!(
            matches!(permit, Some(ExecPermit::TrustedLauncher(ref host)) if host == Path::new(LAUNCHER)),
            "the bundled dispatch must be permitted, and by the launcher's host path: {permit:?}"
        );
    }

    #[test]
    fn the_launcher_without_the_dispatch_flag_is_refused() {
        // This is the escape the two-part predicate exists to close: naming the
        // launcher and asking it to be a fresh shell rather than a single
        // utility.
        assert!(bundled().permit(LAUNCHER, Some("-c")).is_none());
        assert!(bundled().permit(LAUNCHER, None).is_none());
    }

    #[test]
    fn a_different_program_is_refused_even_with_the_flag() {
        assert!(bundled().permit("/bin/sh", Some(FLAG)).is_none());
    }

    #[test]
    fn names_launcher_ignores_the_flag_but_not_the_path() {
        let p = bundled();
        assert!(p.names_launcher(LAUNCHER));
        assert!(!p.names_launcher("/bin/sh"));
        // Sealed and open worlds name no launcher.
        assert!(!ExternalExecution::Sealed.names_launcher(LAUNCHER));
        assert!(!ExternalExecution::Open.names_launcher(LAUNCHER));
    }
}
