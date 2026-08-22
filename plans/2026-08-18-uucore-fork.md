# seahaven: fork and route `uucore`

Second increment of D4, and the one the first increment ran into. Five leaf
utilities (`cat`, `head`, `wc`, `tac`, `nl`) are forked and routed;
`cargo xtask vendor-fork` automates the leaf case. `uucore` is not a leaf, and
the attempt to run the same tool at it stopped on `couldn't read build.rs`.

**Status: implemented.** All nine steps landed; see "Where it landed" at the
end for what matched the plan, what diverged, and what remains. The one open
decision — the directory capability — was settled as option (a) before any of
it was written.

**Revised once, after an independent review.** The first draft's measurements
held up; four of its conclusions did not. It counted upstream's own test code as
production and so claimed ~69 sites to route when the real figure is **24**; it
proposed a locale fix that would have regressed 77 utilities while writing a
gate that could not detect it; three of its eight gates could not fail or could
not be run; and it named `safe_traversal` as the only hand-routing target when
`safe_copy` and `fs` are the same shape. Corrections are inline and marked
`R1`–`R12`. The uncorrected draft is in git history at `2e21aeb`.

## Why this is not another `vendor-fork` run

The motivation is unchanged and is worth restating, because the revision shrank
almost everything else: `uu_cksum` routes to **0** sites of its own (verified —
`codemod --check` on `uu_cksum-0.10.0/src` reports `0 routed, 0 unrouted`)
because every byte it reads goes through `uucore::checksum`. Utilities like that
are unconfined today and this milestone is what confines them.

### 1. The build script embeds locales by scanning a directory that will not exist

`vendor-fork` does not copy `build.rs`; that is a two-line fix and is *not* the
problem. The problem is what the build script does once copied.

uucore compiles `.ftl` locale files into the binary
(`locale.rs:64` — `include!(concat!(env!("OUT_DIR"), "/embedded_locales.rs"))`).
For a crates.io build, `build.rs`'s `embed_static_utility_locales` walks the
**sibling registry directory**, matching entries shaped `uu_<util>-<version>`
via `split_once('-')`, and embeds each one's `locales/en-US.ftl`.

Under `forks/uucore`, the siblings are `forks/uu_cat`, `forks/uu_head`, … —
**no `-version` suffix, so `split_once('-')` matches nothing and no per-utility
locale is embedded.**

The failure is silent, not a build error. The crate's own `locales/en-US.ftl`
still loads (it is under `manifest_dir`) and contains `common-error`
(`locales/en-US.ftl:5`), so `create_english_bundle_from_embedded`
(`locale.rs:318`) returns `Ok` on the partial bundle, so `setup_localization`
succeeds — and every utility-specific `translate!` renders its **raw Fluent
key** instead of a message, with a correct exit code.

This also indicts the present state: today's `cat` messages come from a **stale
`uu_cat-0.10.0` registry copy of a crate we no longer depend on.**

> **`R1` — the first draft's fix was worse than the bug.** It proposed replacing
> the registry scan with one over `../uu_*/locales`, "the fork layout it will
> actually live in." `forks/` contains **five** directories; `coreutils.all`
> pulls **82** utilities. That fix embeds locales for 5 and silently drops 77 —
> the same failure mode at 15× the blast radius. Step 4 is redesigned, and the
> gate that was supposed to catch this is rewritten, because it asserted on
> `cat` — one of the five the broken fix would still have covered.

### 2. The codemod rewrites `#[cfg(test)]` code, and here that matters

Dry run over `uucore/src/lib`: **119 sites routed, 19 unrouted.**

All 19 unrouted are genuine D34 carve-outs (`create_dir`, `hard_link`, and one
`set_permissions` at `benchmark.rs:448` — `R2`, the first draft said all 19 were
the first two), and **all 19 sit inside `#[cfg(test)]` modules or the
un-compiled `benchmark` feature. Production carve-outs: zero.**

The bad news is the mirror image: most of the 119 *routed* sites are in those
same test modules. D13 makes the upstream suites a health metric. Routing
upstream's own tests through a facade that **fails closed with no session
installed** breaks them, and the failures would be read as divergences rather
than as codemod damage.

