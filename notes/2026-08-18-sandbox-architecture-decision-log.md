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
unwritten rather than free.

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

**Revised after the spike:** `uucore` **already ships the abstraction point** —
`safe_traversal.rs`, 1,464 lines of `DirFd` over
`openat`/`fstatat`/`unlinkat`/`mkdirat`/`fchmodat`, described upstream as
"TOCTOU-safe filesystem operations for recursive traversal". Unix-only,
`nix`-based, and adopted only where TOCTOU mattered — `uu_ls` does not use it. So
the upstream ask is not "accept a new abstraction" but "route through the one you
built, and let its root be injectable". Far easier to land, and every utility
migrated upstream is one the codemod stops touching.

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

**The union is not expressible as a mount table, and the read-only half is
wrong for `mod`.** `MountTable::build` refuses overlapping host directories by
contract, and D16's union overlaps by construction: `mod foo` resolves to a
*subdirectory* of the root tree, and `import '../shared.just'` to a tree that
*contains* it. Either way the table refuses to build. So the grant is a set of
per-path access rules resolved by longest matching prefix, not a set of mounts —
the root tree's read-write rule dominating an enclosing read-only import tree.
And read-only is wrong for a submodule at all: its recipes run beside its own
file, so the standard module layout is read-only exactly where it writes.

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

**Corrected:** one instance holds handles to sessions with *different* mount
tables, and the guest supplies the selector — so the handle table is the whole
boundary for D10's ambient-path model, and every effect re-resolves its mount
table from the handle. The handle design itself is D43.

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
a mount and `ln -sf /dev/null x`, a common idiom, is refused. And a link
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

## D43 — Withdrawn: the store boundary is the frame boundary

This entry designed frames because D25 put sub-invocations in one Wasmtime
instance, where a guest could forge a sibling session handle and recover
authority its frame had given up. D25 no longer does that, and the premise went
with it. Within one invocation D16 derives a single grant covering the whole
file graph, so every session in a store shares one mount table and differs only
in working directory and environment — a forged handle recovers nothing. Across
sub-invocations the store *is* the boundary, and Wasmtime handles cannot cross
stores at all.

Three properties from it survive and now live where they belong: generational
indices within a store's tables, for use-after-free; one table per kind, so a
type confusion is not expressible; and a host-side frame stack for D30's depth
cap and for rejecting a duplicated or out-of-order `complete`. Frame teardown
must reap, or a job spawned in a frame that exits before `wait_any` sees it
leaves a live process holding that frame's mount handles —
`kill_external_commands_on_drop` is mandatory for sandboxed sessions.

What does not survive is the host-minted frame handle as every host function's
first argument. `Caller<'_, T>` already identifies the store, and the store
already identifies the frame.

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
