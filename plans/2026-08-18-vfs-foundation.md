# flatland: route brush's filesystem through a vfs

First milestone of the sandbox architecture in
`notes/2026-08-18-sandbox-architecture-decision-log.md`. It builds **only** the
foundation every later decision rests on: a virtual root composed from mounts,
enforced by `cap-std`. No forks, no wasm guest, no broker, no closed world.

**Status: implemented.** See "Where it landed" at the end for what was built,
what was built differently from this plan, and what was not built at all.

**Revised twice on 2026-08-18, after two adversarial reviews.** Round-two
corrections are `C18`–`C25` in the decision log, and one review angle —
enforcement and gate design — was never received because that reviewer died on
an API error.

**First revision note.** The first draft undercounted
the filesystem surface by roughly half, missed a second chokepoint, and
contained two gates that contradicted each other. Corrections are inline;
design-level challenges are in the decision log's `C1`–`C12`.

The milestone exists to answer one question early: **is brush's filesystem
surface small enough to route?** The honest answer after review is "larger than
it first reads, but bounded and concentrated in two places."

## What this milestone does not prove

Stated up front rather than buried, because every gate below reads in the
register of a proof and none of them is one.

**Nothing is enforced at the end of this milestone. Calls are routed.** External
execution is still on, `pathsearch` still resolves host binaries, and ambient
authority is fully intact.

**And routing the *lookup* without routing the *exec* created a confused
deputy, which is worse than leaving both unrouted.** Demonstrated with the
shipped binary under `--mount /:<jail>:ro`, where the namespace's `/bin` holds
exactly one file:

```
$ echo /bin/*        -> /bin/ls          (the mounted script)
$ command -v ls      -> /bin/ls          (resolved through the namespace)
$ ls /               -> Applications ... (the *host's* /bin/ls, listing the host root)
```

The namespace is asked whether `/bin/ls` may be run, answers about the mounted
file, and that approval is then used to execute a different binary: the virtual
path is handed to `std::process::Command::new` as if it were a host path. Under
any chroot-shaped policy *every* command silently runs the host's version
rather than the sandbox's. Two smaller variants: a command name containing `/`
reaches `commands.rs:421` with no predicate at all, and `hash -p` writes an
unchecked host path into the location cache that later lookups return without
revalidating.

The fix belongs to the execution milestone, and it is not "add a check": the
resolved candidate has to be turned into a host path *by the mount table*, or
better into an already-open descriptor. Recorded here because the intermediate
state is a trap — a reader who sees executable lookup going through the
namespace will reasonably assume execution does too.

| Proven | Not proven |
|---|---|
| ~80 production fs sites compile against a vfs facade | that `ro` is enforced — it is a userspace field, not a property of a `Dir` fd (open decision 1) |
| **On Linux, that nothing escaped the mount roots during a compat subset** — the kernel says so, not a lint (gate 8) | the same on macOS and Windows, where no equivalent primitive exists |
| The identity policy does not regress the compat suite on 2 of 4 lanes | that dependencies do not bypass the vfs (open decision 2) |
| A restrictive-policy subset behaves as specified (gate 2) | that the shipped closed-world configuration works — external execution is still on |
| `brush-vfs` unit resolution, including `RESOLVE_IN_ROOT` | |

The ratio worth internalising: **under identity every rejection branch is dead
code**, so the 2212-case suite exercises the *acceptance* half of the vfs and none
of the *rejection* half. Gates 2 and 4 are the entire rejection story and amount to
a few dozen cases. Gate 8 is the only one that makes a **positive** claim; every
other gate here asserts an absence.

## What is already true

- **Two chokepoints, not one.** `Shell::open_file`
  (`brush-core/src/shell/fs.rs:182`) handles every redirection *and* `source`.
  The second is `sys::fs::PathExt` (`brush-core/src/sys/fs.rs:6-37`), which
  backs **10 of the 18** file predicates of `test`/`[[ ]]`
  (`extendedtests.rs:82-175`, `:563-612`) across ~21 call sites. Only
  `readable`/`writable`/`executable` are `nix::unistd::access`
  (`sys/unix/fs.rs:17,21,25`); the rest go through `path.metadata()`. **The other
  8 predicates use inherent `Path` methods PathExt does not declare** and need
  the separate inherent-method rewrite — see `C20`.