The five existing forks escaped this by luck: `uu_wc` has no test module, and in
the other four every `ambient::` call sits above the `#[cfg(test)]` line
(`cat.rs:385,441` vs 720; `head.rs:471` vs 545; `tac.rs:376` vs 457;
`nl.rs:256` vs 473).

**The codemod must skip `#[cfg(test)]` items.**

### 3. What actually routes — production only

> **`R3` — the first draft's table counted test code as production.** It read
> the codemod's per-file totals straight out of the dry run and reported ~69
> sites to route. Splitting each file at its `#[cfg(test)]` boundary gives the
> real figure: **24**. Two of the seven "route" modules route *nothing*.

Under `coreutils.all`, uucore resolves with **76** features (`R4` — the first
draft said 69) and `uptime`, `proc-info`, `selinux`, `smack`,
`feat_systemd_logind`, `benchmark`, `tty` and `utmpx` are **not** among them.
`procfs` **is** enabled, pulled by `parser-size` — another `/proc` reader in the
graph the first draft did not notice.

| module | feature | prod | test | disposition |
|---|---|---|---|---|
| `features/fs.rs` | `fs` | **13** | 1 | route — but see §5 |
| `features/backup_control.rs` | `backup-control` | **4** | 13 | route |
| `features/perms.rs` | `perms` | **3** | 0 | route — but see §5 |
| `features/checksum/{compute,validate}.rs` | `checksum` | **3** | 0 | route |
| `features/safe_traversal.rs` | `safe-traversal` | **1** | 21 | route + §4 |
| `features/fsxattr.rs` | `fsxattr` | **0** | 7 | *nothing to route* |
| `features/safe_copy.rs` | `safe-copy` | **0** | 8 | *nothing to route* — and see §4 |
| `features/fsext.rs` | `fsext` | 3 | 0 | **leave** — `/etc/mtab`, `/proc/self/mountinfo` |
| `mods/locale.rs` | always on | 5 | 11 | **leave** — the catalog beside the executable |
| `mods/os.rs` | always on | 2 | 0 | **leave** — `/proc/sys/kernel/osrelease` |
| `features/{benchmark,proc_info,selinux,smack,systemd_logind,uptime}.rs` | all off | — | — | **leave** — not compiled (29 sites, `R5`) |

The three "leave" modules that compile are the reason a directory-wide run is
wrong. `fsext` reads the mount table; routing it makes `df` fail.
`mods/locale.rs` locates the shell's own message catalog relative to
`current_exe`; routing it makes the catalog unreachable under a restrictive
policy, and the utility would then fail to render the error explaining why. Both
are host introspection and self-location, not namespace access — the same class
as `sys/unix/fd.rs`'s exemption in the foundation milestone.

### 4. The codemod cannot see most of uucore's real filesystem surface

> **`R6` — the first draft named `safe_traversal` as the only hand-routing
> target. It is one of three, and the smallest.** The `syn` codemod matches
> `std::fs`; uucore's actual open paths are `nix`, `rustix` and `libc`.

**`safe_traversal.rs` (`nix`)** — two ambient entry points, and the analysis
here survives review intact:
- `DirFd::open(path, symlink_behavior)` — line 143, `nix::fcntl::open`
- `create_dir_all_safe(path, mode)` — line 616

Every other `DirFd` method (`open_subdir`, `stat_at`, `metadata_at`, `read_dir`,
`unlink_at`, `chown_at`, `chmod_at`, `mkdir_at`, `open_file_at`) is `*at`-relative
and already confined **to whatever root those two produced**. D13 predicted this
shape and the prediction holds.

**`safe_copy.rs` (`rustix`)** — same shape, missed entirely by the first draft
even though D4's own text names it:
- `open_source(path, nofollow)` — line 50
- `create_dest_restrictive(path, nofollow)` — line 73

This is `cp`/`mv`'s open path. Its 8 codemod hits are *all* in its test module;
its production body is invisible to the codemod.

**`features/fs.rs` (`rustix`, `env`, `dunce`)** — the biggest "route" row, and
the one the first draft never questioned:
- `FileInformation::from_path` → `rustix::fs::{stat,lstat}` (lines 84, 86) — the
  dedup/identity entry point for `cp`, `du`, `ls`, `cmp`
- `uucore::fs::canonicalize` builds its absolute base from `env::current_dir()`
  + `dunce::canonicalize` (lines 410–411)

