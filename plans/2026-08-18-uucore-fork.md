# flatland: fork and route `uucore`

Second increment of D4, and the one the first increment ran into. Five leaf
utilities (`cat`, `head`, `wc`, `tac`, `nl`) are forked and routed;
`cargo xtask vendor-fork` automates the leaf case. `uucore` is not a leaf, and
the attempt to run the same tool at it stopped on `couldn't read build.rs`.
That error is the smallest of four reasons this is a milestone rather than a
command invocation.

**Status: not started.** Everything below is measured against
`uucore 0.10.0` in the registry cache unless marked otherwise.

## Why this is not another `vendor-fork` run

The one-line summary of the last increment was "uucore is the real fork
boundary for many utilities" — `uu_cksum` routes to **0** sites because it
delegates entirely to `uucore::checksum`. That is the motivation. These are the
obstacles, all four verified this session.

### 1. The build script embeds locales by scanning a directory that will no longer exist

`vendor-fork` does not copy `build.rs`; that is a two-line fix and is *not* the
problem. The problem is what the build script does once copied.

uucore compiles `.ftl` locale files into the binary
(`locale.rs:64` — `include!(concat!(env!("OUT_DIR"), "/embedded_locales.rs"))`).
For a crates.io build, `build.rs`'s `embed_static_utility_locales` walks the
**sibling registry directory**, matching entries shaped `uu_<util>-<version>`
via `split_once('-')`, and embeds each one's `locales/en-US.ftl`.

Under `forks/uucore`, the siblings are `forks/uu_cat`, `forks/uu_head`, … —
**no `-version` suffix, so `split_once('-')` matches nothing and no
per-utility locale is embedded.**

The failure is silent, not a build error. The crate's own `locales/en-US.ftl`
is still found (it is under `manifest_dir`), so the bundle contains
`common-error`, so `create_english_bundle_from_embedded` returns `Ok`, so
`setup_localization` succeeds — and every utility-specific `translate!` renders
its **raw Fluent key** (`cat-error-…`) instead of a message. Exit 99 would at
least be loud; this is not.

Worth recording because it indicts the present state too: today's build embeds
`uu_cat`'s locales from a **stale registry copy of a crate we no longer depend
on**. That has been accidental since the first fork landed.

### 2. The codemod rewrites `#[cfg(test)]` code, and here that matters

Dry run over `uucore/src/lib`: **119 sites routed, 19 unrouted.**

Every one of the 19 is `create_dir` or `hard_link` — genuine D34 carve-outs —
and **all 19 are inside `#[cfg(test)]` modules or the un-compiled `benchmark`
feature.** Production carve-outs: **zero.** Same reading as `uu_cat`.

The bad news is the mirror image: a large share of the 119 *routed* sites are
inside those same test modules (`safe_traversal.rs` alone: tests begin at line
931, and the file reports 18 routed / 11 unrouted across 1,464 lines). D13
makes the upstream suites a health metric. Routing upstream's own tests through
a facade that **fails closed with no session installed** breaks them, and the
resulting failures would be read as divergences rather than as codemod damage.

The five existing forks escaped this by luck, not design: `uu_wc` has no test
module at all, and in the other four every `ambient::` call sits above the
`#[cfg(test)]` line (verified — `cat.rs:385,441` vs tests at 720; `head.rs:471`
vs 545; `tac.rs:376` vs 457; `nl.rs:256` vs 473). uucore is the first crate
where it bites.

**The codemod needs to skip `#[cfg(test)]` items.** This is a new requirement,
not a latent bug already shipped.

### 3. Three modules that compile must *not* route

Under `coreutils.all`, uucore resolves with 69 features — and `uptime`,
`proc-info`, `selinux`, `smack`, `feat_systemd_logind`, `benchmark`, `tty` and
`utmpx` are **not** among them. That removes most of the introspection surface
from the compiled build outright. What remains, measured:

| module | feature | sites | disposition |
|---|---|---|---|
| `features/fs.rs` | `fs` | 17 | **route** |
| `features/safe_traversal.rs` | `safe-traversal` | 18 | **route** + hand-route (§4) |
| `features/backup_control.rs` | `backup-control` | 15 | **route** |
| `features/fsxattr.rs` | `fsxattr` | 7 | **route** |
| `features/safe_copy.rs` | `safe-copy` | 6 | **route** |
| `features/checksum/{compute,validate}.rs` | `checksum` | 3 | **route** |
| `features/perms.rs` | `perms` | 3 | **route** |
| `features/fsext.rs` | `fsext` | 3 | **leave** — `/etc/mtab`, `/proc/self/mountinfo` |
| `mods/locale.rs` | always on | 16 | **leave** — the message catalog beside the executable |
| `mods/os.rs` | always on | 2 | **leave** — `/proc/sys/kernel/osrelease` |
| `features/{benchmark,proc_info,selinux,smack,systemd_logind,uptime}.rs` | all off | 30 | **leave** — not compiled |

