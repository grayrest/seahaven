# The residual patch set

D13's model is that forks are **generated, not maintained**: `cargo xtask
vendor-fork` re-vendors pristine upstream and re-applies the codemod, and the
size of what is left over per rebase is the health metric.

This file is that leftover. Everything listed here is hand-written, is **not**
reproduced by re-running the tooling, and is therefore **lost by a re-vendor** —
which happened twice during this milestone. Re-apply from git after regenerating
a fork, and treat growth in this list as the signal it is meant to be.

The forks are linted by `cargo xtask check lint`, against the record in
`UNROUTED.txt`; the entries below that describe a *deliberate* unrouted call are
listed there too, so the gate and this file cannot disagree about them. That
record's backlog is empty: every call site in a fork that reaches the host is
now one of the deliberate ones it names.

Only the first loss was caught by `cargo xtask check forks`. The second took all
three `uucore` entries below with it and that gate stayed green, because it runs
each fork's *upstream* suite on the host under an identity session — where
routed and unrouted code behave identically, which is the property that makes it
a useful rebase metric and the reason it cannot see confinement. Nothing in
`cargo test --workspace` covers the gap either: no gate enables
`experimental-bundled-coreutils` or `coreutils.all`, so the bundled utilities
are never exercised by default and both `tests/routing.rs` and the `ls` cases in
`cases/brush/sandbox.yaml` are compiled out or skipped. Until a gate runs with
those features on, a lost routing patch is invisible to CI.

Generated per fork and *not* listed here: `Cargo.toml`, `.gitignore`, the
codemod's rewrites, and `src/**/flatland_test_session.rs` plus its `mod`
declaration.

## `uucore/build.rs` — locale embedding

Upstream embeds each utility's Fluent catalog by scanning its *sibling registry
directory* for `uu_<util>-<version>`, a layout `forks/` cannot reproduce. Left
alone, nothing errors and every utility renders raw Fluent keys with a correct
exit code. Replaced with a scan of `locales/utils/`, vendored by
`cargo xtask vendor-locales`.

Guarded by upstream's own
`mods::locale::tests::test_setup_localization_fallback_to_embedded`.

## `uucore/src/lib/features/safe_copy.rs` — `rustix::fs::open`

`open_source` and `create_dest_restrictive` are `cp`/`mv`'s open path and are
invisible to the `std::fs`-shaped codemod. Routed onto `ambient::open_with`,
carrying `nofollow` and `DEST_INITIAL_MODE`.

## `uucore/src/lib/features/fs.rs` — `rustix` stat, the cwd base, and the root

`FileInformation::from_path` used `rustix::fs::{stat,lstat}`; the type now uses
the `std::fs::Metadata` representation it already had for WASI. Separately,
`canonicalize` built its absolute base from the *host* process cwd while
checking existence against the namespace — now `ambient::current_dir()`.

`canonicalize` also probed each accumulated prefix for a symlink starting with
`/`. On a host filesystem that is a free `Ok(None)`, since no root is a symlink;
the *virtual* root is not backed by any host object at all (D6), so the probe
returned NotFound and the function failed on its own first component. Every
absolute path went through there, which is why `mv` could not move a file
between two paths in the same mount. The root is now pushed and not probed —
skipped rather than special-cased in the error handling, because the question is
meaningless rather than unanswerable.

## `uucore/src/lib/features/fsxattr.rs` — descriptors, not paths

`xattr`'s path functions resolve on the host, so under a mount they address a
file outside it. The crate also ships `xattr::FileExt`, and `brush_vfs`'s
contract *is* descriptors — `Vfs::open_with` hands back a `std::fs::File` — so
`copy_xattrs`, `copy_xattrs_skip_selinux`, `copy_acls`, `retrieve_xattrs` and
`apply_xattrs` open both ends through the facade and work on the descriptors.
Signatures are unchanged, so `uu_cp` and `uu_mv` are untouched.

This corrects a misclassification the decision log had already caught: `xattr`
was listed as a capability the namespace could not express, on a check made
against `cap-fs-ext`'s API surface, which is not the contract.

Read-only descriptors at both ends, which is the part worth measuring rather
than assuming: `fsetxattr` checks the *inode's* permissions rather than the
descriptor's access mode, so a read-only fd can set an attribute — and it is the
only option for a directory, which cannot be opened for writing and which
`cp -r` copies attributes onto. Verified both, including on a directory fd.

`has_acl` and `has_security_cap_acl` stay on the path deliberately. `ls -l`
calls the first once per entry, and upstream's own comment there is about
counting `getxattr` calls; routing it means an open per file listed. Recorded in
`UNROUTED.txt` with that reasoning, and `clippy.toml` now bans `xattr`'s path
functions so a third one cannot join them quietly.

## `uucore/build.rs` and `sed/build.rs` — a crate-level lint allow

Both carry `#![allow(clippy::disallowed_methods)]` with the reason. A build
script runs at build time on the host, before any namespace exists, and cannot
see `brush_vfs` at all — routing one does not compile.