The second is worse than unrouted. After routing, `result.exists()` at line 479
goes through the vfs while the prefix that produced `result` came from the
**host process cwd**, not the session cwd (`Session` carries its own —
`session.rs:158`). The function becomes half-virtual, half-host: exactly the
"partial confinement that reads as confinement" this plan flags for the unforked
utilities, inside its own flagship module.

**Also invisible, also production:** `mods/io.rs:69`
`OwnedFileDescriptorOrHandle::open_file(&OpenOptions, &Path)` — always compiled,
opens an arbitrary path, zero codemod hits because `OpenOptions` is not in
`FACADE_FREE_FNS` at all (`R7` — a general blind spot, not a uucore quirk).
`features/perms.rs:78,80` `libc::{chown,lchown}` on raw pointers.
`features/perms.rs:26` `walkdir::WalkDir` — the crate `deny.toml:108` bans as
unroutable, with the comment "routing them is the forks-and-codemod milestone."
This is that milestone (`R8`). `features/pipes.rs:189` `OpenOptions` on a device.
`features/fs.rs:92-102` a `#[cfg(windows)]` `OpenOptions` branch.

### 5. The codemod will actively break `perms.rs`

`REWRITTEN_PATH_METHODS` (`xtask/src/codemod.rs:117`) rewrites any zero-arg
`.read_dir()` regardless of receiver type. Production uucore has exactly one
non-`Path` receiver and it is in a module this plan marks **route**:

```
features/perms.rs:499:        let entries = match dir_fd.read_dir() {
```

`dir_fd: &DirFd`. The codemod emits `ambient::read_dir(&(dir_fd))`, which fails
the `AsRef<Path>` bound. That is the designed safety net working — a compile
error, not a mis-route — but it means the residual patch set is **not** zero
(`R9`), and it means §4's "everything below `DirFd::open` is left alone" is only
true *inside* `safe_traversal`; the callers are not.

### 6. The repoint must be atomic, and there are seven consumers

`forks/uu_*` and `brush-coreutils-builtins` depend on `uucore = "0.10.0"` from
crates.io — **and so does `brush-builtins`** (`brush-builtins/Cargo.toml:155`),
which the first draft missed (`R10`). That one is a lint-and-ban-enforced
workspace member running **in the parent shell process**, where
`ambient::install` is never called (the only call sites are
`brush-shell/src/bundled.rs:192` and `brush-coreutils-builtins/tests/routing.rs:29`).
It uses only `uucore::format`, which is pure, so a routed uucore is probably
benign there — but "probably benign in a consumer we didn't know about" is
exactly what a gate is for.

Cargo treats `uucore 0.10.0 (registry)` and `uucore 0.10.0 (path)` as different
packages and will link both. That is not a type error — `register!`
(`brush-coreutils-builtins/src/lib.rs:91-107`) passes only `OsString` and `i32`
across the seam — it is a **runtime** one, and in *two* globals, not one
(`R11`):
- the localizer thread-local (`locale.rs:114-124`), set by
  `prepare_uutil_runtime`; a utility reading a different uucore's copy finds it
  unset
- `uucore::error::EXIT_CODE`, a process-global `AtomicI32` (`mods/error.rs:65`)
  that `tests/routing.rs:35` already resets across the crate boundary — a split
  here makes the **existing** routing tests report wrong exit codes

`[patch.crates-io] uucore = { path = "forks/uucore" }` repoints all seven plus
the 77 still-unforked crates.io `uu_*` (`R12` — the first draft said ~90).

**That last group is the new hazard.** An unforked `uu_ls` gets a routed
`uucore` and keeps its own unrouted `std::fs`. Partial confinement that reads as
confinement.

*Unverified:* whether `[patch]` reaches a path dependency of a workspace member
that is itself outside the workspace (`forks/` is `exclude`d). Step 3 checks
rather than assumes.

## Resolved: the directory capability

**Decided: add it (option (a)).** The alternatives were leaving
`safe_traversal`/`safe_copy`/`fs.rs` unconfined and documenting the hole, or
re-resolving paths per operation — the latter rejected because it discards the
`*at` anchoring that is the entire reason `safe_traversal` exists, reintroducing
the TOCTOU races upstream wrote that module to close.

