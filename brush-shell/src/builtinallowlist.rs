//! The default allowlist for `--restrict-builtins` (D11).
//!
//! D11's argument is that badness cannot be enumerated across sixty-odd
//! builtins and four forked upstreams, so the list has to run the other way:
//! what a recipe needs, and nothing else. The set below is a starting position,
//! not a derived truth — it is the one part of this feature that is a judgement
//! about what recipes do, and it is meant to be edited. `--allow-builtin NAME`
//! widens it without a rebuild.
//!
//! Every bundled utility is admitted on top of this list. They are the point of
//! the closed world: `cat`, `ls`, `grep` and the rest are already confined by
//! the namespace, and removing them would leave a shell that can run nothing.

/// Shell builtins a recipe runner is assumed to need.
///
/// Anything in the registry and not here is denied, so this list *is* the whole
/// policy. What it leaves out falls into classes, recorded here rather than as
/// a comment per name because a reader deciding whether to widen the list wants
/// the class:
///
/// - **Registry mutation**: `enable`, `builtin`. `enable` is the measured
///   bypass this policy exists to defeat; `builtin` reaches the registry by
///   name, which is redundant once the registry is filtered.
/// - **Interactive and terminal** (D36 -- the sandbox has no terminal): `bind`,
///   `fc`, `history`, `suspend`, `logout`, `fg`, `bg`, `complete`, `compgen`,
///   `compopt`, `help`.
/// - **Host-facing or host-revealing**: `hash` (writes host paths into the
///   location cache), `caller`, `brushctl`, `brushinfo`, `ulimit`, `umask`,
///   `disown`.
/// - **Aliases**, which a non-interactive shell does not expand anyway:
///   `alias`, `unalias`.
/// - **Directory stack**, redundant with `cd`: `dirs`, `pushd`, `popd`.
/// - **Array-reading conveniences** with no established need: `mapfile`,
///   `readarray`.
///
/// `exec` is *allowed*, against the instinct to deny it. Its program form is
/// already governed by D2's predicate, which is a stronger check than a name
/// list; what denying it would remove is `exec 3>&1`, an ordinary redirection
/// idiom with no host reach.
pub(crate) const DEFAULT_ALLOWED: &[&str] = &[
    // POSIX special builtins and control flow.
    ".", ":", "break", "continue", "eval", "exec", "exit", "export", "readonly", "return", "set",
    "shift", "source", "times", "trap", "unset", //
    // Ordinary shell work.
    "[", "cd", "command", "declare", "echo", "false", "getopts", "let", "local", "printf", "pwd",
    "read", "shopt", "test", "true", "type", "typeset", //
    // Job control. D25's `spawn`/`wait_any` is built on these.
    "jobs", "kill", "wait",
];

/// Builds the policy for `--restrict-builtins`.
///
/// `bundled` names every utility in the installed bundled registry, `allow` is
/// what `--allow-builtin` added, and `deny` is what `--deny-builtin` removed.
/// Denial is applied last, so it also reaches the bundled set -- which is
/// admitted wholesale and is otherwise not deniable at all.
pub(crate) fn policy<'a>(
    bundled: impl IntoIterator<Item = &'a String>,
    allow: &[String],
    deny: &[String],
) -> brush_core::BuiltinPolicy {
    let mut names: std::collections::BTreeSet<String> = DEFAULT_ALLOWED
        .iter()
        .map(|s| (*s).to_owned())
        .chain(bundled.into_iter().cloned())
        .chain(allow.iter().cloned())
        .collect();
    for name in deny {
        names.remove(name);
    }
    brush_core::BuiltinPolicy::Allowlist(names)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_list_is_free_of_duplicates() {
        let mut seen = std::collections::BTreeSet::new();
        for name in DEFAULT_ALLOWED {
            assert!(seen.insert(*name), "{name} appears twice");
        }
    }

    #[test]
    fn the_measured_bypass_is_denied() {
        // `enable` is the reason a denied builtin must be absent rather than
        // flagged; if it is ever admitted, the policy defeats itself.
        assert!(
            !DEFAULT_ALLOWED.contains(&"enable"),
            "admitting `enable` lets a script re-register anything this policy denied"
        );
    }

    #[test]
    fn bundled_utilities_and_extras_are_admitted() {
        let bundled = vec!["cat".to_owned(), "ls".to_owned()];
        let policy = policy(&bundled, &["umask".to_owned()], &[]);
        assert!(policy.admits("cat"));
        assert!(policy.admits("ls"));
        assert!(
            policy.admits("umask"),
            "--allow-builtin must widen the list"
        );
        assert!(policy.admits("echo"), "the default list must still apply");
        assert!(!policy.admits("enable"));
    }

    #[test]
    fn denial_reaches_the_bundled_set_and_wins_over_allow() {
        let bundled = vec!["cat".to_owned(), "find".to_owned()];
        let policy = policy(
            &bundled,
            &["umask".to_owned()],
            &["find".to_owned(), "umask".to_owned()],
        );
        assert!(policy.admits("cat"));
        assert!(
            !policy.admits("find"),
            "a bundled utility is admitted wholesale, so denial is the only way to remove one"
        );
        assert!(
            !policy.admits("umask"),
            "--deny-builtin is applied after --allow-builtin"
        );
    }
}
