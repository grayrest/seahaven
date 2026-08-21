# Flatland sandbox architecture — decision log

Companion to `plans/2026-08-18-vfs-foundation.md`. Flatland is a fork of `brush`
becoming the platform under `~/dev/roc/rocjust`.

**Two boundaries against two different adversaries.** Wasmtime contains the Roc
application; the vfs contains the shell and the forked utilities. Neither
substitutes for the other — `cap-std`'s own README states it is not a sandbox
for untrusted Rust code, so one mechanism cannot carry both.

Decisions were resolved by owner Q&A, then subjected to three rounds of
adversarial review. **Review findings are folded into the decisions they
correct**, marked `Corrected:`, rather than kept as a separate layer — an
implementation brief should not teach something and retract it forty entries
later. `[ASSUMPTION]` marks the few resolved from existing patterns without an
owner call.

## Nothing is open

The six questions that were open here were resolved by owner Q&A on 2026-08-18
and folded into the decisions they qualify: `ro` in D6, the unexpressible
capabilities in D4, the Wasmtime configuration in D35, grant derivation in D16,
handle design in D43, and the `sys/wasm` question in D37.

---

## D1 — Two boundaries, not one

The opening frame was "rewrite all filesystem access through a limited set of
functions". That secures Rust code in this repo and leaves the `uu_*` crates and
external processes outside. Rather than weaken the claim to "guardrail", the
boundary was split: a VM for the Roc app (D17), the vfs for the shell (D3).

## D2 — Closed world: no arbitrary external execution

Rejected: declared programs under OS child-sandboxing, which reopens the ambient
authority hole. Consequence accepted: `just` recipes calling `cargo`/`npm` cannot
run. `awk` has no supplier — `uutils/awk` is 22 stars and WIP.

**Corrected:** this cannot be implemented as "delete the exec path".
`brush-shell/src/bundled.rs:236-263` builds the bundled shim as a `SimpleCommand`
whose name is `current_exe()`'s host path, reaching `Command::new` through the
ordinary external path (`commands.rs:415-419`). D2 is a **predicate** — own exe
*and* `argv[1] == --invoke-bundled` — not a deletion. Exec is never removed.

**Implemented** as `ExternalExecution` (`brush-core/src/execpolicy.rs`), checked
in `compose_std_command` — the single point both the external path and the
`exec` builtin compose a `std::process::Command`, so neither routes around it.
The predicate is the two parts the correction named: the trusted launcher's host
path *and* the dispatch flag in `argv[1]`. The path alone would let `exec
<launcher> -c '...'` start a fresh shell that begins under the identity policy;
the shell-level suite (`brush-shell/tests/closed_world_tests.rs`) pins that this
is refused. The launcher is run by its known host path rather than resolved
through the namespace, which under a restrictive mount does not contain it, and
the executable-lookup guard carries the same exemption so the shim is not turned
away early as "not found". Selected by `--closed-world`, a separate axis from
`--mount`; the default stays `Open` under identity so the compat suite is
unaffected (the predicate is a no-op there).

**What it does not do is confine the child.** A bundled `ls` runs in a freshly
spawned process that inherits ambient authority, and — because nothing
re-installs the namespace across the spawn — an *absolute* virtual path handed to
it (`ls /work`) is misread against the child's identity namespace, while a
relative one (`cat note.txt`, resolved against the parent-translated cwd) works.
Carrying the session to the child is D24's job; D2 decides only whether the
spawn happens at all.

## D3 — `cap-std` capability handles, with a lint ban over them

Rejected: path-validation helpers (convention, not enforcement; TOCTOU-racy) and
a hand-rolled VFS trait (re-derives cap-std's symlink hardening, the part that is
subtly wrong).

**Corrected — the ban is a regression mechanism, not a completeness proof.** It
cannot see dependencies, `#[cfg]`-disabled code, or macro-generated code. What it
*can* see is more than an earlier correction claimed: `disallowed_methods`
matches resolved def-paths, so inherent `Path` methods like `p.metadata()` **are**
caught (verified). Completeness against the residue needs the OS layer — see D41,
which pulls a Landlock-backed test into the first milestone precisely so the
negative claim becomes an executable one.