The three "leave" modules that *do* compile are the whole reason a
directory-wide codemod run is wrong. `fsext` reads the mount table; routing it
makes `df` fail. `mods/locale.rs` locates the shell's own message catalog
relative to `current_exe`; routing it makes the catalog unreachable under a
restrictive policy — and the utility would then fail to render the error
explaining why. Both are host introspection and self-location, not namespace
access: the same class as `sys/unix/fd.rs`'s exemption in the foundation
milestone, and they get the same treatment.

### 4. `safe_traversal` is `nix`, invisible to the codemod — and smaller than it looks

`safe_traversal.rs` uses `nix::fcntl::{open, openat}`, `nix::sys::stat::{fstat,
fstatat, fchmodat, mkdirat}`, `nix::unistd::{unlinkat, fchown, fchownat}`. The
`syn` codemod only sees `std::fs`, so none of it is reachable mechanically.

But it has exactly **two ambient entry points**:

- `DirFd::open(path, symlink_behavior)` — line 143, `nix::fcntl::open` on a
  caller-supplied path
- `create_dir_all_safe(path, mode)` — line 616

Every other method on `DirFd` — `open_subdir`, `stat_at`, `metadata_at`,
`read_dir`, `unlink_at`, `chown_at`, `fchown`, `chmod_at`, `fchmod`,
`mkdir_at`, `open_file_at` — is `*at`-relative to an existing descriptor and is
therefore **already confined to whatever root those two produced**.

So this is not 1,464 lines of rewriting. It is two functions that must resolve
through the vfs and hand back a descriptor. D13 predicted exactly this —
"route through the abstraction you already built, and let its root be
injectable" — and the prediction holds.

### 5. The repoint must be atomic

`forks/uu_cat` and friends currently depend on `uucore = "0.10.0"` from
crates.io, as does `brush-coreutils-builtins`. Cargo treats
`uucore 0.10.0 (registry)` and `uucore 0.10.0 (path)` as **different packages**
and will happily link both.

That would not be a type error — `register!` passes only `OsString` and `i32`
across the seam — it would be a **runtime** one. `prepare_uutil_runtime` calls
`uucore::locale::setup_localization`, which sets a **thread-local** in whichever
uucore `brush-coreutils-builtins` links; a utility reading a *different*
uucore's thread-local finds it unset. Silent, again, and in the same subsystem
as §1.

`[patch.crates-io] uucore = { path = "forks/uucore" }` in the root manifest
repoints every consumer in one line: the five forks, `brush-coreutils-builtins`,
and the ~90 still-unforked crates.io `uu_*` crates.

**That last group is the new hazard.** An unforked `uu_ls` would get a routed
`uucore` and keep its own unrouted `std::fs` — partial confinement that reads
as confinement. Gate 4 exists for that.

*Unverified:* whether `[patch]` reaches a path dependency of a workspace member
that is itself outside the workspace (`forks/` is `exclude`d). Step 3 checks
rather than assumes.

## What this milestone does not prove

**That the ~90 unforked utilities are confined.** After step 3 they get a routed
`uucore` and nothing more. `du`, `ls`, `cp`, `rm` and `find` still reach the
filesystem directly. This milestone makes uucore-mediated access confined; it
does not make a utility confined.

**That the "leave" list is complete.** It is complete for the feature set
`coreutils.all` resolves to *today*. A future utility enabling `uptime` or
`proc-info` brings 30 unrouted sites back with no gate firing. Step 7 is the
answer to that and is the weakest step here.

**Anything about Windows.** `safe_traversal` is Unix-only, and the hand-routing
in step 5 is Unix-only with it.

## The change

1. **`vendor-fork` learns build scripts.** `copy_lib_sources` copies `build.rs`
   when the manifest declares one, and `write_manifest` preserves the `build`
   and `[lib] path` keys. Mechanical; unblocks a pristine vendor.

2. **`vendor-fork` learns `--skip`, and the codemod learns `#[cfg(test)]`.**
   Two independent exclusions with two different justifications, deliberately
   not merged into one flag:
   - `--skip <path>` excludes a *module* from the codemod run, for the step-3
     table's "leave" rows. Recorded in the fork's manifest header so the
     exclusion list is re-readable at the next rebase, per D13.
   - Skipping `#[cfg(test)]` items is unconditional and needs no flag: routing
     an upstream test is never right. Applies to every fork, including a re-run
     over the existing five (which is expected to be a no-op — verified above —
     and that no-op is itself the regression test).

3. **Vendor pristine `uucore`, as its own commit.** The D13 split, unchanged
   from the leaf case: baseline in one commit, codemod output in the next, so
   the transformation is a reviewable diff. Then
   `[patch.crates-io] uucore = { path = "forks/uucore" }` in the root manifest,
   and `cargo tree -d` to confirm exactly one uucore in the graph. **The
   pristine fork must build and pass its own tests before any routing** — that
   is what makes a later failure attributable.

