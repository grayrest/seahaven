//! The default-deny builtin allowlist (D11): a policy on which builtins exist.
//!
//! The vfs decides what a builtin may *open* (D3, D6) and the closed world
//! decides what may be *executed* (D2). Neither says anything about what a
//! builtin may *do*, and that is the surface this policy governs.
//!
//! # Why a denied builtin is absent rather than flagged
//!
//! The registry already carries a `disabled` flag, honoured at every dispatch
//! site, and it is the wrong mechanism twice over. `enable NAME` clears it from
//! inside the shell, so a sandboxed script un-does any deny expressed that way;
//! and `builtins` is `serde(skip)`, with the reconstituting path rebuilding the
//! default set unconditionally, so it does not survive a round trip either.
//!
//! So a denied builtin is never *registered*. That single choice is why this
//! module is small: the three dispatch reads and the seven listing readers all
//! ask the registry, and a name that is not in it is uniformly "not a shell
//! builtin" — the answer `enable`, `command -v`, `type` and completion already
//! give for a name that never existed.
//!
//! # What it does not reach
//!
//! An allowlist is a statement about *names*, so it cannot fix a permitted
//! builtin that does too much. `kill` is on the default list because a recipe
//! runner needs job control, and its bare-PID form signals any process on the
//! host; that is bounded separately, in the builtin. `BASH_FUNC_*` inheritance
//! is not a builtin at all. Both are governed by asking this policy whether it
//! is [`Open`](BuiltinPolicy::Open), which is the closest thing the shell has to
//! "am I sandboxed", not by the list itself.

use std::collections::BTreeSet;

/// Policy governing which builtins a shell registers.
#[derive(Debug, Clone, Default)]
pub enum BuiltinPolicy {
    /// Deny everything. The fail-closed default a shell built without a policy
    /// — or reconstituted from disk, where the registry is not data that
    /// survives serialization — lands on, matching the session's
    /// empty-namespace default and [`ExternalExecution::Sealed`].
    ///
    /// [`ExternalExecution::Sealed`]: crate::execpolicy::ExternalExecution::Sealed
    #[default]
    Sealed,
    /// Register whatever is offered. What the identity policy uses, so the
    /// shell behaves as an ordinary bash and the compatibility suite runs the
    /// production binary unchanged. A no-op predicate.
    Open,
    /// Register only these names.
    Allowlist(BTreeSet<String>),
}

impl BuiltinPolicy {
    /// Builds an allowlist from a collection of names.
    pub fn allowlist<I, S>(names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self::Allowlist(names.into_iter().map(Into::into).collect())
    }

    /// Whether `name` may be registered.
    #[must_use]
    pub fn admits(&self, name: &str) -> bool {
        match self {
            Self::Sealed => false,
            Self::Open => true,
            Self::Allowlist(names) => names.contains(name),
        }
    }

    /// Whether this policy restricts anything at all.
    ///
    /// Read by the two escapes an allowlist cannot express — `kill`'s bare-PID
    /// form and `BASH_FUNC_*` inheritance — because "is this shell sandboxed"
    /// is the question they actually need answered and the shell has no better
    /// way to ask it. When D24 gives a session a policy object of its own, that
    /// is where this belongs.
    #[must_use]
    pub const fn is_open(&self) -> bool {
        matches!(self, Self::Open)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_is_fail_closed() {
        assert!(!BuiltinPolicy::default().admits("echo"));
        assert!(!BuiltinPolicy::default().is_open());
    }

    #[test]
    fn an_open_policy_admits_anything() {
        assert!(BuiltinPolicy::Open.admits("echo"));
        assert!(BuiltinPolicy::Open.admits("no-such-builtin"));
    }

    #[test]
    fn an_allowlist_admits_only_what_it_names() {
        let policy = BuiltinPolicy::allowlist(["echo", "printf"]);
        assert!(policy.admits("echo"));
        assert!(!policy.admits("enable"));
        assert!(!policy.is_open());
    }
}