- **A platform abstraction already exists**, `brush-core/src/sys/`. It is a
  *portability* seam that must become a *policy* seam. Note it is thinner than
  it looks: only `fs` has a real variant on all four platforms; `process` has
  neither a unix nor a windows variant (both re-export `tokio_process`), and
  `fd` has no windows variant.
- **`brush-parser` production source has zero filesystem sites.** The four
  earlier attributed to it were tests, a bench, an example, and one
  `std::io::Read::read_to_string`. Nothing to route there.
- **Reedline, not rustyline** (`brush-interactive/Cargo.toml:36`), and history
  is *already* routed through brush-core —
  `brush-interactive/src/reedline/history.rs:147` calls `shell.save_history()`.
  Good news the first draft did not know it had.
- **Lint infrastructure is in place**: the workspace denies `clippy::all`,
  `pedantic`, `nursery`, `cargo`, and a `clippy.toml` exists.
- **CI covers ubuntu-24.04, macos-latest, windows-2025, `wasm32-unknown-unknown`
  and a `wasm32-wasip2` test lane under wasmtime**, plus an
  `x86_64-pc-windows-gnu` cross build.

What is **not** true, and must be fixed:

- `absolute_path` (`shell/fs.rs:166`) only joins — no normalization, no `..`
  rejection.
- `home_dir` (`shell/fs.rs:80`) **fails open** to the host passwd database.
- `try_open_special_file` runs on the **raw, unresolved** path *before*
  `absolute_path` (`shell/fs.rs:186-190`), and on Unix is `const fn … None`
  (`sys/unix/fs.rs:224`), so `>/dev/null` reaches a host device.
- `/dev/fd/N` **falls through to the host fd** on a shell-fd-table miss
  (`shell/fs.rs:196-203`).
- **Three traversals, not one**: `patterns.rs:309` (glob), `pathsearch.rs:65`
  (PATH scan), `sys/unix/fd.rs:40` (`/proc/self/fd`, or `/dev/fd` on macOS/BSD).
- **`brush-builtins` and `brush-interactive` are not fs-free**: `cd.rs:81` and
  `pwd.rs:33` call `canonicalize` (`cd -P`, `pwd -P`), `command.rs:100` and
  `type_.rs:188` use `PathExt::executable`, `completion.rs:99` calls `is_dir`,
  `highlighting.rs:390` calls `exists`.

## Resolved: the wasm lane is dropped

`cap-primitives` has two backends, `rustix` and `windows`; there is no
`target_family = "wasm"` support. Rather than write a second backend or weaken
the no-divergence claim, **both wasm lanes are removed from CI**. brush is
always native in the host under this architecture — the Roc *guest* is wasm
(D17), the shell never is — so a wasm build of brush served no purpose here.

Done: `.github/workflows/ci.yaml` loses the `wasm32-unknown-unknown` and
`wasm32-wasip2` build-matrix entries, the `wasm32/wasi-0.2` test lane, and the
now-dead WASI-runtime install step (36 lines). `brush-core/src/sys/wasm/`
remains in the tree and compiles for anyone who wants it; it is simply no longer
gated on.

## Resolved: absolute symlinks resolve against the virtual root

cap-std rejects **every** absolute symlink, not just escaping ones
(`cap-primitives-4.0.2/src/fs/manually/open.rs:426,473`; Linux inherits it from
`RESOLVE_BENEATH`). Measured on a dev host: `/usr/local/bin` is **13 of 13**
absolute symlinks, `/usr/bin` 8 of 39, both hard-coded into the harness's PATH
(`brush-test-harness/src/config.rs:41-47`) against a suite invoking external
`true` ×429 and `cat` ×87. Under identity they would degrade to silent
`command not found` — a broad, quiet gate 1 failure.