**Scoping it after the decision shrank it, and this is the useful finding:**
only *one* of the four hand-route targets actually needs a directory descriptor.
The other three need small path-based additions, and two of the three already
have most of what they need.

| target | what it actually needs | state today |
|---|---|---|
| `safe_traversal::{DirFd::open, create_dir_all_safe}` | **a confined directory descriptor** — the new API | does not exist |
| `safe_copy::{open_source, create_dest_restrictive}` | `O_NOFOLLOW` + a create mode on an ordinary file open | `OpenMode` (`fs.rs:80`) is already a builder struct — grows two fields |
| `fs.rs::FileInformation::from_path` | `dev`/`ino` from a routed stat | `ambient::{metadata, symlink_metadata}` already return `std::fs::Metadata`; and `FileInformation` **already stores `fs::Metadata` on WASI** (`fs.rs:57`) — use that representation on Unix too |
| `fs.rs::canonicalize`'s absolute base | the *session* cwd, not the host's | `Session::cwd()` already exists (`session.rs:56`); needs an `ambient::current_dir()` over it |

So the reversal of `brush-vfs/src/fs.rs:3-7` is narrow. That text argues callers
should not be *forced* to hold a `Dir`, because an API demanding one would make
utilities restructure rather than rewrite — and that reasoning still stands for
every path-based caller. What changes is that a caller which is *already*
descriptor-shaped, as `DirFd` is, may ask for one. The path-based facade remains
the default and the only thing the codemod emits.

**D3 needs an amendment, not a repeal.** Its claim is that capability handles
never escape `brush-vfs`. After this they escape to exactly one consumer, under
a wrapper type that exposes `*at` operations and no way to recover a host path.
That amendment is step 4 and is a prerequisite for step 7, not a footnote.

## What this milestone does not prove

**That the 77 unforked utilities are confined.** After step 3 they get a routed
`uucore` and nothing more.

**That the "leave" list is complete.** It is complete for the feature set
`coreutils.all` resolves to *today*. A future utility enabling `uptime` or
`proc-info` brings 29 unrouted sites back. Step 7 is the answer and is the
weakest step here.

**Anything about Windows.** `safe_traversal` is Unix-only, `safe_copy`'s
`rustix` calls are Unix-only, the step-5 directory capability is Unix-only with
them, and `fs.rs:92-102`'s Windows `OpenOptions` branch is unrouted and stays
that way. So on Windows this milestone confines what the codemod routes and
nothing that step 7 hand-routes — a real asymmetry, not a deferral.

## The change

1. **`vendor-fork` learns build scripts and stops stripping dev-dependencies.**
   `copy_lib_sources` copies `build.rs` when the manifest declares one. Separately,
   `write_manifest` (`xtask/src/vendor.rs:189`) currently removes
   `dev-dependencies` — which deletes the `tempfile` every uucore test module
   uses, so the fork cannot compile its own tests and gates 1–2 cannot run.
   `[lib] path` and `package.build` already survive; the first draft listed those
   as gaps and they are not.

2. **The codemod skips `#[cfg(test)]`, and `vendor-fork` learns `--skip`.**
   Two exclusions, deliberately not merged:
   - Skipping `#[cfg(test)]` is unconditional and needs no flag: routing an
     upstream test is never right.
   - `--skip <path>` excludes a module, for the §3 "leave" rows, recorded in the
     fork manifest header so the list is re-readable at the next rebase (D13).

   Also fix `REWRITTEN_PATH_METHODS` against §5 — either infer the receiver type
   or add `DirFd` to a known-non-`Path` deny list.

3. **Vendor pristine `uucore` as its own commit, then patch.** D13's split
   unchanged. Then `[patch.crates-io]` in the root manifest, then `cargo tree -d`.
   **The pristine fork must build and pass its own tests before any routing.**

4. **Fix the locale embedding — without regressing the 77.** `R1` kills the
   `../uu_*` scan. The two designs that survive:
   - **Dual scan**: `forks/uu_*/locales` *and* the registry, located via
     `CARGO_HOME`/`CARGO_REGISTRY` rather than `manifest_dir.parent()`. Keeps
     current behaviour; inherits its fragility (it is why `cat`'s strings come
     from a stale crate today).
   - **Commit the merged `.ftl` set** into `forks/uucore/locales/` as a
     generated artifact with a regeneration xtask. Deterministic, reviewable,
     and severs the stale-registry dependency — at the cost of a vendored blob
     that must be refreshed per upstream import.

   Prefer the second. Either way this is the first fork edit that is **not**
   codemod output, and it gets a comment naming this plan.