4. **Fix the locale embedding.** The build script's registry scan cannot work
   from `forks/`. Prefer replacing that scan with one over `../uu_*/locales`
   (the fork layout it will actually live in), keeping the `UUCORE_TARGET_UTIL`
   and multicall paths untouched. This is a divergence from upstream and gets a
   comment naming this plan, since it is the first fork edit that is *not*
   codemod output.

5. **Route, per the step-3 table.** `cargo xtask codemod forks/uucore/src/lib`
   with `--skip` for `fsext.rs`, `mods/locale.rs`, `mods/os.rs`, and the six
   un-compiled modules. Expect ~69 production sites and a residual patch set of
   zero.

6. **Hand-route `safe_traversal`'s two roots.** `DirFd::open` and
   `create_dir_all_safe` resolve through `brush_vfs::ambient` and wrap the
   resulting descriptor; every `*at` method below them is left alone. Unix-only.
   This is the only hand-written routing in the milestone and the only part that
   cannot be regenerated, so it is the residual patch set — the thing D13 says
   to measure.

7. **A feature-set guard.** A test that asserts the set of uucore features
   `coreutils.all` resolves to, and fails when it changes. Enabling `uptime`
   then forces a decision about `uptime.rs`'s unrouted sites instead of
   shipping them. Without this, step 3's table silently expires.

## What stays behind, deliberately

The ~90 unforked utilities. `findutils`, `grep`, `sed`. The expected-failure
infrastructure for upstream suites (D13) — uucore has no deliberate divergence
yet, and step 4's locale edit is a build-system fix, not a behavioural one.
The D24 child-confinement wiring, which still installs identity rather than the
parent's policy.

## Gates

Each gate names its falsifier.

1. **The pristine fork builds and passes upstream's own tests, before routing.**
   *Fails if:* `cargo test -p uucore` on the vendored baseline differs from the
   same run against the registry copy. Without this the baseline is not a
   baseline and step 5's failures are unattributable.

2. **Upstream's tests still pass after routing.** *Fails if:* any uucore unit
   test regresses. This is the gate that catches a `#[cfg(test)]` skip that
   did not work — a routed test fails closed with no session, loudly.

3. **Exactly one `uucore` in the graph.** *Fails if:* `cargo tree -d` reports
   two, or if the `[patch]` entry is reported unused. Directly falsifies the
   §5 hazard, and is the check that tells us whether `[patch]` reaches
   `forks/`-excluded path deps at all.

4. **Localization still renders.** *Fails if:* any bundled utility emits a raw
   Fluent key. Assert on a message body — e.g. `cat` on a missing file — not on
   an exit code, since §1's failure mode leaves the exit code correct. The
   existing `brush-coreutils-builtins` routing tests do not cover this and must
   be extended.

5. **The five existing forks are byte-identical after a codemod re-run.**
   *Fails if:* `--check` reports any rewrite. Proves the `#[cfg(test)]` skip
   changed nothing that was already correct.

6. **Confinement, in-process, through uucore.** *Fails if:* a utility that
   reaches the filesystem *only* via uucore opens a host path outside the mount.
   `uu_cksum` is the case to use — it routes to 0 sites of its own, so before
   this milestone it is unconfined and after it is confined. Extends the
   existing `brush-coreutils-builtins/tests/routing.rs` pattern.

7. **The feature-set guard fires.** *Fails if:* the assertion in step 7 can be
   made to pass while `uptime` is enabled. A guard that cannot fail is a note.

8. **The ban still holds and `deny.toml` is honest.** *Fails if:*
   `cargo xtask check ban` regresses, or `walkdir`/`xattr`'s `wrappers` entries
   in `deny.toml` no longer name their actual parents once uucore is a path dep.

## Risk

**Medium.** Lower than the foundation milestone — the facade exists, the
codemod exists, the confinement test pattern exists, and the production carve-out
count is zero. Concentrated in three places.

**Two silent failure modes, both in localization** (§1 and §5), both of which
leave exit codes correct. Gate 4 is the only thing standing between them and a
green suite, which makes it the most load-bearing gate here and the one most
worth writing first.

**`[patch]` is unverified against a `forks/`-excluded path dep.** If it does not
reach, the fallback is editing the `uucore` dependency in all six manifests —
more churn, regenerated by `vendor-fork` anyway, and it leaves the ~90 unforked
crates on the registry uucore, which is *safer* for gate 6's honesty and worse
for the utilities. Either outcome is workable; the plan should not assume which.

**Step 7 is the weakest step.** It guards a table that is correct today and has
no mechanism keeping it correct. A feature-set assertion is a blunt instrument —
it fires on any feature change, including benign ones, and will be tempting to
update rather than to think about.

**Rollback:** steps 1–3 are revertible (drop the `[patch]` line and the fork
directory). Step 6's hand-routing is not regenerable and is the part to review
hardest. Step 4's build-script edit diverges from upstream permanently and is
the first entry in uucore's residual patch set.

**Decision rule:** if gate 1 cannot be held — the pristine fork does not match
the registry copy — stop. A baseline that already differs makes every later
failure ambiguous, and the honest response is to fix the vendoring rather than
to route on top of it.