Per D42, `brush-vfs` implements `RESOLVE_IN_ROOT` semantics itself: an absolute
target is virtual-absolute and re-resolved from the virtual root; relative targets
keep `RESOLVE_BENEATH`. Concretely this means the crate does its own symlink
traversal rather than delegating wholly to `cap-std`, which requires:

- A **symlink-resolution loop with a depth bound** (`ELOOP` past ~40 hops), since
  cap-std's own bound no longer covers the absolute case.
- Re-entering resolution at the *virtual* root on each absolute hop, with each
  hop re-checked against the mount table — a target may cross mounts.
- **The heaviest test and fuzz coverage in the crate.** This is the one place the
  design knowingly re-derives hardening cap-std was chosen to provide, so it is
  where gate 6's fuzzing and gate 9's Landlock check earn their keep.

**Measure absolute-symlink density on the actual runner images before trusting
the effort estimate.**

## Step 0 — five upstream bugs, offered to `reubeno/brush` first

1. `sys/windows/fs.rs:151` — `path.ends_with("dev/null") && path.is_absolute()`
   uses `Path::ends_with`, which matches trailing **components**, so a repo
   containing `dev/null` opens `NUL`. `interp.rs:1668` pre-absolutizes the
   target, so `>dev/null` in any repo reaches it.
2. `braceexpansion.rs:44` — no cap on `(start..=end)`. The range is a lazy
   iterator, so exhaustion happens at collection, and a range cap alone misses
   the sibling case: nested braces are a cartesian product with the same shape.
   **Cap the total field count at the collection point.**
3. `sys/stubs/commands.rs:67` — `inject_fds` errors on any fd, and that stub
   *is* the Windows implementation. The fix is
   `PROC_THREAD_ATTRIBUTE_HANDLE_LIST` inheritance. Not on this milestone's
   critical path, but it is a real missing feature and would also give D24 a
   simpler transport.
5. **`exec` carrying an injected descriptor panics a tokio I/O worker** with
   "Bad file descriptor", on every invocation including a 6-byte here-doc. Found
   by the new `exec` regression test; independent of here-documents (`C24`).
4. `interp.rs` — here-doc bodies were written into an anonymous pipe
   with no reader, and the `F_SETPIPE_SZ` that makes that safe is Linux/Android
   only. A here-doc past the pipe buffer **hangs forever on macOS and Windows**;
   on Linux one past `pipe-max-size` errors instead. Same script, two failures.
   See `C13` — this is the cheapest denial of service in the tree and D35's
   quota and deadline both miss it. **Fixed per D40** — materialized to a
   `tempfile::tempfile()`, as bash does. D39's inline/thread hybrid was reverted
   after review showed it truncated payloads across `exec` and made `read -t 0`
   racy under load. Tests are now sized past 1 MiB so the Linux lane guards it,
   plus `exec` and `read -t 0` cases. Suite green at 1799 passing, 0 failing.

## The change

1. **`brush-vfs`, a new workspace crate — with a path-based, std-typed API.**
   It exposes `vfs::open(path)`, `vfs::metadata(path)`, `vfs::read_dir(path)`
   taking paths and returning **`std::fs::File` / `std::fs::Metadata`**;
   `cap_std::fs::Dir` handles are internal and never escape. This is not a
   stylistic call — see `notes/2026-08-18-codemod-spike.md`: `uu_ls`'s
   `PathData` carries `Cow<'a, Path>` and re-opens by absolute path throughout,
   so a `Dir`-handle API would force a restructure and break D34's rule
   immediately. It is sound because confinement comes from *resolution*, not
   from the handle type — `cap_std::fs::File::into_std()` hands back a plain
   descriptor that carries no ambient authority. Virtual path grammar
   (`/`-separated; no drive letters, backslashes, reserved device names,
   alternate data streams, trailing dots or spaces; NFC-normalized;
   case-folding collisions rejected at mount time), mount table, `cap-std`
   handles. Host paths are constructed here and never accepted from a caller.