5. **Amend D3, then grow `brush-vfs` by four things.** The amendment lands
   *first* — a capability escaping the crate is a decision, and writing the code
   before the decision is recorded inverts D13's discipline.
   - **A directory-capability type.** A confined descriptor exposing the `*at`
     operations `DirFd` needs (`openat`, `fstatat`, `unlinkat`, `mkdirat`,
     `fchmodat`, `fchownat`, `readdir`) and **no way to recover a host path** —
     that last property is what keeps D3's claim true in its amended form.
     Unix-only, matching `safe_traversal`.
   - **`OpenMode` grows `nofollow` and a create mode** (`fs.rs:80`), for
     `safe_copy`. `open_source` needs `O_NOFOLLOW`; `create_dest_restrictive`
     needs it plus `DEST_INITIAL_MODE`'s `0600`.
   - **`ambient::current_dir()`** over `Session::cwd()`, for `fs.rs`'s
     `canonicalize` base.
   - **Confinement tests for each**, in `brush-vfs`'s own suite, before any
     consumer exists — the crate's existing pattern.

6. **Route the §3 table** — ~24 production sites, `--skip` for the three
   compiled "leave" modules and the six un-compiled ones. Residual patch set
   expected to be `perms.rs:499` only, and zero once step 2's
   `REWRITTEN_PATH_METHODS` fix lands.

7. **Hand-route the four targets** against step 5's API:
   `safe_traversal::{DirFd::open, create_dir_all_safe}` onto the directory
   capability; `safe_copy::{open_source, create_dest_restrictive}` onto
   `OpenMode`; `FileInformation::from_path` onto `ambient::{metadata,
   symlink_metadata}` via the WASI representation; `fs.rs::canonicalize`'s base
   onto `ambient::current_dir()`. **This is the residual patch set D13 says to
   measure** — the only code here that a re-run cannot regenerate.

8. **A feature-set guard.** Assert the 76-feature set `coreutils.all` resolves
   to and fail when it changes, so enabling `uptime` forces a decision rather
   than shipping 29 unrouted sites. *Re-derive after step 2 — the first draft
   froze a table that step 2 invalidates.*

9. **CI wiring.** `xtask/src/{ci,test}.rs` contain **zero** references to
   `forks/`. Gates 1, 2 and 5 have no home today and must be given one, or they
   are notes.

## What stays behind, deliberately

The 77 unforked utilities. `findutils`, `grep`, `sed`. D13's expected-failure
infrastructure for upstream suites. The D24 child-confinement wiring.

## Gates

Each gate names its falsifier. Three of the first draft's eight could not fail
or could not be run and are replaced.

1. **The pristine fork builds and passes upstream's own tests, before routing.**
   *Mechanism, which the first draft lacked:* `cargo test` **inside**
   `forks/uucore` as its own workspace — `cargo test -p uucore` from the root
   fails with `package uucore cannot be tested because it requires
   dev-dependencies and is not a member of the workspace` (verified against
   `uu_cat`). Note `[patch]` does not apply in that mode and the fork's
   `Cargo.lock` is gitignored (`vendor.rs:229`), so **the tested dependency set
   is not the shipped one** — state that rather than implying otherwise.
   *Fails if:* the vendored baseline does not pass.

2. **Upstream's tests still pass after routing.** *Fails if:* any uucore unit
   test regresses. This is what catches a `#[cfg(test)]` skip that did not work
   — a routed test fails closed, loudly. Same mechanism as gate 1.

3. **Exactly one `uucore` in the graph.** *Fails if:* `cargo tree -d` reports
   two, **or** cargo's `[patch]` … `was not used` warning appears on stderr —
   grep for it, it is a warning and does not affect the exit code. Falsifies §6
   and tells us whether `[patch]` reaches `forks/`-excluded path deps.