The allow is at the source rather than in `UNROUTED.txt` because of how the gate
works: `check lint` *denies* the ban, a denied lint is an error, and an error in
a build script aborts the crate before its library is ever analysed. That is not
a hypothetical. It hid the whole of `uucore`'s and `sed`'s libraries from the
fork lint — seventeen unrouted call sites reported as "no new ones" — until the
allow let their build scripts compile.

## `uucore/src/lib/features/safe_traversal.rs` — `nix::fcntl::open`

`DirFd::open` accepted any host path. Now rooted through
`ambient::open_dir_fd`. Every `*at` call below it is unchanged. See D3's
amendment for why this roots rather than seals.

## `uu_ls/src/{ls,display}.rs` and `uu_du/src/du.rs` — two type names

`ReadDir` and `DirEntry` are the only two types the facade cannot hand back as
`std`'s own, because neither has a public constructor. These three files *name*
them in signatures, so the names follow the calls. Everything else the codemod
touches keeps its `std` type, which is what makes it an identifier swap rather
than a type swap (D34).

## `findutils`, `uu_cp`, `uu_grep` — the walk

Each swapped `walkdir` for `brush_vfs::walk`, which mirrors its API, so the
change is an import and a constructor per crate. `uu_cp` also swaps the error
type its `CpError::WalkDirErr` wraps, and `findutils` swaps the entry and error
types its `WalkEntry` adapter converts from. Their `walkdir` dependency is
removed from the generated manifest, which is why `deny.toml` no longer lists
them.

`findutils/known-test-failures.txt` records the one upstream test that diverges
under routing, macOS only.

## `uu_grep` — two `Path::is_dir` calls, routed by hand

`lib.rs` and `searcher.rs` each ask `Path::is_dir()` before deciding whether to
recurse. It is an *inherent* method, which D34's signature-preservation rule
puts outside what the codemod can see, so both asked the **host**. Under a
namespace the host answers "no" for every virtual path, and `grep -r /work`
degraded to reading a directory as a file: `grep: /work: No such file or
directory`.

Invisible until D24 gave a bundled child a real namespace to be wrong about --
before that, the child ran under identity and no virtual path resolved for it
either, so the two failures looked the same. Both sites now go through
`brush_vfs::ambient::metadata`.

## `uu_ls` — three `Path::metadata` calls, routed by hand

The same D34 carve-out as `uu_grep`'s, in the utility that feels it most.
`Path::metadata` is an *inherent* method, so the codemod cannot see it, and
three sites asked the host: the `Dereference::DirArgs` arm, the
`must_dereference` fast path in `PathData::new`, and the dereferencing arm of
`get_metadata_with_deref_opt`. The last is the funnel for `PathData::metadata`,
`--group-directories-first` and `get_security_context`, and it is the more
telling of the two arms: its `symlink_metadata` half *was* rewritten, because
that one is reached as a free function, so the function asked the namespace and
the host one line apart.

Under a namespace the host answers "no such file" for every virtual path, so
`ls <symlink-to-dir>` printed the link's own name instead of listing its target,
and anything under `-L` failed outright with `ls: cannot access '/work'`. Pinned
by three cases in `brush-shell/tests/cases/brush/sandbox.yaml`.

`dotdot_path`'s `dotdot.metadata()` is the same shape and is left alone: it is
inside `#[cfg(target_os = "wasi")]`, so no build this repo gates on reaches it
and there is no failing case to point at.

## `uu_ln` — the symlink write, and five predicates

The only one of these that was an *escape* rather than a wrong answer.
`std::os::unix::fs::symlink` writes to the host and resolves the link name
against the host process's working directory, so under a mount
`cd /work && ln -s f.txt newlink` exited 0 and created `newlink` outside the
namespace, where nothing in the shell could reach it. Routed by swapping the
import for `brush_vfs::ambient::symlink`, which takes the same two arguments in
the same order — so every call site is unchanged, which is the codemod's own
idiom — and which additionally checks that the stored target lands inside the
mount. `ln -s ../f.txt up` used to hand out a name for the mount's parent and
is now refused. The WASI arm spelled the same write `rustix::fs::symlink` and
is routed with it.