**Amended (D4's uucore increment) — one capability escapes, under a wrapper
that cannot name a host path.** The original claim was that `cap-std` handles
stay inside `brush-vfs` and callers only ever receive `std::fs::File` and
`std::fs::Metadata`. That holds for every path-shaped caller and remains the
default; the facade the codemod emits is unchanged. It does not hold for a
caller that is *already* descriptor-shaped. `uucore::safe_traversal::DirFd` is
1,464 lines of `openat`/`fstatat`/`unlinkat`/`mkdirat` built precisely so a
recursive walk cannot be redirected between the check and the use, and it is
what `chmod -R` and `chown -R` descend with. Handing it paths to re-resolve
would confine it while destroying the property it exists for, so `brush-vfs`
grows a directory capability and `DirFd` is rooted in one.

The amendment is narrow and the narrowness is the point. What escapes is a
wrapper exposing `*at` operations and **no way to recover a host path** — no
`PathBuf`-returning method, no host-absolute string, no raw fd handed out for a
caller to reconstruct one from `/proc/self/fd`. That single property is what
keeps the original claim's *substance*: a consumer still cannot name a location
outside the namespace, which is what "handles do not escape" was protecting.
Since a mechanism whose failure mode is silence needs proving on — the same
reasoning that gave the ban its positive control — it is asserted by test, not
by review convention.

**And one carve-out inside the amendment, because `DirFd` was not portable onto
the sealed type in this milestone.** `Dir::into_owned_fd_for_at_traversal`
surrenders the descriptor to a caller that does its own syscalls. The fd is
confined *where it lands* — resolution proved the directory is inside the
namespace, and no path survives — but it does not refuse `..`, because a
directory descriptor is a position in the host tree and the kernel will move
upward from it. So `safe_traversal` is *rooted* in the namespace rather than
sealed inside it.

That is a real weakening and is recorded as one rather than described as a
detail. It was taken because the alternative it replaces is worse by a wide
margin — `DirFd::open` accepted **any host path outright** — and because
sealing it means porting `DirFd` onto the capability's API, which needs
`chown`, a call `cap-std` does not provide at all, plus converting `FileStat`
to `Metadata` through every caller and removing the public `AsFd`/`AsRawFd`
impls `perms.rs` depends on. That is a milestone, not a step. The residual
exposure is that a future upstream change starts passing `".."` where it
passes directory entry names today; the carve-out is named in the gate that
would otherwise fail, so a *second* one cannot appear quietly.

And the ban can silently switch itself off in three ways, all verified: globs are
not expanded, a typo'd entry warns but exits 0, and a member-crate `clippy.toml`
shadows the root. **That is why it needs a positive control** rather than trust —
a mechanism whose failure mode is silence must be proven on. Types must not be
banned at all: step 1's facade returns `std::fs::File`, so banning types would
make `brush-vfs`'s own API an `#[allow]`. Types carry no authority; the functions
that mint them do.

## D4 — Fork `uutils/{coreutils, findutils, grep, sed}`

Rejected: dropping coreutils for a hand-written set (loses GNU fidelity and the
upstream suites) and keeping only argv-pure utilities (two filesystem models in
one process).

**Corrected:** the fork set carries capabilities the codemod's transformations
do not cover. Checking each against `cap-fs-ext` splits the list the earlier
correction lumped together, and only two members survive as genuinely
unexpressible:

- **Expressible, merely unwritten — codemod targets, not exemptions.**
  `filetime` (`uu_cp`, `uu_touch`) has `cap_fs_ext::DirExt::set_times` and
  `set_symlink_times`. `fs_extra` (`uu_mv`) and `walkdir` (`uucore`) are
  recursive copy, move and walk, which a walk over `Vfs` expresses; there is no
  crate to swap in, but nothing is blocked.
- **Not expressible: `notify` (`uu_tail`) alone.**

  `xattr` was in this class and should not have been. The check was made
  against `cap-fs-ext`'s API surface, which indeed has no xattr calls — but
  `brush-vfs`'s contract is *descriptors*, not cap-fs-ext, and `uucore` already
  ships the descriptor form: `uucore::fsxattr::copy_xattrs_fd(&File, &File)`,
  whose own doc notes it pins both inodes so a concurrent renamer cannot
  redirect it, unlike the path-based `copy_xattrs` that `uu_cp` calls today.
  `std::fs::File` is exactly what `Vfs::open_with` returns. Same for `ls -l`'s
  ACL indicator: `has_acl` is path-based, but `xattr::FileExt::list_xattr` is
  not. Both are codemod targets.

  `fs_extra` was also mis-scoped: `uu_mv` imports exactly
  `fs_extra::dir::get_size`, used once to size a progress bar. The recursive
  copy is `uu_cp`'s `walkdir`. The real blocker for `mv` is elsewhere — see
  below.
- **Not filesystem questions.** `uu_env`'s exec is governed by D2's predicate,
  not by the namespace. `onig` is a C regex library; its risk class is memory
  safety in a C dependency, and it was in this list by category error.

**The disposition for something the namespace cannot express is to drop the
capability and keep the utility** — `tail` without `-f`, `cp` and `ls` without
xattr preservation. Dropping whole utilities was rejected because `cp` and `ls`
are not optional in a recipe runner; exempting the crates was rejected because
it turns the vfs claim into one a reader has to carry an exception list for;
and writing the missing primitives against raw `*at` syscalls was rejected as
exactly the hand-rolled hardening D3 exists to avoid. The closed world is
already lossy — D2 accepts losing `cargo` and `npm`, and `awk` has no supplier
— so losing a flag is in keeping, and each loss is a documented difference from
GNU that the upstream suites will name case by case.

`env` is kept with its exec form refused: `env` with no command is `printenv`
and is harmless, and `env CMD` already fails closed through D2's predicate, so
the fork only has to make the failure legible.

`onig` is replaced with `fancy-regex`, and that is a **syntax change, not a
dependency swap**. `uu_expr` uses POSIX *BRE* and relies on onig's dialect
selection (`Syntax::grep()`), with a transpiler that deliberately leaves
`\(`, `\)`, `\{`, `\|`, `\+` alone because grep syntax understands them.
fancy-regex inverts exactly those: `(` groups and `\(` is a literal, so
`expr "$s" : '\(.*\)'` — the canonical capture idiom — silently stops
capturing. Also carried by onig and not by the swap: the GNU error messages
keyed on onig's error codes, which the upstream `expr` suite asserts; non-UTF-8
matching, survivable only because D33 pinned UTF-8; and `MatchParam`'s match-step
limit, which D27's ReDoS bound needs re-established as
`fancy_regex::RegexBuilder::backtrack_limit`. The swap stands, but it is a
transpiler rewrite with an upstream test suite to satisfy, not a line in a
manifest.

**The real gap `mv` exposes is in `brush-vfs`, not in a dependency.** There is
no `rename`, no `remove_dir_all` and no link creation: `clippy.toml` bans all of
them as "not expressible in the namespace yet". So the cross-mount
copy-then-delete fallback has no primitive — and **D26's "links are validated
at creation" has no creation site to validate**, which makes that check
unwritten rather than free. *(Since resolved: `rename`, `remove_dir_all` and
`symlink` were added to the namespace in the D2 milestone — see D2's note.)*

**Implemented — first increment (`uu_cat`).** The pipeline is proven end to end
on the smallest utility, and the rest is repetition of it:

- **The codemod's target is `brush_vfs::ambient`** — free functions shaped like
  `std::fs`, reading a process-global session (D34). It exists because a util
  calls `File::open` with no session handle in scope, and D34 forbids threading
  one through; the process is the confinement unit, so a process-global session
  is sound. It fails closed with none installed.
- **The codemod is `cargo xtask codemod`** — a `syn` pass that edits by byte
  span (minimal diff, D13's health metric). It does the identifier swap D34
  specified: `File::open(p)` → `ambient::open(p)`, the returned `File` untouched
  because the facade returns `std` types. This version handles free-function and
  `File::` associated calls and prunes the now-unused imports; the
  inherent-method visitor (the majority of a *large* util's sites, per the
  spike) needs receiver-type inference to tell `path.is_dir()` from a pure
  `FileType::is_dir()` and is deferred — it *reports* rather than pretends. The
  `canonicalize` and xattr carve-outs above are reported, not rewritten.
- **The fork is vendored, not maintained (D13):** pristine upstream committed as
  a baseline, the codemod diff committed separately so it is auditable. It lives
  under `forks/` and is **excluded from the workspace** — upstream does not pass
  brush's pedantic lints, and the ban runs `--workspace`, so a member fork would
  trip it; the fork's routing is proven by the Landlock test (D41), not the lint
  (D3), exactly as the crates.io version it replaced was never linted.
- **Deferred:** the inherent-method visitor, the two carve-outs, and the other
  ~99 coreutils plus findutils/grep/sed. And **child confinement is D24's**: the
  bundled child installs the *identity* session, so `uu_cat` is routed but not
  yet confined to the parent's mounts — the in-process routing test confines it
  directly to prove the routing is real.

**Second increment — the batch, the automation, and two walls.** The pipeline
now has `cargo xtask vendor-fork <name>` (vendor + generate manifest + run the
codemod in one step), and the codemod grew the distinctively-named half of the
inherent-method visitor (`path.exists()`, `.read_link()`, `.read_dir()`,
`.canonicalize()`, `.try_exists()` → `ambient::m(&(recv))`, sound because those
names are `Path`-only and the facade's `AsRef<Path>` bound turns a wrong
receiver into a compile error) plus the `canonicalize` carve-out (a virtual
path). **Five utilities are routed and confinement-proven** — `cat`, `head`,
`wc`, `tac`, `nl` — each verified by an in-process test that a host path outside
the mount does not open.

Two walls the batch exposed, each an owner decision before the remaining ~95:

1. **`uucore` is the real fork boundary, not the `uu_*` crates.** `uu_cksum`
   codemods to *zero* sites: it opens nothing directly, delegating to
   `uucore::checksum`. `uucore` has its own filesystem surface —
   `safe_copy::open_source`, `safe_traversal::DirFd`, and bare `File::open` in
   `fsext`/`smack`/`uptime`. So forking a leaf `uu_*` routes only the utilities
   that open directly (the five above do; the confinement test is how you tell).
   The rest need `uucore` itself forked and routed, which D13's spike already
   pointed at ("route through `safe_traversal::DirFd`, make its root
   injectable") — but `DirFd` is Unix-only and `nix`-based, so reconciling it
   with the cap-std, cross-platform facade is a real design task, not a codemod
   run. **This is the gating decision for scale.**
2. **`symlink_metadata` has no `std::fs::Metadata` form under cap-std.** The
   facade returns `std` types so signatures survive, but cap-std's
   `symlink_metadata` yields a `cap_std::fs::Metadata` with no conversion to
   `std::fs::Metadata` (which has no public constructor). Any utility that keeps
   a `Metadata` across calls — `ls` most of all — cannot be routed until this is
   resolved (a facade `Metadata` type breaks signature preservation; a
   descriptor-based `fstatat` needs writing). `metadata`/`is_dir`/`is_file` as
   *methods* are the related unsolved case: ambiguous between `Path` and
   `File`/`FileType`, they need receiver-type inference the syntactic codemod
   does not have.

   **Wall 2 resolved (`symlink_metadata`).** `Vfs::symlink_metadata` opens the
   link as itself — `O_PATH`/`O_NOFOLLOW` (Linux/BSD) or `O_SYMLINK` (macOS)
   relative to a cap-std-confined parent descriptor — and `fstat`s it, yielding a
   genuine `std::fs::Metadata`. Signature preservation holds; the codemod routes
   it. The ambiguous-*method* case (`p.metadata()`) remains, needing type
   inference. Windows follows the final link, a documented gap.

   **Wall 1 assessed (`uucore`), and it is milestone-sized, not a codemod run.**
   `uucore` is ~34k lines and its filesystem access is *two* kinds that need
   opposite treatment. **Namespace access** — `checksum`, `buf_copy`,
   `safe_copy`, `fs` operate on the user's files and must route. **System
   introspection** — `fsext` reads `/etc/mtab`, `uptime` reads `/proc/uptime`,
   `smack`/`proc_info`/`selinux` read `/proc` and `/sys` — names host paths that
   are *not* in any namespace; routing them through the vfs would make `df`,
   `uptime` and friends fail, so they must be left on ambient `std::fs` (and
   exempted from the ban, since the fork is unlinted anyway). And
   `safe_traversal::DirFd` — the abstraction D13's spike wanted to route — uses
   `nix` `openat`/`fstatat`, invisible to the `std::fs`-shaped codemod; making
   its root injectable is hand work. So `uucore` is not one `vendor-fork`; it is
   a per-module triage (route / leave / hand-route) that warrants its own plan.

   **That plan is written: `plans/2026-08-18-uucore-fork.md`.** Measuring it
   sharpened the assessment above in four ways. The triage is *smaller* than
   feared — under the features `coreutils.all` actually resolves to, `uptime`,
   `proc-info`, `selinux`, `smack` and `benchmark` are **not compiled**, so the
   compiled "leave" list is three modules (`fsext`, `mods/locale.rs`,
   `mods/os.rs`) against seven that route. `safe_traversal` is also smaller: it
   has exactly **two** ambient entry points (`DirFd::open`,
   `create_dir_all_safe`), and every `*at` method below them is already confined
   to the root those produce — D13's spike prediction landing intact. Production
   carve-outs are **zero**; all 19 sites the codemod leaves unrouted are inside
   `#[cfg(test)]` or the un-compiled `benchmark` feature. And two obstacles the
   assessment missed, both silent: uucore's `build.rs` embeds locale files by
   scanning the sibling *registry* directory for `uu_<util>-<version>`, which
   `forks/` cannot satisfy, so a naive fork renders raw Fluent keys with a
   correct exit code; and the codemod currently rewrites `#[cfg(test)]` bodies,
   which D13's health metric cannot survive — the five existing forks escaped
   only because none has a filesystem call inside a test module.

   **The coreutils half is done.** 48 utility forks plus `uucore`, all
   generated by `cargo xtask vendor-fork` and gated by `cargo xtask check
   forks`, which runs every fork's own upstream suite as D13's health metric
   (49/49 green). 41 utilities are proven confined in one sweep by
   `brush-coreutils-builtins/tests/routing.rs`.

   Two findings worth keeping. **Only 48 of 83 utilities needed forking at
   all** — the other 35 reach the filesystem exclusively through `uucore`, so
   routing that one crate confined them, `cksum` being the clean example (it
   routes to zero sites of its own). Surveying before forking was worth more
   than any individual fork. And **the two-copies hazard is not specific to
   `uucore`**: `uu_base64` wraps `uu_base32::base_common`, so repointing only
   the consumer manifest left `base64` reading through an unforked `base32`.
   Every fork is patched now, which reaches transitive dependents too.

   Two utilities are **deliberately not confined**, on the same
   introspection-not-namespace reasoning that exempts `fsext`: `df`, whose
   `filesystem.rs` canonicalizes mount device names out of the host mount
   table, and by extension anything else that reports on the host rather than
   the workspace. `df` had been passing the confinement sweep incidentally,
   which is worse than failing it, so it is excluded by name with its reason.

   **`findutils`, `grep` and `sed` are forked and routed, and they were a
   different kind of work.** They are not dependencies of this workspace, so
   forking them began with a decision to bundle — new dependencies, new feature
   flags, new registry entries — rather than with routing. `findutils` supplies
   two utilities, `find` and `xargs`. The fork set is 52 crates and all 52
   upstream suites are green.

   **They are not reachable from the binary, and that is an oversight rather
   than a decision.** `brush-coreutils-builtins` gained `findutils.all` and
   `textutils.all`, and its confinement sweep exercises all four under
   `--all-features`; `brush-shell`'s `experimental-bundled-coreutils` was never
   widened past `coreutils.all`. Measured on a build with `--features
   experimental`: `type -t cat` says `builtin` and `type -t find`, `xargs`,
   `grep` and `sed` all say `file` — the host's copies, which a closed world
   then refuses. Wiring the feature through changes what the shipped binary
   contains, so it is called out here rather than done in passing.

   **`find` was held back, and the reason generalises.** It is a *traversal*,
   not an open: routing every open it makes would still have let it enumerate
   the host tree, because the walk itself was `walkdir`, which takes a path and
   descends by path. Bundling it before that was fixed would have shipped a
   utility whose confinement claim was false in the one way that matters most
   for a directory tree. So `find` stayed out of the registry until
   `brush_vfs::walk` existed (`plans/2026-08-21-vfs-walker.md`) — a walk that
   descends by directory capability and yields virtual paths — and `grep -r` and
   `cp -r` moved onto it in the same change. The confinement sweep asserts that
   a `find` rooted outside the mount enumerates **nothing**, which a non-zero
   exit code alone would not have shown.

   **One `walkdir` consumer is left, deliberately.** `uucore::perms`'s `-R` walk
   needs `chown`, which `cap-std` does not provide and the namespace therefore
   cannot express. It is unreachable from every bundled utility — `chown`,
   `chgrp` and `chmod` are not in the registry — so it is dead code rather than
   a hole, and it is named as such in `deny.toml` and
   `forks/RESIDUAL-PATCHES.md` rather than exempted silently. `notify`
   (`uu_tail -f`) remains the one capability this entry already declared
   unexpressible.

   **D4 is complete.**

## D5 — Re-exec now; in-process is a different project

`bundled.rs:3` already spawns `current_exe() --invoke-bundled` *because* uutils
code reads the host process's standard fds. A `uumain` is process-shaped: process
cwd, process env, global stdout, `std::process::exit`, global SIGPIPE state. The
process boundary supplies all five, so the fork diff is only the fs rewrite.

Under D34's signature-preservation rule the in-process migration is a **separate
project**, not a later phase. It is also **load-bearing for D33**: `setlocale` is
process-global C-library state (and `onig` carries its own), so "session locale"
is expressible only while one process serves one session.

## D6 — Virtual root composed from mounts

Policy is `(virtual_path, host_dir, ro|rw)`; the union is the app's `/`. Rejected:
a single real subtree and WASI-style real preopens — both leak host layout, so
nothing is hermetic and Windows drive letters reach scripts. Virtualisation makes
host paths *unnameable*: an escape has no syntax.

**Corrected — three leaks in that claim.** `echo ~root` names a host path
(`expansion.rs:1050` → passwd DB). Same class, all function calls rather than
variables so D21's policy does not reach them: `prompt.rs:87,96,118` (`\u`, root
test, `\h`), `completion.rs:552,568,637` (`compgen -g/-u/-A hostname`),
`wellknownvars.rs:459` (`$SHELL`). And `test -ef` exposes host device numbers,
leaking mount layout — synthesise stable per-session ids or accept it explicitly.

**`ro` is a policy property, not a kernel one.** `openat(dirfd, name,
O_WRONLY|O_CREAT)` succeeds regardless of how the dirfd was opened, and
`cap_std::fs::Dir` has no read-only mode, so `ro` is a field checked in
userspace. Making it kernel-real was considered and rejected: Landlock would do
it on Linux and leave macOS and Windows behaving differently, and a
writer-holding broker would buy uniformity at the price of pulling a whole
later milestone forward and paying IPC per open. The claim is therefore scoped
rather than strengthened — **`ro` holds for code inside the `brush-vfs`
boundary**, which the clippy ban bounds. It says nothing about code that reaches
the filesystem another way, which today means the bundled coreutils and every
external command.

**The Landlock test does not exercise `ro`, and an earlier draft of this entry
claimed it did.** That test builds one mount, read-write, and its ruleset is
`AccessFs::from_all`, so it could not distinguish a read-only mount even if it
had one — Landlock would grant write regardless. `--mount VIRTUAL:HOST:ro` is a
shipped flag, so `ro` is a live property whose only coverage is the vfs unit
suite and the shell-level expectation cases.

## D7 — No in-memory mounts

Considered and dropped. `openfiles.rs:139` requires an `OwnedFd` for every
`OpenFile` variant, and `echo hi > /tmp/f` then `wc -l /tmp/f` are two different
child processes, so the bytes would live in the parent's heap where no child can
see them. `memfd_create` is Linux/FreeBSD/Android only.

**Corrected (minor):** that function is `#[cfg(unix)]`, and the enum already
carries a `Stream(Box<dyn Stream>)` extension point. The cross-process argument
stands.

## D8 — Identity mount policy keeps the compat suite

Sandboxing is always compiled in; an *identity* policy (host `/` mounted rw) is a
policy, not a code path, so the 2212-case suite runs the production binary with no
`#[cfg]` divergence to rot. The ~21 files invoking `bash`/`make` are dropped
rather than adding a test-only exec feature.

**Corrected — this does not avoid two configurations, it relocates them.** What
rots is not a `#[cfg]` but a *branch never taken*: under identity every
`if policy.restrictive { reject }` is untested code in the shipped binary. The
plan traded a compile-time divergence, which a build catches, for a runtime one,
which nothing catches. The answer is a second gate running a compat subset under
a `strict` policy — added to the plan as gate 2.

## D9 — The shell is permanently network-free

brush has *zero* network code — no `TcpStream`, no `reqwest`, no TLS, no
`/dev/tcp`. With external execution gone, reachability is already nil at no cost.
A `fetch` builtin was rejected: it re-creates curl's exfiltration surface in the
least-typed layer.

**Corrected:** D24's broker adds an AF_UNIX listener and, on Windows, a named pipe
reachable over SMB as `\\host\pipe\name` when IPC$ is exposed. Nothing enforces
the network-free property — tokio is already built with the `net` feature.

## D10 — Ambient paths for the Roc API

`Dir`-typed effects would document an app's footprint in its signatures but buy
no confinement against the stated adversary: an untrusted app handed a root `Dir`
threads it everywhere. Confinement comes from the mount table regardless, so
rocjust's ~40 call sites are a mechanical port.

## D11 — Default-deny allowlist, not an enumerated kill list

Two escapes surfaced from reading two files: `brush-builtins/src/kill.rs:122-125`
passes a bare numeric argument to `kill_process` with no job-table check (any host
PID), and `wellknownvars.rs:28` parses `BASH_FUNC_*` env entries into shell
functions — the Shellshock shape. You cannot enumerate badness across 61 builtins
and five forked projects.

**Corrected:** the allowlist cannot be *registry* state. `shell.rs:127` marks
`builtins` `serde(skip)`, and the only deserialize path rebuilds the default set
(`brush-shell/src/entry.rs:451`), so a per-policy disable would be reset on the far
side of D24's broker. It must be a policy value inside the session blob.

**Corrected again, and this is the sharper reason.** The registry already
carries a `disabled` flag, honoured at every dispatch site — and `enable NAME`
clears it from inside the shell. Measured: `enable -n kill` blocks both `kill`
and `builtin kill`, then `enable kill` restores it and the host process dies. So
the flag is reset twice over, once by a script and once by a round trip, and
the conclusion is not "guard `enable`" — that is the enumerated kill list this
entry rejects — but that **a denied builtin must not be in the registry at
all**.

**Implemented** as `BuiltinPolicy` (`brush-core/src/builtinpolicy.rs`), enforced
at *registration* rather than dispatch, which is why it is small: the three
dispatch reads and the seven listing readers all ask the registry, and a name
that is not in it is uniformly "not a shell builtin". Selected by
`--restrict-builtins`, a separate axis from `--mount` and `--closed-world`;
`--allow-builtin` and `--deny-builtin` adjust the list without a rebuild. The
default admits 36 of 63 shell builtins plus every bundled utility.

**Both escapes this entry names are fixed, and neither by the allowlist.** They
were reproduced against the shipped binary under `--mount` and `--closed-world`
first — the `sleep` died, the injected function ran. A recipe runner needs
`kill`, so `kill` is on the list and its bare-PID form is now resolved through
the job table; `BASH_FUNC_*` is not a builtin at all, so it is dropped
separately. Both ask `BuiltinPolicy::is_open()`, which is the closest thing the
shell has to "am I sandboxed" and belongs on D24's session object instead.

**Two limits worth stating.** D2's predicate had to grow the dispatched utility
name: `<launcher> --invoke-bundled NAME` runs in a fresh process reading the
*process-global* bundled registry, so a shell-side allowlist alone is routed
around by composing the dispatch directly. And denying a builtin without a
closed world **promotes** it — the name falls through to an external lookup, so
a denied `find` becomes the host's `/usr/bin/find`. `--restrict-builtins` is
only meaningful alongside `--closed-world`, which is asserted in both directions
rather than left as a reader's assumption.

## D12 — Strict virtual path grammar carries cross-platform parity

cap-std is kernel-enforced on Linux (`openat2(RESOLVE_BENEATH)`) and FreeBSD, and
userspace-emulated on macOS and Windows. Since the app names only *virtual* paths
and the host path is constructed by us, the Windows pathology class — reserved
device names, 8.3 short names, alternate data streams, trailing dots,
drive-relative paths — is deleted rather than defended.

## D13 — The forks are generated, not maintained

A `syn` codemod re-run on each upstream import; the diff is the codemod plus a
residual patch set, whose size per rebase is the health metric.

**A deliberate divergence gets an expected-failure entry, keyed to the decision
that causes it.** The upstream suites are the other health metric, and they only
stay one if a failure means something. So each upstream case a divergence breaks
names the decision responsible -- D26's link rewrite is the first -- and a case
that starts *passing* is itself a failure, the same rule the compat harness
already applies to its 379 known failures. Editing an upstream case to assert
our behaviour was rejected: the next rebase reverts it silently and nothing
records why it differed.

**The compat harness's precedent does not transfer intact.** Its marker is a
field on a case brush owns, co-located with the case, needing no identifier.
Upstream coreutils cases are `#[test]` functions in a tree this decision forbids
editing, so the marker has to live in an external list keyed by a test path that
upstream renames and splits freely. That makes the mirror rule mandatory rather
than optional: **an entry whose case no longer exists is itself an error**, or a
rename converts "expected failure" into silence — the exact outcome the rule
exists to prevent. Deriving the keys from the codemod's own `syn` pass over the
test tree is the way to keep the list honest; failing that, gating on the count
and reconciling per rebase is a named manual step rather than an oversight.

**Revised after the spike:** `uucore` **already ships the abstraction point** —
`safe_traversal.rs`, 1,464 lines of `DirFd` over
`openat`/`fstatat`/`unlinkat`/`mkdirat`/`fchmodat`, described upstream as
"TOCTOU-safe filesystem operations for recursive traversal". Unix-only,
`nix`-based, and adopted only where TOCTOU mattered — `uu_ls` does not use it. So
the upstream ask is not "accept a new abstraction" but "route through the one you
built, and let its root be injectable". Far easier to land, and every utility
migrated upstream is one the codemod stops touching.

**Realized for the first fork (`uu_cat`).** The vendor-then-codemod split is
concrete: `forks/uu_cat` holds pristine upstream in one commit and the
`cargo xtask codemod` output in the next, so the transformation is a reviewable
diff on its own. The residual patch set for `uu_cat` is empty — the codemod
routed every production site — which is the health metric reading zero at the
smallest scale. The expected-failure infrastructure for upstream suites is not
yet built; it is the first thing a utility with a *deliberate* divergence (a
dropped flag) will need, and `uu_cat` has none. See D4's first-increment note.

## D14 — Confinement now, hermetic mode later

Clock, RNG, PID and readdir ordering are injected through the same policy object
as the mount table, so `--hermetic` can freeze them later with no call-site churn.
Mandatory determinism now would break just's `uuid()`, `datetime()` and
`just_pid()` for a payoff no consumer has asked for.

## D15 — The cwd belongs to a session handle

`commands.rs:190` does `cmd.current_dir(shell.working_dir())` and `:660` guards on
`working_dir().exists()`; a virtual path satisfies neither, and the `current_dir`
line must be **deleted**, not adapted. A session bundles `(mounts, virtual cwd,
env, stdio, clock/RNG, locale)`. Roc's `Env.set_cwd!` becomes `Session.set_cwd!` —
a process-global cwd cannot express `[working-directory(...)]` under `--jobs`,
which is a latent bug in rocjust today.

## D16 — Launcher sets the ceiling; a manifest may only narrow it

A repo-local manifest is attacker-controlled input. The launcher's config defines
the maximum grant, the manifest subsets it, and the default ceiling is the
justfile's own directory tree rw and nothing else. A hostile repo is inert, and
the common case needs no configuration — which is what keeps D29's prompts rare
enough to stay meaningful.

**Corrected — the rule applies at every level, not only the first.** A recursive
invocation (D30) is a *new* invocation, so re-deriving its grant from the
launcher ceiling would let a manifest narrow itself, recurse, and recover the
ceiling. **A sub-invocation's grant is a subset of its invoker's**, never of the
launcher's.

Subset permits equality, and equality is the wrong default. The rule above is
"a justfile gets its own directory tree rw and nothing else"; applying it only
at depth 0 means a justfile in a vendored subdirectory — *more*
attacker-controlled than the root one, not less — runs with the root's
authority. So a sub-invocation's default grant is **its own tree, intersected
with its invoker's grant**, which reads identically at depth 3 and at depth 0.
Plain inheritance was rejected for that reason; requiring every recursion to
declare its grant explicitly was rejected as breaking every existing recursive
justfile until its parent is edited. The cost is real and accepted: a
sub-justfile writing into a shared parent build directory now needs an explicit
grant, and that pattern is common.

**Three places in rocjust reach outside the tree before any sub-invocation
exists, and the rule as stated forbids all of them.** `import` and `mod` resolve
targets by joining and lexically cleaning, so `../shared.just` is expected and
documented; a submodule's recipes run beside *its own* file, i.e. with a working
directory outside the importer's tree. Neither is a D30 frame, so there is no
invoker's grant to intersect with. And justfile discovery walks *upward* to the
filesystem root looking for a candidate — which has to happen before a grant can
be derived at all, so discovery needs its own explicitly stated ceiling rather
than falling under this rule. `set fallback`, which rocjust parses and does not
yet act on, retries in the parent directory and would intersect to empty the day
it is wired up.

**Resolved: the grant is the union of every reachable file's tree, computed up
front.** Imports and modules are written statically in the source, so the loader
resolves the whole graph before execution and derives one grant covering it --
read-write for the root justfile's tree, read-only for each imported tree. The
default ceiling stops being "the justfile's own tree" and becomes "what this
justfile is made of", which is what it always meant. Adding trees lazily as each
import resolves was rejected because a grant that grows during execution cannot
be shown to a user before the run starts; requiring an explicit grant per
outside import was rejected because rocjust documents the shared-justfile
pattern as supported.

**Corrected: most of the union *is* expressible as a mount table.** An earlier
version of this said the table refuses to build because `MountTable::build`
rejects overlap. It rejects overlapping *host directories*; nested *virtual*
mount points with differing access build fine and are tested — `/work` on one
host directory read-write with `/work/vendor` on another read-only, resolved by
longest match. So a `mod` in a subdirectory of the root tree needs no second
mount at all: it is already inside the root's, and its access should be
read-write anyway, since a submodule's recipes run beside its own file.

What is genuinely inexpressible is the *enclosing* case: `import '../shared.just'`
names a tree containing the root's, and mounting both would overlap on the host.
That one needs either per-path access rules or a read-only mount of the common
ancestor with the root's tree nested read-write inside it.

If per-path rules are used, the rule must prefix-match the **landing** path and
re-match at every symlink hop, not the requested path. `..` is not the hazard --
it is resolved lexically before any host path exists -- but a symlink is: brush-vfs
already decides writability at the resolved location precisely because "a symlink
from a writable mount into a read-only one must be governed by where it lands".
Prefix rules also re-open the reason the overlap refusal exists at all, which is
hard links: one mount with two rules means `ln /work/import-ro/secret /work/proj/x`
gives a read-write name to a read-only inode.

**Nothing can resolve the graph before the sandbox exists.** Import and module
resolution is performed by *guest* code, by probing four candidate filenames per
`mod` — the text is static but the graph is a filesystem question. So graph
resolution joins discovery as one launcher act, needing a host-side scanner that
recognises `import`/`import?`/`mod`/`mod?`. The guest's loader must then fail
closed on any file the host did not pre-resolve, which turns a divergence
between the two parsers into an error rather than a silent gap in the grant.

Three run-time widenings the up-front union cannot see, all of which must be
resolved up front or refused under a restrictive policy: `set fallback` — which
*is* implemented, contrary to an earlier note here, and performs a second
unbounded upward walk inside the sandbox; `dotenv` resolution, which also climbs
to the filesystem root; and `[working-directory(...)]`, which is a run-time
expression that may contain a backtick and whose absolute result is passed
straight through.

Discovery -- finding the root justfile in the first place -- is not covered by
this rule and could not be; see D44.

Whether the intersection is computed on virtual or host paths, and before or
after canonicalization, is load-bearing and unstated: after canonicalization a
symlinked subdirectory intersects to empty, and before it grants a host tree the
invoker never had. It must be the pre-canonicalization virtual path, with a
symlinked justfile requiring an explicit grant. D31's per-project `HOME` needs
naming separately — on virtual paths a sub-project silently inherits the
invoker's home, and on host paths it gets none at all.

## D17 — Roc compiles to wasm by default; native is a trusted opt-in

Under an in-process boundary, untrusted Roc runs as native code in the same
address space as the vfs confining it, with the whole Roc compiler in the TCB —
and rocjust documents a live miscompile on the current toolchain (a field added to
an `Ast.Item` payload passes `roc check` and every test, then crashes at runtime).
Wasmtime supplies a real boundary. Native stays available for trusted code as a
chosen trade.

## D18 — Custom host functions; no `wasi:filesystem`

WASI preopens would work — `wasmtime-wasi` is cap-std-backed — but would create a
second filesystem path with its own semantics running parallel to the vfs. Custom
imports bind identically in both tiers: wasm imports in one, direct calls in the
other. One Roc API, two bindings, no drift.

## D19 — Both tiers have identical capability

Tier differs only in speed and TCB. Had native been privileged, every author would
want it and there would be one trust level again — the weak one. Tier is a
launcher decision recorded in the same per-project trust store as mount grants.

## D20 — `/dev` is a synthetic namespace, not a mount

`sys/unix/fs.rs:224` is `const fn try_open_special_file(_) -> None`, so `>/dev/null`
currently falls through to a host device open. Instead: `/dev/null`'s fd is opened
once at startup *before* the sandbox closes and inherited; `/dev/stdin|stdout|stderr`
and `/dev/fd/N` resolve against the session fd table. Process substitution needs
nothing — `interp.rs:1918` already builds it from `std::io::pipe()` at a synthetic
shell fd and never consults the host's `/dev/fd`.

**Corrected, three ways.** (a) `try_open_special_file` runs on the **raw,
unresolved** path *before* `absolute_path` (`shell/fs.rs:186-190`) and must move
after resolution. (b) A `/dev/fd/N` **table miss falls through to the host fd**
(`shell/fs.rs:196-203`); it must be `ENOENT`, and that is the one `/dev` rule that
differs from bash and therefore cannot be identity-exempt. (c) "Every other `/dev`
path is a hard error" applies to **restrictive policies only** — the suite uses
`/dev/full` ×7, which is not in the permitted set, so an unconditional rule fails
gate 1 by construction. "Unimplementable by construction" is therefore too strong
for `/dev/tcp`; it is a policy value.

## D21 — Environment policy has three classes, not two

*Synthesized* from policy, never inherited: `PATH` `HOME` `TMPDIR` `HOSTNAME`
`LANG`/`LC_*` `TZ` `SHELL` `UID`/`EUID`/`GROUPS`. *Passthrough*: `NO_COLOR` `CI`.
*Denied*: everything else, `BASH_FUNC_*` unconditionally. A pass/deny list has
nowhere to put the first group, where every hard case lives.

**Corrected:** `TERM`, `COLUMNS` and `LINES` were moved **out** of passthrough.
D36 strips control sequences from sandboxed output, and passing `TERM` in is how
utilities decide to *emit* them; `COLUMNS`/`LINES` describe a geometry that no
longer exists and appear nowhere in brush's source. `TERM` is synthesized as
`dumb`.

## D22 — [ASSUMPTION] `PATH=/bin`, with `/bin` synthesized from the builtin registry

Proposed and not contested. `ls /bin` lists the utilities, `command -v cat` answers
`/bin/cat`. The cliff is documented rather than hidden: prepending a directory to
`PATH` resolves to nothing, because no path can hold an executable.

## D23 — The shell claims to be bash, and marks itself sandboxed

`BASH`/`BASH_VERSION`/`BASH_VERSINFO` stay pinned to the implemented level: scripts
use them as syntax gates and the syntax genuinely is bash's. `FLATLAND_SANDBOX`
gives code that needs to detect the restricted world an accurate signal instead of
an inference from absence.

## D24 — Sessions reach children over a broker handshake, not inherited fds

`sys/windows.rs` routes `commands` to the stub module and `sys/stubs/commands.rs:67`
errors on any fd to inject; `command-fds` is `cfg(unix)` only. **Windows children
get stdio and nothing else** — even `3>file` fails there today. The child connects
on a unix socket or named pipe and the parent sends the session plus the mount
`Dir` handles via `SCM_RIGHTS` or `DuplicateHandle`, neither of which needs
inheritance. Opens are local `cap-std` afterwards.

**Corrected — the serde blob is not "largely free".** `shell.rs` skips **five**
fields (`error_formatter`, `jobs`, `builtins`, `parser_impl`, `key_bindings`), and
`openfiles.rs:79-96` deserializes every file/pipe/stream variant to `null()`, which
opens `/dev/null` **by host path** (`sys/unix/fs.rs:214`) — inside the lint
allowlist, after the sandbox is supposed to be closed. Every non-stdio session fd
would silently become `/dev/null` in the child.

**Implemented on Unix, compile-checked on Windows**
(`brush-core/src/broker/`, `plans/2026-08-21-broker.md`). The hole it closes was
measured first: with `--mount`, `--closed-world` and `--restrict-builtins` all
set, the bundled `cat` read `/etc/hosts` and `ls /etc` returned 82 entries. All
three transcripts are inverted now, and the third one matters most --
`cat /work/inside.txt` *succeeds*, which is what distinguishes a child that
received the namespace from one that received nothing.

**The milestone is smaller than this entry reads, and the survey is why.**
Redirections already crossed correctly, so nothing here carries stdio. A bundled
child runs one utility rather than a shell, so none of the serde blob is needed
-- which is why the correction above about `openfiles.rs` did not bite. What
crosses is a mount point, an `Access` and a directory handle per mount, plus the
cwd.

**The credential is the pid** (`LOCAL_PEERPID`, `SO_PEERCRED`,
`GetNamedPipeClientProcessId`), so one question is asked on all three platforms.
Nothing checks uid, which makes the rendezvous directory's `0700` mode -- an
owner-only SDDL on Windows -- the only cross-user mechanism rather than a second
layer. The path travels in the environment, not argv: `/proc/PID/environ` is
owner-readable where `/proc/PID/cmdline` is not.

**Two things the entry did not anticipate.** `Mount::host_path` had to become an
`Option`, which turns out to strengthen D3 rather than weaken it -- a child
handed capabilities has no host path to leak even by accident, and
`Vfs::host_path` errors rather than guessing. And `exec` of a bundled dispatch is
refused, because `exec` leaves no parent to serve the handshake; the child would
fail closed correctly but only after its timeout, which is a right answer
delivered as a hang.

**Cost, measured rather than assumed:** about 1.4 ms per bundled dispatch on top
of ~8.8 ms of process spawn (200 dispatches, debug build, medians of seven
runs).

Constraints the broker milestone inherits, each because the obvious
implementation is wrong: peer credentials bind a *connection*, not a session, so
pid registration must precede spawn or the ordering races; a double-connect passes
the credential check identically and fd-passing has no revocation, so acceptance
must be one-shot; and forgery is impossible today **only** because a script cannot
spawn anything, which makes the no-exec invariant load-bearing rather than
incidental — as is its sibling, that the parent never places `--inherited-fds` on
a child's command line (`entry.rs:522-527` turns any integer into a dup'd fd).

## D25 — Parallelism is `spawn` + `wait_any`, not a blocking exec

A single Wasmtime instance has one call in flight and Roc has no async or threads,
so a blocking exec serializes everything. One instance per job is actively wrong:
rocjust's memo table (at most once per invocation, keyed by name *and* arguments)
is shared state, so splitting instances makes a diamond run twice. Designed now
even though the executor lands with `[parallel]`, because retrofitting handles
means re-porting the Roc app.

**Corrected again, and this reverses the entry's premise.** One instance is one
store, so one deadline and one memory budget cover every job in it (D35), and
the guest -- which D17 establishes as untrusted -- legitimately holds both its
own frame handle and its sub-invocation's, so it can act for the broader frame
and ignore D16's narrowing entirely. Per-frame grants inside one untrusted guest
are advisory, not enforced.

**So it is one store per sub-invocation, with the shared memo table owned by the
host.** That makes the narrowing real, and gives each sub-invocation its own
deadline and memory budget so a runaway job stops itself rather than its
siblings. **Corrected: the account of what crosses was wrong.** The claim here was that
only the recipe "ran" set crosses invocations and that the evaluator's memo is
per-evaluation. The `ran` half holds — its key is a qualified name and the list
of bound argument values, plain data. The other half does not. `Evaluator.Memo`
caches `Backtick`, `Shell`, `ReadFile`, `Uuid`, `JustPid`, `Which` and the rest;
it is seeded by `force_assignments!` before anything runs and then threaded
through every planned invocation on the command line and back out of each one.
So it is per-*invocation*, and its key is a tag union over builtin call shapes
rather than flat data.

That forks the design and the fork has to be stated rather than left implicit.
A store is per **invocation**: parallel jobs within one invocation share it, and
therefore share one deadline and one memory budget — one runaway job does trip
its siblings, which is accepted and said out loud rather than claimed otherwise.
A **sub-invocation** gets a fresh store with a fresh memo and a fresh `ran` set,
which is exactly the subprocess semantics D30 is emulating. The host then holds
nothing, and "host-owned memo table" is withdrawn.

Every effect re-resolves its mount table from the session handle, which is what
makes the handle table the boundary for D10's ambient-path model. An earlier
revision of this entry said the opposite of the paragraph above it — that one
instance holds sessions with *different* mount tables — and was left standing
when the store decision changed. Within a store there is one grant and therefore
one mount table; sessions differ in working directory, environment and open
descriptors. The handle design is D43.

## D26 — Links are validated at creation

cap-std blocks *following* escaping symlinks, not creating them. An unvalidated
`ln -s /etc/passwd evil` is inert in the sandbox and fully live on the host the
next time anything tars or copies the workspace — containment ends when the run
does. The check is free because an escaping link can never resolve anyway: **the
safe set and the useful set are the same set**. Also: no cross-mount hardlinks,
and the policy loader rejects mounts whose host directories overlap.

**Why the rewrite is containment and not tidiness.** It was briefly argued here
that an absolute virtual target stored verbatim is harmless -- that `/work/a` on
the host is dangling or unrelated, so no reach is possible. That is wrong, and
wrong in the direction that matters: the virtual and host namespaces share a
*spelling* space. A policy built to look like a normal filesystem mounts exactly
the names the host also has, so with something mounted at `/etc`,
`ln -s /etc/passwd evil` is a valid virtual target that, stored verbatim, points
at the host's real file the moment anything outside the sandbox follows it. The
collision is the common case for a realistic policy, not the edge case. This is
D26's original hazard, unchanged.

**Resolved: an absolute target is rewritten relative to the link, or refused.**
The check had no creation site at all -- `brush-vfs` could not create a link --
so it was unwritten rather than free. Now that it can, the rule is that a link
must mean the same thing inside the sandbox and on the host afterwards, since
containment ends when the run does: `ln -s /work/a/b c` inside `/work` stores
`a/b`. A target in a different mount cannot be expressed relatively and is
refused. The cost, stated because it is observable: `readlink` reports the
stored form, so it prints the rewritten target rather than what was written.
Storing what was written was rejected as reopening the hazard this entry names.

**Three costs, accepted and recorded rather than discovered later.** `uu_ln`
already implements this rewrite as the opt-in `--relative` flag, so making it
mandatory renders `ln -s` and `ln -sr` indistinguishable and fails the upstream
`test_ln` cases that separate them -- see D13 for how such a failure is tracked.
`/dev` is synthetic under D20 and `/bin` is synthesized under D22, so neither is
a mount and `ln -sf /dev/null x`, a common idiom, is refused.

**And the implementation to reuse contains a fail-open that the mandatory form
must not inherit.** `uu_ln`'s `--relative` falls back to storing the *absolute*
target when canonicalization fails. As an opt-in convenience that is reasonable;
as the mandatory containment rule it silently restores the hazard on any error,
so the mandatory form refuses instead. The rewrite is also a canonicalization,
which collapses intermediate symlinks in the target: `ln -s /work/current/bin/x`
with `current -> v2` is frozen to `v2`, and a "current version" indirection is
the main reason absolute links get written. That is silent rather than a
refusal, and it puts `canonicalize` on an attacker-controlled path at every
`ln -s`, which D35's byte cap has to cover. And a link
pointing outside the subtree being copied but inside the mount can be retargeted
by a copy to a different depth -- narrower than it first appears, since relative
links are what *survive* a tree being moved and absolute ones are not, but real.

**Corrected — `ro` is not a property of a `Dir` fd.** It is enforced in
userspace from a field, so anything reaching the filesystem without passing the
vfs's access check writes to a `ro` mount anyway. That is accepted and scoped
rather than fixed; see D6.

## D27 — DoS is in scope, bounded cheaply

Fork bombs are impossible for free. `braceexpansion.rs:44` has no cap on
`(start..=end)`, so `echo {1..1000000000}` is a one-line memory DoS — and a range
cap alone misses the sibling case, since nested braces are a cartesian product
with the same shape; cap the total field count at the collection point. `ulimit`
calls `rlimit::Resource::*.set()`, which is process-global, so under `--jobs` one
session degrades every other: a correctness argument for removal, not only a
security one. `umask` and `tcsetattr` are the same shape (the latter removed by
D36).

## D28 — Per-job output buffering

`openfiles.rs:182` is a real `IsTerminal` syscall consumed by the `test -t`
operator (`extendedtests.rs:132`), `read`, `help` and job control, so naive
buffering flips a POSIX test operator inside every parallel recipe. stdout and
stderr merge into one ordered buffer so just's command echo stays attached to its
output.

**Corrected by D36 — the merge is a *rendering* decision at the host; the two
streams stay separate on the wire.** The compat suite compares stdout and stderr
separately (`runner.rs:400-427`), so a merged buffer on the wire would make gate 1
unsatisfiable. And `is_terminal` is no longer a recorded parent state: under D36
it is the constant `false`.

## D29 — Granted-set trust store; subsets auto-accept, supersets prompt

Path-keyed consent lets a different repo cloned to the same path inherit the grant;
hashing the whole manifest re-prompts on unrelated edits. Storing the *granted set*
and auto-accepting subsets means narrowing never re-asks. Non-interactive runs fail
closed with an error naming the exact `--mount` flag. **No blanket approve-all flag
exists** — every `--yes` becomes a copy-pasted line in every pipeline, on exactly
the machines where the boundary matters most.

## D30 — `just_executable()` returns `/bin/just`, and recursion works

The idiom exists so a recipe can re-invoke just, which a closed world forbids.
The `just` builtin returns a sub-invocation request; `wait_any` surfaces it to the
guest as a variant alongside `Finished`; the guest runs it with a fresh memo table
and calls `complete`. A trampoline, not wasm re-entrancy — but rocjust's main loop
becomes a dispatcher, far cheaper to do *during* the basic-cli port than after.

**Corrected:** the trampoline is unbounded — D2's "fork bombs are impossible" is
true of processes and false here. Depth is capped per invoke chain at **256**,
reusing the bound rocjust already applies to user functions so there is one
number to remember rather than two. A total-invocation cap was considered and
dropped: it would catch breadth, which depth alone does not bound, but adds a
second knob for a case D35's deadline already terminates. Grant derivation is
in D16.

## D31 — `HOME` is a per-project persistent mount

`shell/fs.rs:80` falls back to the host passwd database when `HOME` is unset — a
**fail-open** leak where everything else fails closed. Delete it, do not shadow it.
`/home/user` maps to a per-project directory under the user's state dir so caches
survive and one repo's home is invisible to another; XDG derives as subdirectories.

**Corrected — the rc vector needs the other knob.** `ProfileLoadBehavior::Skip` is
consulted only inside `if self.options.login_shell`
(`shell/initscripts.rs:57-120`); `~/.bashrc` and `~/.brushrc` are governed by
**`RcLoadBehavior`**. And since `/home/user` is persistent and writable, a hostile
repo dropping `~/.bashrc` gets execution on every later run — this converts a
host-scoped persistence vector into a project-scoped one unless rc loading is off.

## D32 — The broker authenticates with kernel peer credentials

`SO_PEERCRED` / `LOCAL_PEERPID` / `GetNamedPipeClientProcessId` report the
connecting PID unforgeably. A nonce in `argv` was rejected: `/proc/PID/cmdline` is
world-readable. PID reuse cannot race it because the parent holds the child handle
— the same thing that lets it `wait`. Cross-user is handled by a `0700` directory
or user-SID DACL.

**Corrected:** macOS gives **pid XOR uid**, not both — `LOCAL_PEERPID` returns a
pid with no uid; `LOCAL_PEERCRED` returns `xucred` with no pid. And creating then
validating the `0700` directory is itself a TOCTOU check on macOS, where there is
no `openat2`, so the rendezvous is protected by cap-std's weaker implementation.

## D33 — Locale is a session fact on three axes

Collation to codepoint order, encoding to UTF-8 unconditionally, messages to
English. Pinning a locale *string* is not portable: `C.UTF-8` does not exist on
macOS and Windows has no POSIX locales. uutils `sort` honours `LC_COLLATE` and
diverges from GNU under multibyte, agreeing only under `C`. brush is already
codepoint-shaped — `patterns.rs:415` compiles globs to `fancy_regex::Regex`. Cost
accepted: no dictionary collation for anyone. Depends on D5 (see there).

## D34 — Signature preservation bounds the codemod

In scope: any rewrite expressible as "read a process-global session". Explicitly
*not* needed: `std::env::var` (the parent sets the child's env),
`std::process::exit` (it is a child), and `std::io::stdout()` (the pipe to the
parent is its stdout). The metric is residual patch-set size per rebase.

**Validated by the spike**, with three conditions it forced: `brush-vfs` must be
**path-based and std-typed** (`uu_ls`'s `PathData` carries `Cow<'a, Path>` and
re-opens by absolute path, so a `Dir`-handle API breaks the rule immediately); the
codemod needs an **inherent-method visitor** as well as a path visitor, and that is
the majority of the work; and `canonicalize` semantics plus xattr/ACL access are
owner decisions, not rewrites.

**Corrected:** the list is **three** transformations, not four — `IsTerminal`
drops out because D36 makes `is_terminal` a constant. And the earlier claim that
this rewrite class was invisible to clippy was wrong (see D3).

## D35 — The quota counts peak resident bytes, sized against free space

Cumulative bytes written is trivial and wrong: `sort` spills to temp files and
codegen rewrites outputs, so a legitimate run trips it. Default
`min(4 GB, 10% of free space)`, a global free-space floor, and a 60-minute
deadline. Generous by design — both catch *unbounded* behaviour, not legitimate
work. Tight defaults with a raise flow were rejected as the fatigue pattern D29
exists to avoid.

**Unbounded host compute is bounded at the call site, not by interruption.**
Neither mechanism below reaches host code that is CPU-bound in Rust: the regex
builtins behind `=~` and `!~`, and `canonicalize` and `FileDigest` on a
pathological input. A `select!` can observe a deadline but cannot stop a thread,
and abandoning one still holds the store's `&mut`. So the bound is a refusal to
start unbounded work -- a backtracking limit on every host regex, as D27 already
requires for `expr`, and a byte cap on digest and canonicalize.

**Resolved: two mechanisms, because neither covers the other.** Epoch
interruption reaches a guest spinning in its own code, at loop backedges and
function entries. It does *not* reach a guest blocked in a host call, and
`wait_any` is one -- so the host puts its own timeout around every call that can
block. Together they cover both shapes; either alone leaves a hole. The guest
stays synchronous, which keeps the host API simple, and the residue that leaves
-- a guest that returns from a call and loops -- is exactly what epochs catch.

**Corrected — the deadline needs a mechanism to reach a guest that never
yields.** A wasm infinite loop is not preemptible and an RSS quota never sees
it. **`epoch_interruption`**: a counter bumped by a tokio interval task, checked
at loop backedges and function entries, around 1% overhead, and per-store
deadlines so D25's parallel jobs each carry their own. `consume_fuel` was
rejected because the limit here is wall-clock, and fuel is a work budget — it
would need an instructions-to-time calibration that drifts with hardware, so a
slow machine trips the deadline later or never. Fuel's determinism buys nothing
until D14's hermetic mode exists, and the two flags are independent, so it can
be added then. Killing the process from outside was rejected because it cannot
unwind and, under D25, takes every sibling job with it.

**Two qualifications this entry got wrong on first writing.** Epochs and fuel
alike *do not* interrupt code blocked in a host call — Wasmtime's own
`Config::epoch_interruption` documentation says so and recommends the async
variant. D25's `wait_any` is a blocking host call, and so is a long read on a
pipe, so the one shape the parallelism design centres on is precisely the shape
epochs do not preempt. The deadline therefore has to be enforced *around* each
host call by the host — `async_support` plus `epoch_deadline_async_yield_and_update`,
or a `select!` on the host side — which is a design, not a flag, and drags in
`async_stack_size` (required to be at least `max_wasm_stack`, so that pin
becomes a startup constraint rather than a free knob).

And "per-store deadlines so D25's parallel jobs each carry their own" is false
under D25: one instance is one store, D25 puts several jobs in one instance, and
D30 runs the sub-invocation there too. One deadline covers all of them, so one
overrunning job trips it for its siblings — the exact failure this entry cites
when rejecting "kill the process from outside". Either accept one deadline per
invocation *chain* and say so, or give each job its own store and solve the
shared memo table another way.

Three Wasmtime settings are pinned rather than left at their defaults.
**`allocation_strategy` is on-demand, not pooling** — pooling reuses memory
slabs across instantiations, so a job could read residue from a previous job's
heap; it may arrive later with an explicit zeroing guarantee and a test for it.
**`wasm_threads` is off** — D25 establishes that Roc has no async or threads, so
shared-memory threading adds a concurrency surface to the boundary and buys
nothing. **`max_wasm_stack` is given an explicit
value**, so a runaway recursion traps inside the guest instead of reaching an
RSS quota that measures the whole host process. Memory has no
`memory_maximum_size` knob under that name: growth is gated by the module's
declared maximum and by `StoreLimitsBuilder::memory_size` through
`Store::limiter`, which is per-*store* and therefore shares the collapse
described above.

## D36 — The sandbox has no terminal

**In plain terms.** A terminal is not a place where text appears; it is a device
that reacts to commands embedded in the text stream. Certain byte sequences are
not printed, they are executed — a program that can write to your terminal can set
your clipboard, read it back into its own input, or change how your keyboard
behaves after it exits, without ever asking whether it is on a terminal. So the
terminal is a **capability**, like the filesystem, and sandboxed code receives a
pipe rather than the device.

This is *not* "make `isatty` return false" — reporting no-terminal while still
handing over the real one fixes nothing. Here `isatty` is false as a *consequence*.

Dropped: escape-sequence pass-through (the host strips control sequences —
`term_detection.rs:83` shows brush already tracks OSC 52 as a capability);
`tcsetattr` and terminal `Settings`; `umask`; terminal handover in job control
(`take_foreground`, `lead_session`), which also forecloses macOS's unconditional
`TIOCSTI`; `/dev/tty`.

**Costs, named specifically:** nothing inside can draw; colour only if the host
renders it; and in `brush-builtins/src/read.rs`, **`read -p` never prints its
prompt** (`:520` gates on `is_terminal`) while **`read -s` and `read -N` lose
their mechanism** (`:565-575` builds `terminal::Settings`). just's `[confirm]`
becomes a host-rendered prompt surfaced as a D30-style `wait_any` variant. The
launcher's own prompt, completion and history are unaffected.

## D37 — The wasm lanes are dropped from CI

`cap-primitives` has no `target_family = "wasm"` backend, and brush is always
native in the host — the Roc *guest* is wasm, the shell never is. Removed the
`wasm32-unknown-unknown` and `wasm32-wasip2` build entries, the `wasm32/wasi-0.2`
test lane, and the dead WASI-runtime step.

`brush-core/src/sys/wasm/` was left in the tree with **no target that linted or
compiled it**, and `sys/wasm/fs.rs` returned `true` unconditionally from
`readable`/`writable`/`executable` — recorded then as a wasm-specific fail-open
that nothing saw.

**Corrected — it was not wasm-specific.** Those methods came from `PathExt`,
which the whole platform layer implemented and which, once executable lookup
moved to `access(2)` and `FileFacts` replaced the `exists_and_is_*` predicates,
had no callers anywhere. Deleting the trait removed the fail-open on every
platform rather than on one. What remained of `sys/wasm/` was a re-export of
`sys/stubs` under another name, so it went too: this decision already settled
that brush is always native, and restoring the module is a revert against that.
Keeping it behind a `cargo check` lane was rejected as paying a CI lane to
compile something nobody builds.

## D38–D40 — Here-documents materialize to a temp file

Three positions in sequence: temp file (D38), reversed to an inline/thread hybrid
(D39), reverted to the temp file (D40) after review measured what the hybrid cost.

The bug: `setup_open_file_with_contents` wrote the payload into a pipe nothing was
reading, so past the pipe's capacity it blocked forever on macOS and Windows while
Linux grew the buffer with `F_SETPIPE_SZ` and failed differently. The hybrid wrote
small payloads inline and threaded beyond — and **truncated across `exec`** (the
write end is `O_CLOEXEC`, so 65536 bytes arrived where bash delivers 270336, with
no error; on Linux a regression from working code), made `read -t 0` racy above the
bound (31% wrong under load), and accumulated one thread per here-doc.

`tempfile::tempfile()` gives an unnamed file — unlinked on Unix,
`FILE_FLAG_DELETE_ON_CLOSE` on Windows — so D38's one open risk was solved by an
existing dependency. Every byte exists before the descriptor is handed over, which
is what `read -t 0` observes and what survives `exec`, and the fd is seekable as
bash's is. **Implemented; suite green at 1799 passing.**

**The lesson:** both reversals were argued from properties nobody had measured,
and D39 was chosen for upstreamability while not being upstreamable. Measuring
cost one session.

## D41 — A Landlock-backed integration test lands in *this* milestone

D3's ban is a regression mechanism, not a proof, and D12 deferred the OS layer to
a later milestone — which left the first milestone with no way to make a positive
claim about completeness.

So the milestone gains a **kernel-enforced completeness check**: the kernel, not a
lint, decides whether anything reached outside the mount roots. This converts "we
found every call site" from a negative claim into an executable one, and it is the
only mechanism in the design that reaches dependency code and, later, `uu_*` code.

Landlock in a *test* is not the OS-enforcement milestone — no policy plumbing, no
shipped behaviour change, no macOS or Windows equivalent needed. Linux-only by
design: that is where the primitive is, and one platform *proving* the routing is
complete is worth more than three platforms asserting it.

## D42 — Absolute symlinks resolve against the virtual root

cap-std rejects **every** absolute symlink, not only escaping ones
(`cap-primitives-4.0.2/src/fs/manually/open.rs:426,473`; Linux inherits it from
`RESOLVE_BENEATH`). Measured: `/usr/local/bin` is **13 of 13** absolute symlinks,
`/usr/bin` 8 of 39, both hard-coded into the harness PATH against a suite invoking
external `true` ×429. Under identity they would degrade to silent
`command not found`.

Rejected: rejecting them and accepting that identity is not transparent.

An absolute symlink target is therefore interpreted as **virtual-absolute and
re-resolved from the virtual root** — `RESOLVE_IN_ROOT` semantics, which is what a
chroot-like namespace should mean, and which keeps `$PATH` working. Relative
targets keep `RESOLVE_BENEATH` behaviour.

The cost, stated because it cuts against D3: resolving symlinks ourselves is
exactly the hardening cap-std was chosen to avoid re-deriving. This is the one
place the design knowingly takes that on, and it is why D41's kernel-enforced
check matters more here than anywhere else.

## D43 — Per-frame handle tables, reinstated after a wrong withdrawal

**This entry was withdrawn and the withdrawal was wrong.** The argument was:
D25 now gives each sub-invocation its own store, so within one store D16 derives
a single grant, every session shares one mount table, and a forged handle
recovers nothing.

Two things break it. **A submodule is not a sub-invocation.** `mod` nests inside
the invoking process, so it lives in the invoker's store — and D16 rejected
equality for exactly this case, because a justfile in a vendored subdirectory is
*more* attacker-controlled than the root one, not less. One grant per store
hands a vendored `mod` the root tree read-write. Per-frame tables were the only
named place a narrower table could live inside a store, so withdrawing them
removed the mechanism and left the requirement.

And **"differs only in working directory and environment" is not what a session
is.** D15 makes it `(mounts, cwd, env, stdio, clock/RNG, locale)`, `/dev/fd/N`
resolves against the session's descriptor table, and a shell carries persistent
descriptors beyond stdio. A descriptor is authority that outlives the check that
minted it, so "recovers nothing" is a claim about mount tables applied to a
bundle that is not only a mount table. The filesystem half of the argument does
hold under one grant; the descriptor half does not.

The store boundary does subsume the frame boundary *across* sub-invocations —
Wasmtime handles cannot cross stores — but that was never the hard case.

Three properties from it survive and now live where they belong: generational
indices within a store's tables, for use-after-free; one table per kind, so a
type confusion is not expressible; and a host-side frame stack for D30's depth
cap and for rejecting a duplicated or out-of-order `complete`. Frame teardown
must reap, or a job spawned in a frame that exits before `wait_any` sees it
leaves a live process holding that frame's mount handles —
`kill_external_commands_on_drop` is mandatory for sandboxed sessions.

**`Caller<'_, T>` does not replace the frame handle.** It identifies the store
the call came from, which is the *parent's* store when the parent's guest calls
`complete` for a sub-invocation that ran elsewhere. With two `just` calls in
flight the guest still has to say which one finished, so the host-minted frame
handle stays.

## D44 — Discovery is a launcher act, bounded by a source-control root

Finding the file to run is not something the sandbox can do, because the sandbox
does not exist yet: the grant is derived from where the file is, and finding it
means probing ancestors. D16 has no answer for it and could not have one.

So discovery happens in the launcher, before any policy, alongside the
launcher's own configuration (D6). Nothing inside the boundary performs the
walk; the sandbox starts with the answer already in hand. A transient read-only
policy for the probe was rejected — a second policy is a second thing to reason
about, and it would exist for one syscall's worth of work.

**The walk is bounded, and the bound is the point.** An unbounded upward search
for a file that does not exist ends at the filesystem root, and a root that
happens to contain the sought file grants everything below it. So the default
walk looks for a **source-control root** — `.git`, `.hg`, `.svn` or `.jj`, as a
file *or* a directory, because git writes `.git` as a file in worktrees and
submodules and a directory-only check silently fails in both. The first marker
found wins, so a nested repository gets its own tree rather than its parent's.

The walk **fails** on reaching `$HOME`, the filesystem root, or a platform
system tree, and **a marker found at or above one of those does not rescue it**.
That case is not hypothetical: a dotfiles repository makes `$HOME` a source
control root, and `$HOME` as a grant is nearly as bad as `/` — which is the
outcome the bound exists to prevent. Anyone whose project *is* their dotfiles
repository has to name a ceiling explicitly.

Reaching a stop with no marker found is also a failure rather than a fallback to
the working directory. The narrow fallback was argued for and rejected: it
cannot produce a large grant, but "no root, no run" is the rule that never needs
explaining, and a directory outside version control is exactly where a
surprising grant would be least noticed.

The strategy is configurable, because this platform is meant to carry more than
rocjust and not every project is a repository. What is not configurable is that
*some* bound applies.

**Corrected — the bound has to be on the answer, not on the walk.** As first
written it constrained only the search, and three routes reach a root justfile
without searching: `--justfile PATH` is taken as-is, so `--justfile ~/justfile`
yields the $HOME grant this entry calls nearly as bad as `/`;
`--working-directory` relocates where recipes run to any absolute path, layered
on *after* the search; and `--init` runs before the search by design. So the
rule is a predicate on the resulting root directory — it must lie strictly below
a marker that is itself strictly below every stop — applied to all four routes
alike.

**First-marker-wins deletes the monorepo root justfile.** A vendored subtree, a
submodule or a linked worktree carries `.git`, so a walk from inside one stops
there and discovery fails where upstream `just` succeeds. The two searches are
therefore separate: walk up for the justfile, and independently for the
outermost marker below the first stop, then require the justfile's directory to
lie within that ceiling.

**The grant must not contain the marker's own directory.** `.git/hooks/*` and
`.git/config` — which carries `core.pager`, `core.fsmonitor` and `!sh -c`
aliases — would otherwise be writable by an untrusted justfile, and git executes
what it finds there at the user's next command. The same reasoning D26 applies
to symlinks, and stronger. The walk knows where the marker is, so it carves it
out when deriving the grant. Note the accidental asymmetry this removes: in a
worktree or submodule the real git directory already lies outside the ceiling,
so only an ordinary clone was exposed.

**The stop list is per platform, canonicalized, and case-folded.** `/tmp`
canonicalizes to `/private/tmp` on macOS and `$TMPDIR` to `/private/var/...`, so
a list containing `/private` or `/var` would kill discovery for every
temp-directory checkout — including this project's own scratch worktrees. The
set is therefore enumerated rather than described: on macOS `/System`,
`/Library`, `/usr`, `/bin`, `/sbin`, `/opt`, `/Applications`, `/Volumes` and
`/private/etc`, but *not* `/private/tmp`; on Linux `/usr`, `/etc`, `/boot`,
`/proc`, `/sys`, `/dev`; on Windows `%SystemRoot%` and the program directories.

And the home directory is resolved through the same helper the shell uses, not
through `HOME`. On Windows `HOME` is normally unset — the shell synthesizes it
from `USERPROFILE` *inside* the boundary — so a launcher reading `HOME` would
find nothing, the stop would silently vanish, and a `git init`'d user profile
would become a valid ceiling. That is the same fail-open class as the two
Windows defects e0b9ea2 fixed.