4. **Localization renders — asserted on an *unforked* utility.** *Fails if:*
   `df` or `ls` emits a raw Fluent key. **Must not use `cat`**: it is one of the
   five forks, so it would pass under the broken `R1` design and let the
   regression ship. Assert on a message body, not an exit code — §1's failure
   leaves the exit code correct. **Write this gate first**; between step 3 and
   step 4 every bundled utility renders raw keys.

5. ~~Existing forks byte-identical after a codemod re-run.~~ **Replaced — it
   could not fail.** `codemod --check` on all five already reports `0 routed, 0
   unrouted` (verified), because they are already routed; a skip can only reduce
   rewrites, so 0 stays 0 unconditionally, and it would read green even if the
   skip excluded every item in every file. **New gate: the skip is tested
   directly** — a codemod unit fixture with an fs call inside `#[cfg(test)]` and
   one outside, asserting exactly one rewrite. *Fails if:* both or neither are
   rewritten.

6. **Confinement, in-process, through uucore.** *Fails if:* `uu_cksum` opens a
   host path outside the mount. It routes to 0 sites of its own (verified), so
   it is unconfined before this milestone and confined after — the one gate here
   that cleanly isolates what the milestone adds. Extends
   `brush-coreutils-builtins/tests/routing.rs`.

7. **The feature-set guard fires.** *Fails if:* the step-7 assertion can pass
   while `uptime` is enabled.

8. ~~The ban still holds and `deny.toml` is honest.~~ **Replaced — both halves
   were unfalsifiable.** `check_ban` (`xtask/src/check.rs:117,137`) runs clippy
   against a fixture and never sees the workspace, and `forks/` is excluded
   anyway, so nothing here can move it; and `deny.toml`'s `wrappers` entries are
   crate *names*, which do not change when uucore becomes a path dep. **New
   gate: `deny.toml` reflects the post-routing reality.** *Fails if:* `walkdir`
   and `xattr` remain justified by comments that this milestone falsified, or if
   `nix`, `rustix` and `procfs` — the actual unrouted surface after step 5 — are
   absent from the deny list. Whether they are *bans* or documented `wrappers`
   entries depends on how much step 7 confines.

9. **The directory capability cannot leak a host path.** *Fails if:* any public
   method on the new type returns a `PathBuf`, a host-absolute string, or a raw
   fd a caller could `fstat`-and-reconstruct from `/proc/self/fd`. This is the
   single property D3's amendment rests on; without it the amendment is a
   repeal. Assert it as a test, not a review convention — D3's own text is that
   a mechanism whose failure mode is silence must be proven on.

10. **A `DirFd` rooted in the namespace cannot walk out of it.** *Fails if:* a
    `..` component, an absolute symlink, or a symlinked subdirectory reaches a
    host path outside the mount. `safe_traversal` is the recursive-descent
    engine behind `chmod -R` and `chown -R`; a confined root that leaks on
    traversal confines nothing. Mirrors the escape suite `brush-vfs` already has
    for paths.

## Risk

**Medium**, and lower than the first draft implied — 24 production sites, not
69. Concentrated in four places.

**Two silent failure modes, both leaving exit codes correct** (§1 and §6), both
in localization. Gate 4 is the only thing between them and a green suite, which
is why it moved to first.

**The directory capability is the largest single piece, and it is new
security-relevant surface rather than a rewrite.** Everything else in this
milestone moves existing calls onto an existing facade; step 5 writes a
primitive that did not exist, in the one place D3 says hand-rolled hardening is
the thing to avoid. It is scoped down by leaning on `cap-std`'s `Dir` underneath
rather than re-deriving `*at` resolution — but the wrapper is still the part to
review hardest, and gates 9 and 10 are the two that most need to be written
before the code rather than after.

**Step 7 is unregenerable and grew.** The first draft treated hand-routing as
two functions in one module; it is six entry points across three. Every one of
them is a place a future upstream import can silently diverge, which is exactly
what D13's residual-patch-set metric is for — expect it to be the number that
moves at the next rebase.

**`[patch]` is unverified against a `forks/`-excluded path dep.** If it does not
reach, the fallback is editing seven manifests — more churn, regenerated by
`vendor-fork` anyway, and it leaves the 77 unforked utilities on the registry
uucore, which is *safer* for gate 6's honesty and worse for the utilities.