Note that the ban list already named `std::os::unix::fs::symlink`, and clippy
flags it the moment anyone lints this crate. Nobody does: the forks are not
workspace members, so `cargo xtask check lint` never sees them. The ban was
working and unread.

Five inherent `Path` predicates go with it — two `is_dir` deciding which *form*
of the command line this is, and three `is_symlink` — all the D34 carve-out
again. Each has its own failing case, and one is worse than a wrong answer:
with `exists` routed and `is_symlink` not, `ln -sf` over an existing link took
the overwrite branch, removed the real link *through the namespace*, and wrote
its replacement to the host. The file was gone from the mount and the new link
was outside it, in one step.

Pinned by four cases in `brush-shell/tests/cases/brush/sandbox.yaml`. They
mount a subdirectory rather than `.`, because with the mount at `.` the host
working directory and the mount root are the same directory and an escaped
write is indistinguishable from a confined one.

The `#[cfg(windows)]` arm is deliberately untouched. It dispatches
`symlink_dir` / `symlink_file`, a distinction `Vfs::symlink` does not take and
which cap-std resolves its own way; routing it is a change to Windows behaviour
this repository cannot test and has no failing case for.

## `uu_mv` — the absolute base, twenty predicates, and one descriptor

The largest single fork patch, and the one where the D34 carve-out compounds:
`mv` asks the filesystem what kind of thing each operand is at nearly every
step, and each answer came from the host.

`std::path::absolute` is the interesting one, because it is not a predicate.
It is `getcwd(2)` plus a join, so both operands of every same-file comparison
were rooted at the *host* working directory — `mv f.txt g.txt` under a mount
failed naming a host path component. There was no facade function for it, so
one was added: `brush_vfs::ambient::absolute` takes the session's cwd and the
virtual-path grammar, which also means the result is normalised and a path
that leaves the namespace is an error rather than a string. The name is
unchanged, so the five call sites are.

Then twenty inherent `Path::{is_dir, is_file, is_symlink, metadata}` calls
across `mv.rs` and `hardlink.rs`, routed onto their facade equivalents. These
decided which *form* of the command line was being run, whether the target was
a directory, whether either side was a symlink, and which error to report — so
the visible failures ranged from `target 'dir': Not a directory` for a plain
directory to silently taking the wrong branch.

`create_symlink_replace` is the `DirFd` shape again. It mirrors GNU's
`force_symlinkat`: open the destination's parent once and operate through `*at`
so a concurrent rename of the parent cannot redirect the operation. The `openat`
that opened that parent anchored on `CWD` — the host process's directory, the
single ambient entry point for the whole sequence. It is now rooted through
`ambient::open_dir_fd`, and the `symlinkat`/`renameat`/`unlinkat` below it are
unchanged, inheriting the confinement of the descriptor they start from, exactly
as `safe_traversal` does.

`nix::unistd::mkfifo` is left alone, in both fallbacks that use it. There is no
facade equivalent — the ban list says "not expressible in the namespace yet" and
means it — and both sites sit behind an `EXDEV` cross-device fallback that a
single mount cannot reach, so there is no failing case to point at either. It is
a real hole for a future multi-mount `mv` of a FIFO, and it is recorded here
rather than fixed blind.

Pinned by four cases in `brush-shell/tests/cases/brush/sandbox.yaml`, and by a
differential check while developing: every `mv` form tried produced byte-identical
output under a restrictive mount and under the identity policy, which is the
oracle that isolates confinement from upstream behaviour.

## `uucore::perms` — left on `walkdir`, deliberately

`dive_into` is the last walk in the fork set that is not routed, and it stays
that way. It is unreachable from anything bundled: `chmod`, `chown` and `chgrp`
are not in the coreutils set, and on Linux `perms` takes the vfs-rooted `DirFd`
path regardless. Porting it would need a routed `chown` -- `libc::chown` takes a
host path, so a virtual one from the walk would be wrong -- which is real
trusted-boundary code written for a caller that does not exist. `deny.toml` says
the same thing next to the entry.

## `uu_df` — an exemption, not a patch

Vendored with `--skip src/filesystem.rs`. That module canonicalizes mount
*device names* out of the host mount table: host introspection, not namespace
access, the same class as `uucore::fsext`. Routing it makes `df` report nothing,
which upstream's `test_dev_name_match` catches with `MountMissing`. `df` is
therefore **not confined**, by decision.