2. **The mount table is `Arc<MountTable>`.** `Shell` is cloned at five sites —
   background job (`interp.rs:282`), subshell (`:687`), coproc (`:754`), process
   substitution (`:1926`) and command substitution (`commands.rs:759`). Function
   calls do *not* clone (`commands.rs:726-730`). A `Dir`-per-mount `Clone` would `dup(2)` per mount
   per subshell and exhaust the fd table in a deep pipeline. The loader rejects
   overlapping host directories (D26).
3. **An `identity` policy** mounting host `/` rw. A policy *value*, not a build
   configuration.
4. **A policy surface, and one restrictive-policy test — before step 5.**
   There is no way to select a non-identity policy from outside the process:
   `brush-shell/src/args.rs` has no `--mount`/`--policy` flag (`C32`). Without one
   the shell-level escape suite cannot be written, and every later step is
   validated only under identity, where **every rejection branch is dead code**.
   Land the flag (it can ride `--config`, `brush-shell/src/config.rs`) plus one
   end-to-end restrictive case, so the steps that follow have an observable
   rejection to be written against.

5. **`absolute_path` normalizes, rejects, and becomes fallible.** This is a
   wider refactor than it reads: it is `pub`, and several callers are in
   `bool`-returning predicate positions (`type_.rs:188`, `completion.rs:1217`)
   with no error channel. Each needs a deliberate call about what bash does.
6. **Route `PathExt` through the session** — the second chokepoint. The required
   primitive is `cap_fs_ext::DirExt::access`, **not** `Metadata::mode()`
   reasoning: mode bits flip `test -w` for root and under ACLs, and CI runs as
   root in containers.
7. **Route the open sites**: `open_file`, `set_working_dir` (`shell/fs.rs:24`),
   `interp.rs:1659`/`:1683`/`:1882`, `shell/execution.rs:31` **and** `:85` (the
   `source_if_exists` gate and the open must not disagree), `history.rs:185`,
   `shell/history.rs:15`, `commands.rs:660`, `completion.rs:1217`,
   `brush-shell/src/config.rs`, `entry.rs`, and the `brush-builtins` /
   `brush-interactive` sites listed above. Delete the `home_dir` passwd
   fallback.
8. **Route the three traversals**: `patterns.rs`, `pathsearch.rs`, and
   `sys/unix/fd.rs` — the last needs an explicit exemption, since it is process
   introspection rather than namespace access and reads `/dev/fd` on macOS.
9. **`/dev` handling, scoped by policy.** Move `try_open_special_file` to
   **after** virtual-path resolution. Make a `/dev/fd/N` table miss `ENOENT`
   rather than a host fallthrough. Open `/dev/null` once at startup and hold the
   fd. **The "every other `/dev` path is a hard error" rule applies to
   restrictive policies only** — under identity it must not fire, or `>/dev/urandom`,
   `</dev/zero` and `test -c /dev/tty` break and Gate 1 fails by construction.
   That is still a policy value, not a cfg. On wasm there is no `/dev/null` fd
   to hold (`sys/stubs/fs.rs:30`).
10. **The ban — constructors only, with a positive control.** Ban the
    *constructors* (`std::fs::File::open`, `OpenOptions::open`,
    `DirBuilder::create`, the 19 stable free functions), `std::env::{current_dir,
    home_dir, current_exe}`, the inherent `Path` I/O methods, `nix::unistd::access`
    and the rest of the `nix` fs surface. **Ban no types** — step 1's facade
    returns `std::fs::File`/`Metadata`, and there are 31 type mentions in the
    workspace today, so banning types makes `brush-vfs`'s whole API an `#[allow]`
    (`C28`). Types carry no authority; the functions that mint them do.

    Three things make the ban real rather than decorative (`C27`, both verified):
    a typo'd or renamed entry warns and **exits 0**, so the clippy invocation
    needs `-D warnings`; a member-crate `clippy.toml` shadows the root outright,
    so CI must assert exactly one exists in the tree; and there must be a
    **positive control** — a fixture using every banned path once, asserted to
    *fail*. Without it the ban has no evidence it is switched on.

    Consider a dylint lint matching def-path *prefixes* instead (`C29`): strictly
    more complete, and maintenance-free across Rust and `nix` releases.