**Step 7 guards a table with no mechanism keeping it correct.** A feature-set
assertion fires on any change, including benign ones, and will be tempting to
update rather than think about.

**Rollback:** steps 3–4 are revertible (drop the `[patch]` line and the fork).
Step 2 is **not** scoped to this fork — the `#[cfg(test)]` skip and the
`REWRITTEN_PATH_METHODS` fix change the codemod for all forks, and reverting
`forks/uucore` does not undo them. Step 6's hand-routing, under (a), is the
non-regenerable residual patch set.

**Decision rule:** if gate 1 cannot be held — the pristine fork does not pass
its own tests — stop. A baseline that already differs makes every later failure
ambiguous, and the honest response is to fix the vendoring rather than route on
top of it.

## Where it landed

Written after the fact. The plan above is left as written so the difference is
visible.

### Built as planned

All nine steps. The measurements held: **24 production sites routed**, exactly
as re-derived after the review, with `perms.rs:499` the single unrouted site and
correctly so. `[patch]` did reach the `forks/`-excluded path dependency, closing
the one unknown the plan flagged — `exclude` governs member discovery, not
resolution.

Gate 1 got the comparison arm the review said it lacked: the registry copy
staged in a scratch directory alongside its 84 `uu_*-0.10.0` siblings,
reproducing the layout its build script expects. That made the locale bug
*measurable* rather than argued — fork 312/3 against registry 313/2 before the
fix, identical 313/2 after.

### Built differently

**Step 4 vendors the catalogs rather than dual-scanning.** The plan preferred
this and the preference was right, but for a sharper reason than it gave:
upstream's registry scan was already embedding `cat`'s strings from a **stale
copy of a crate the workspace no longer depends on**. Committing them makes the
embedded strings a reviewable input.

**Gate 4 was not written.** It was unnecessary. Upstream's own
`test_setup_localization_fallback_to_embedded` asserts a utility about-string
and receives the raw key, so the localization gate already existed inside the
suite gate 2 runs. A separate `df`/`ls` assertion would have been a second,
weaker copy of it.

**Gates 5 and 8 were replaced as the plan directed**, and gate 2 grew teeth it
was not specified to have — see below.

### Built after the fact

**An identity session for the fork's own test suite.** Routing broke 11
upstream tests, all `no vfs session is installed`. The plan had deferred D13's
expected-failure infrastructure on the grounds that "uucore has no deliberate
divergence yet"; routing *is* the divergence. Muting 11 tests would have gone
dark over every `create_dir_all_safe` case — the exact code step 7 then
hand-routed. The hook is `#[cfg(test)]`, so it is absent from production builds
rather than disabled in them.

**`known-test-failures.txt`, so D13's expected-failure mechanism exists.** The
plan listed it under "what stays behind". `check forks` needed it immediately:
a fork inherits upstream's host-dependent failures, so the metric is "did *we*
change the count". The mirror rule is enforced both ways.

**A carve-out inside D3's amendment.** `DirFd` could not be ported onto the
sealed capability in this milestone — it needs `chown`, which `cap-std` does not
provide — so `Dir::into_owned_fd_for_at_traversal` surrenders the descriptor.
`safe_traversal` is *rooted* in the namespace, not sealed inside it: a raw
directory fd can still be walked upward with `openat(fd, "..")`. Recorded as a
weakening in D3 and named in the gate that would otherwise fail, so a second one
cannot appear quietly. Taken because what it replaces — `DirFd::open` accepting
any host path outright — is worse by a wide margin.

**A real bug, caught by gate 2 rather than by review.** `O_NOFOLLOW` passed down
as a raw open flag is *inert* here: resolution follows a final symlink before
any descriptor exists, so the flag lands on the target and never fires.
Upstream's `open_source_rejects_symlink_with_nofollow` returned `Ok(File)`. It is
answered during resolution now. Both ends of that link are inside the namespace,
so confinement alone would never have caught it — which is the argument for
having run upstream's suite instead of muting it.

### Not built

`DirFd`'s full port onto the sealed capability. The 77 unforked utilities
remain unconfined except through `uucore`. `findutils`, `grep`, `sed`. D24's
child-confinement wiring. Windows: `safe_traversal`, `safe_copy` and the
directory capability are all Unix-only, so there this milestone confines what
the codemod routed and nothing that was hand-routed.
