//! The session facts the env effects report (D15, D21, D22, D30).
//!
//! These are not read from the machine. They are what a launcher *decides* a
//! confined session should believe about its world, which is the whole point of
//! confinement: the guest's environment, its notion of the platform, its
//! process id and its parallelism are policy, set once when the session is
//! built, not queried from the host at each call.

use std::collections::BTreeMap;

/// The operating system a session believes it runs on.
///
/// **Policy-declared, not the machine's.** The first draft of the plan read the
/// real host OS on the grounds that justfiles carry `[macos]`/`[linux]`
/// attributes — true, and not a reason to read the machine. D12 records that
/// cap-std path resolution is kernel-enforced on Linux and FreeBSD and
/// *userspace-emulated* on macOS and Windows, so the honest reading of a machine
/// OS is "tell the guest whether its confinement has TOCTOU windows". A launcher
/// that wants a justfile's `[macos]` recipe declares this target; it need not,
/// and should not, be where the host actually runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetOs {
    /// Linux.
    Linux,
    /// macOS.
    MacOs,
    /// Windows.
    Windows,
    /// Anything else, named.
    Other(String),
}

/// The architecture a session believes it runs on. Policy-declared; see
/// [`TargetOs`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetArch {
    /// 32-bit x86.
    X86,
    /// 64-bit x86.
    X64,
    /// 32-bit ARM.
    Arm,
    /// 64-bit ARM.
    Aarch64,
    /// Anything else, named.
    Other(String),
}

/// What `env_platform!` reports: a declared OS and architecture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlatformTarget {
    /// The declared operating system.
    pub os: TargetOs,
    /// The declared architecture.
    pub arch: TargetArch,
}

/// The virtual path `env_exe_path!` reports (D30).
///
/// A constant, because a closed world re-invokes `just` through a single known
/// name rather than the host's executable path. It **does not resolve yet**:
/// D22's `/bin` — synthesized from the builtin registry — is where a truthful
/// answer will come from, and D22 is not built. The plan names this a decision
/// to take rather than discover: the effect returns the name the closed world
/// will use, and the name is correct before the directory it names exists.
pub const EXE_PATH: &str = "/bin/just";

/// The facts a confined session reports through the env effects.
///
/// Assembled by the launcher and handed to [`VfsPlatform`](crate::VfsPlatform)
/// at construction. Everything here is a value a policy chose, not a value read
/// from the host — that separation is what keeps `env_dict!` from being the
/// hole it is in an ordinary process.
#[derive(Debug, Clone)]
pub struct SessionFacts {
    /// The environment, already reduced to D21's synthesized and passthrough
    /// classes by the launcher. The effects read it; they do not filter it,
    /// because filtering is a policy decision made once, before the guest runs.
    pub env: BTreeMap<String, String>,
    /// The platform the guest believes it is on. See [`PlatformTarget`].
    pub platform: PlatformTarget,
    /// A **session-scoped** process id, not the host's. rocjust reads its own
    /// pid; the host's would correlate a run to a specific machine, and a child
    /// process's pid — which upstream's `signals::` tests want — is the
    /// scheduler's to hand out, not this.
    pub pid: i64,
    /// The parallelism the guest may assume — the job limit, not the machine's
    /// core count. A confined session's fan-out is a quota (D35), so reporting
    /// the host's cores would invite a guest to oversubscribe a budget it does
    /// not have.
    pub num_cpus: i64,
    /// The virtual path `env_temp_dir!` reports. A policy value: a bare mount
    /// namespace has no temp directory, and a launcher that grants one names it
    /// here.
    pub temp_dir: String,
}

impl SessionFacts {
    /// Neutral facts for a test or a bare session.
    ///
    /// Not a `Default` impl on purpose: a launcher must *choose* these, and an
    /// impl that manufactures them invites reading a default where a decision
    /// belongs. Tests want a fixed starting point, and say so by calling this.
    #[must_use]
    pub fn neutral() -> Self {
        Self {
            env: BTreeMap::new(),
            platform: PlatformTarget {
                os: TargetOs::Linux,
                arch: TargetArch::X64,
            },
            pid: 1,
            num_cpus: 1,
            temp_dir: String::from("/tmp"),
        }
    }
}