11. **A Landlock-backed completeness test (Linux).** Per D41. A test binary that
    applies a Landlock ruleset restricting the process to the mount roots, then
    runs a designated compat subset under a restrictive policy. Any filesystem
    access that did not route through the vfs is killed by the kernel rather than
    missed by a lint. Needs `landlock` (or a raw `landlock_create_ruleset`
    syscall wrapper) as a Linux-only dev-dependency, an ABI-version check so
    older kernels skip rather than fail, and a CI lane pinned to a runner image
    with Landlock ABI ≥ 3. No policy plumbing and no shipped behaviour change —
    this is a test, not the OS-enforcement milestone.

12. **A `Session` type** holding `(Arc<MountTable>, virtual cwd)`. It must be
    `#[serde(skip)]` on `Shell` with a reconstitution path, since a `Dir` is an
    `OwnedFd` and `brush-shell/src/entry.rs:451` deserializes a whole shell from
    disk today.

## What stays behind, deliberately

The forks and the codemod; the closed world; wasm, the broker, `spawn`/`wait_any`,
the approval store, quotas, link validation, and the locale/tty session facts;
the default-deny builtin allowlist. Each depends on this foundation.

## Gates

Each gate names its falsifier. A gate without one is a note.

1. **Compat suite green under the identity policy.** *Fails if:* any case
   regresses, or a `known_failure: true` case starts passing —
   `brush-test-harness/src/runner.rs:249-256` counts that as a failure, and 379
   cases carry the flag. This plan authorizes the resulting yaml churn.
   **Coverage caveat to state, not hide:** the suite does not run on Windows at
   all (`xtask/src/test.rs:30-31`), and only the linux-x86_64 and macOS lanes
   install a recent bash. And under identity *every rejection branch in the vfs
   is dead code*, so this gate covers the acceptance half only.

2. **A compat subset green under a `strict` policy.** Rooted at the harness temp
   dir, in `TestMode::Expectation` against committed expectations, using the step
   4 flag via `additional_test_args` (`testcase.rs:55`). *Fails if:* a rejected
   path produces the wrong exit-status class — notably `[[ -e ../../etc/passwd ]]`
   must be **false**, not an error (`C34`). Without this gate the shipped
   configuration is never the tested one (`C33`).

3. **The ban is on, and proven on.** *Fails if:* the positive-control fixture
   compiles clean; if `cargo clippy` is invoked without `-D warnings`; or if more
   than one `clippy.toml` exists in the tree. Run on `{linux, macos, windows}` —
   already free in the `check` job — plus `x86_64-linux-android`, the only cross
   target adding meaningful `cfg` coverage (~10 blocks). "Every target in the
   matrix" was dropped: it means clippy on 9 cross jobs, roughly doubling the CI
   bill for ~18 blocks, and it named `sys/wasm`, which D37 left with no target at
   all. **Decide separately whether to delete `sys/wasm`** — note
   `sys/wasm/fs.rs:6-16` returns `true` unconditionally from
   `readable`/`writable`/`executable`, a fail-open nothing currently sees.

4. **Two escape suites.** A `brush-vfs` unit suite proving resolution, *and* a
   shell-level suite in `tests/cases/brush` (`TestMode::Expectation` — **not**
   `cases/compat`, whose oracle is bash and will disagree) exercising the step 4
   policy flag. *Fails if:* any case carries `skip`, `ignore_exit_status` or
   `incompatible_os` — the runner counts skipped as success
   (`runner.rs:240-246`), so those are the vacuity levers (`S4`). Cases: `..`
   traversal, absolute paths, symlink-out, symlink-to-`../..`, `/work/../work/x`
   (accepted), and a **`/dev/fd/N` table miss returning `ENOENT`**. Grammar cases
   (`CON`, `x.txt:stream`, trailing dot/space, `C:\…`) are *unit* tests and run
   everywhere. Assert on error *kind* in the unit suite only; a shell sees a
   message and a status.

5. **`>/dev/null` works on all platforms** with no `/dev` mount. *Fails if:* the
   byte behaviour of `2>&1`, `<(cmd)` or `exec 3>file` changes — asserted with
   `expected_*` in `cases/brush`, which does run on Windows, rather than "as on
   `main`", which is satisfied by a shared failure. Note `exec` with an injected
   fd panics a tokio worker on every platform today (`C24`), so that path is
   asserted on stdout with stderr ignored until it is fixed.

6. **Fuzzing that actually runs.** *Fails if:* `cargo fuzz run fuzz_vfs_resolve`
   crashes within its budget. Requires a **nightly scheduled workflow, a
   committed corpus, and crash artifacts uploaded** — there is **no fuzzing in CI
   today** (verified: zero references in `.github/`, `xtask/`, `scripts/`), so a
   target alone compiles under `--all-targets` and never executes (`C31`, `S6`).
   If the workflow is not built, **drop this gate** rather than let it read as
   coverage.

7. **Performance budget that can fail.** *Fails if:* any tracked benchmark
   regresses past a stated threshold. Requires adding a threshold flag and a
   non-zero exit to `scripts/compare-benchmark-results.py`, which has neither, and
   an **absolute committed baseline** rather than a diff against `main` — once
   the vfs merges, `main` is the vfs and the ratchet evaporates. Track
   `instantiate_shell`, `clone_shell_object`, a new glob bench, **and a
   `find_first_executable_in_path` bench** — one `access` per PATH entry per
   command is the largest predicted regression and is currently unmeasured.
   `benches/shell.rs:6` is `#[cfg(unix)]` on an ubuntu-only job, so either extend
   to macOS or state that the macOS and Windows cost is accepted unmeasured.

8. **The Landlock check passes on Linux.** *Fails if:* the kernel kills the test
   process — meaning some access did not route through the vfs. This is the only
   gate that makes a **positive** completeness claim; every other gate here
   asserts an absence. Skips cleanly on kernels below Landlock ABI 3, and *fails*
   rather than skips if the CI lane's kernel regresses below it, so the check
   cannot quietly stop running.

9. **Dependency bans.** *Fails if:* `walkdir`, `xattr`, `filetime`, `fs_extra` or
   `notify` enter the tree (`C4`). Populate `deny.toml`'s `[bans] deny`, which
   already runs in CI at `ci.yaml:411`. This replaces the old `cargo tree` gate,
   which was a tautology — `brush-vfs` appears because it is a path dependency,
   routed or not. Also assert `brush-vfs` declares an **empty `[features]`
   table**, which is what "no features that alter resolution" actually means.

## Risk

**High**, and concentrated in five places.

**The identity policy is not a behavioural no-op.** `..` is fine —
cap-primitives pops a retained parent stack and rejects only escapes
(`cap-primitives-4.0.2/src/fs/manually/open.rs:184-207`), so `/work/../work/x`
resolves normally as gate 4 assumes. The real exposure is **absolute symlinks**,
which cap-std rejects unconditionally, and which are pervasive on `$PATH` — see
the pre-step-1 decision above. The macOS `/var → private/var` case is *relative*
and therefore harmless; the first draft named it and missed the class that
matters.

**Step 5 is a semantic trap, which is why step 4 now precedes it.** 21 of
`absolute_path`'s 34 call sites are in `extendedtests.rs:81-174, 569-604` and
already return `Result<bool, Error>`, so `?` compiles instantly and is **wrong**:
bash's `[[ -e ../../etc/passwd ]]` is *false*, not an error. Under identity the
branch never fires and no gate sees it (`C34`). Landing the policy surface first
gives every predicate site an observable behaviour to be written against.

**`absolute_path` becoming fallible** is a four-crate refactor with judgement
calls in predicate positions, and it is not cheaply revertible once merged.
Neither is the `PathExt` rewrite.

**Chasing 2212 differential cases back to green** across three platforms after
path semantics move is genuinely unbounded work.

**The codemod spike has been run** (`notes/2026-08-18-codemod-spike.md`) and
D34's hypothesis survives: 12 sites in `uu_cat`, 132 in `uu_ls`, all
signature-preserving *given* the path-based std-typed API now specified in step
1. Two carve-outs remain owner decisions rather than rewrites — `canonicalize`
semantics under a virtual root, and `uucore::fsxattr::has_acl`, for which
cap-std has no equivalent. The largest remaining unknown moved from "does the
architecture work" to "how long does routing take".

Estimate: **six to ten focused weeks** — roughly 2 weeks for `brush-vfs`, 3
weeks routing (most of it in the sites the first draft did not name), 1 week on
`patterns.rs`, and 2–4 unbounded weeks returning the compat suite to green.

**Rollback:** the crate and the policy are revertible; the `absolute_path`
signature and `PathExt` rewrites are not. Decision rule to agree in advance —
if Gate 1 cannot be held within two weeks of the routing landing, stop and
reconsider the architecture rather than continuing to chase cases. There is also
no story yet for carrying this diff across the next `reubeno/brush` import; D13
has a maintenance model for the *uutils* forks and none for the *brush* fork,
which is the one about to become invasive.

## Where it landed

Written after the fact. The plan above is left as written so the difference is
visible.

### Built as planned

Steps 1–4, 6–9, 11 and 12. `brush-vfs` with its path grammar, mount table and
`Session`; the `--mount VIRTUAL:HOST[:ro|:rw]` flag and the identity policy;
every open, predicate, traversal and executable lookup in `brush-core`,
`brush-builtins` and `brush-interactive` routed through the namespace; the
`clippy.toml` ban with `cargo xtask check ban` as its positive control; and the
Landlock completeness test.

Gates 1–5, 8 and 9 exist and can fail. Gates 6 and 7 were built rather than
dropped: `.github/workflows/fuzz.yaml` runs `fuzz_vfs_resolve` nightly against
a committed corpus, and `compare-benchmark-results.py --baseline` enforces
absolute ceilings committed in `brush-shell/benches/baseline.json`.

### Built differently

**Step 5 — `absolute_path` was not made fallible.** The property it existed to
provide is that no host-shaped path reaches the filesystem unchecked. That is
provided instead by routing each call site through the namespace, which rejects
at resolution. Twenty-one of the thirty-four call sites are `test` predicates
that must answer *false* rather than error, so `?` would have been wrong at
most of them anyway. `absolute_path` still returns a `PathBuf`; nothing
consumes one without asking the namespace about it.

**Step 9 — `try_open_special_file` did not move after path resolution.** It
became the lexical predicate `is_null_device_path` and still runs first,
because `absolute_path` mangles `/dev/null` on Windows, where it is not an
absolute path, before a later check would ever see it. The hazard that motivated
moving it — a repository containing `dev/null` matching a trailing-component
comparison — was fixed at the comparison instead.

**Step 9 — the "every other `/dev` path is a hard error" rule was not written.**
It would be dead code: `/dev` is not mounted under a restrictive policy, so the
namespace already answers `ENOENT`, and if an operator mounts something there
the namespace's answer is the correct one.

**Gate 3 — the ban is not run for `x86_64-linux-android`.** It runs on Linux,
macOS and Windows, which is the existing `check` matrix and free.

**Gate 9 — all five banned crates were already in the tree.** They arrive
through the bundled uutils coreutils, and `xattr` through `uucore`, which
`brush-builtins` itself depends on. Each direct parent is named as a `wrappers`
entry rather than the feature being exempted, so the list is an inventory of
what is knowingly unrouted and a *new* coreutil reaching for one still fails.

### Not built

The owner decisions listed at the top of the decision log, including whether to
delete `sys/wasm`. Everything under "What stays behind, deliberately".
