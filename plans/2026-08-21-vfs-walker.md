# seahaven: a confined recursive walk

The last piece of D4. Four forks traverse with `walkdir`, which opens
directories by path and reads them itself, so the namespace never sees the
walk. `deny.toml` bans the crate for exactly that reason and then lists every
consumer as a `wrappers` exemption — an inventory of what is knowingly
unrouted.

**Status: implemented**, with one deliberate exception — see "Where it landed"
at the end.

## The part that is already shipping

Stated first because it is the reason this is not merely tidy-up.

`find` is held back — forked, routed, deliberately unregistered. But three
recursive modes **are registered and in use today**, and their walks are
unrouted: `grep -r`, `cp -r`, and `chmod -R` / `chown -R` through
`uucore::perms`.

Measured, not assumed. Running the bundled `grep -r` against a directory
outside the mount:

```
grep -r needle <host path outside the mount>   ->  exit 2
```

The reads fail closed — no file content escapes, because every open still goes
through the facade. What does escape is **enumeration**: `walkdir` lists host
directories the namespace does not contain, and the utility learns which paths
exist and what type they are. Error messages naming discovered paths are the
channel that turns that into output.

So the current position is narrower than "unconfined" and wider than
"confined": *content is contained, structure is not*. That distinction is worth
holding onto, because it is also the thing a reviewer is most likely to get
wrong in either direction.

## What is already true

**Four consumers, and the API surface is bounded.** The union across all four:

| | builder | iterator | entry | error |
|---|---|---|---|---|
| `findutils` (`find`) | `new`, `min_depth`, `max_depth`, `follow_links`, `follow_root_links`, `same_file_system`, `contents_first`, `sort_by` | `into_iter`, `next`, `skip_current_dir` | `path`, `file_name`, `file_type`, `metadata`, `depth`, `ino`, `path_is_symlink`, `into_path` | `path`, `io_error` |
| `uucore` (`perms`) | `new`, `follow_links`, `min_depth` | `into_iter`, `next`, `skip_current_dir` | `path`, `file_type` | `path`, `io_error` |
| `uu_cp` | `new`, `follow_links`, `same_file_system` | `into_iter` | `path` | — |
| `uu_grep` | `new`, `follow_links` | `into_iter`, `next`, `skip_current_dir` | `path` | — |

Call counts across the four forks: `.path()` 281, `.metadata()` 54,
`.file_type()` 42, `.file_name()` 26, `.ino()` 10, `.depth()` 10,
`.path_is_symlink()` 4, `.io_error()` 2, `.into_path()` 2. `filter_entry` is
**not** used anywhere, which removes the one part of walkdir's API that would
have forced a closure-typed design.

`findutils` wraps entries in its own `WalkEntry::from_walkdir`
(`src/find/matchers/entry.rs:220`), so its coupling is concentrated at one
adapter rather than spread through the matchers.

**The pieces to build on exist, and they are cross-platform.**
`brush_vfs::dir::Dir` already does confined descent (`open_dir`, `metadata`,
`entry_names`), and `Vfs::open_dir` mints one from a resolved path. A walk over
that is anchored per level rather than re-resolving a path per entry.
`cap_std::fs::Dir` has a Windows backend, and only two things on our wrapper are
Unix — `set_permissions`, which takes a `u32` mode, and the descriptor accessor
`safe_traversal` needs. Neither is used by a walk.

**One Windows gap is real and load-bearing.** `Vfs::symlink_metadata` falls back
to `metadata` on non-Unix, so it *follows* the final link there. A walker built
on it would silently follow directory symlinks under `follow_links(false)` — an
escape-shaped bug, not a cosmetic difference. This was already a recorded
deferral; the walker is what makes it matter, so it is step 0 rather than a
footnote.

**`deny.toml`'s inventory has already rotted, and cannot fail.** `findutils`
and `uu_grep` are direct parents of `walkdir` and are *not* listed as wrappers.
`cargo-deny` reports this as `unmatched-wrapper` — a **warning** — and exits 0,
so `cargo xtask check deps` passes while the list misdescribes the tree. Two
separate defects: the entries are stale, and the mechanism that was supposed to
notice cannot.

## The change

0. **Make `symlink_metadata` stop following on Windows, and give `Dir` a
   no-follow child stat.** The technique is already proven in this tree:
   `uucore::fs::FileInformation::from_path` opens with
   `FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS` and stats the
   handle, which is the Windows analogue of the `O_PATH`/`O_SYMLINK` open
   `symlink_metadata_at` already does on Unix. `OpenMode::to_cap_std` currently
   wires `custom_flags` only under `#[cfg(unix)]` and needs a Windows arm.

   This is first because everything after it inherits the answer, and because a
   walker that quietly follows links on one platform is worse than no walker.

1. **`brush_vfs::walk`, mirroring `walkdir`'s shape.** A builder with the eight
   options above, an iterator yielding `Result<DirEntry, Error>`, and
   `skip_current_dir`. Mirrored deliberately: it keeps each consumer's change an
   identifier swap (D34) rather than a restructure, which is the same bargain
   the facade already makes for `std::fs`.

   `DirEntry::path()` returns a **virtual** path. That is the whole point — a
   host path handed back here would make every consumer half-virtual, the defect
   already fixed once in `uucore::fs::canonicalize`.

2. **Descend by capability, not by path.** Each level is a
   `dir::Dir::open_dir(name)`; each entry is stat'd with `Dir::metadata` /
   `Dir::symlink_metadata`. No path is re-resolved mid-walk, so the walk is
   `*at`-anchored — strictly better than `walkdir`, which re-opens by path and
   can be redirected between listing a directory and descending into it. This is
   what `uucore::safe_traversal` exists to provide and what `chmod -R` should
   have had all along.

3. **The three semantics that are easy to get subtly wrong**, called out so
   they get tests rather than confidence:
   - **Symlink loops.** With `follow_links`, `walkdir` tracks ancestors and
     reports a loop rather than spinning. Needs dev/ino ancestry, and must
     produce the same error shape `uucore::perms` already matches on
     (`io_error()` absent → "Too many levels of symbolic links").
   - **`same_file_system`.** A comparison against the root's volume, which is
     `MetadataExt::dev()` on Unix and `volume_serial_number()` on Windows —
     different APIs for the same question, so it needs a small internal identity
     type rather than a `dev` field. `DirEntry::ino()` is the mirror case and
     stays `#[cfg(unix)]`, as `walkdir`'s own does.
   - **`contents_first`.** Post-order, which `find -depth` depends on and which
     interacts with `skip_current_dir`.

4. **`Dir` grows what `perms` needs.** Recursive `chown` has no `cap-std`
   expression at all, so this is a raw `fchownat` inside the trusted boundary,
   the same shape as `symlink_metadata_at`. Unix-only, because `uucore::perms`
   is. Without it `perms` cannot leave `walkdir` and step 5 stalls on its
   largest consumer.

5. **Port the four consumers**, smallest first: `uu_grep`, `uu_cp`,
   `uucore::perms`, then `findutils`. Each is a residual patch entry
   (`forks/RESIDUAL-PATCHES.md`) since none is codemod output.

6. **Register `find`.** The confinement sweep in
   `brush-coreutils-builtins/tests/routing.rs` currently asserts `find` is
   *absent*; that assertion inverts.

7. **Retire the `walkdir` ban entry, and fix the mechanism that let it rot.**
   If no fork depends on `walkdir` afterwards, the entry goes entirely. Either
   way `unmatched-wrapper` must become an error rather than a warning, or the
   next stale entry is equally invisible.

## What stays behind

`notify` (`uu_tail -f`), `fs_extra` (`uu_mv`'s progress sizing), `filetime`,
`xattr`. Each is a separate unroutable dependency with its own disposition
under D4; none is a traversal.

Nothing platform-shaped. An earlier draft of this plan excluded Windows on the
grounds that `Dir` is Unix-only. That was simply wrong — `Dir` is
cross-platform, and checking the claim instead of repeating it turned up that
`brush-vfs` did not compile for Windows at all, which is now fixed. Windows
directories need recursive walking exactly as much as Unix ones, so the walker
covers both and `deny.toml`'s `walkdir` entry can be deleted outright rather
than narrowed.

## Gates

1. **The no-follow stat does not follow, on every platform.** A symlink to a
   directory reports as a symlink, not as its target. *Fails if:* Windows still
   answers with the target's metadata. Without this the differential in gate 2
   passes on Unix and means nothing on Windows.
2. **Differential against `walkdir` itself.** Both walk the same fixture tree
   under the identity policy; the yielded sequences must be identical —
   same entries, same order, same depths, same error positions. Fixture covers
   nesting, a symlink to a directory, a symlink loop, a dangling symlink, a
   FIFO, mixed depths, and a directory made unreadable mid-tree. *Fails if:* any
   divergence. This is the gate that actually retires the risk; the rest are
   narrower. Run on Linux, macOS and Windows, since the point is that the walk
   agrees with `walkdir` everywhere `walkdir` runs.
3. **Enumeration is confined.** `grep -r`, `cp -r`, `chmod -R` and `find`
   rooted outside the mount must yield **zero entries**, not merely fail to read
   them. *Fails if:* any entry is produced from outside the namespace. The
   existing sweep asserts on exit codes, which this deliberately does not — an
   exit code cannot distinguish "enumerated then refused" from "never
   enumerated", and that distinction is the milestone.
4. **A symlink loop terminates.** *Fails if:* the walk hangs, or reports an
   error `uucore::perms`'s existing match arm does not recognize. A hang cannot
   be asserted against directly, so the guard is that the test returns at all —
   the same shape as the FIFO regression test.
5. **`find` is registered and confined.** *Fails if:* it reads or enumerates
   outside the mount, or the sweep's absence assertion is left inverted-but-
   unreplaced.
6. **The fork suites stay green.** `cargo xtask check forks`, all 52.
   *Fails if:* any upstream test regresses, or a `known-test-failures.txt` entry
   starts passing. This is what catches semantic drift the differential fixture
   did not think to cover.
7. **`deny.toml` describes the tree, and can say so.** *Fails if:*
   `unmatched-wrapper` is still a warning, or any `wrappers` entry names a crate
   that is not a real parent. Both halves are needed: the current entry is stale
   *and* the check that should have caught it exits 0.
8. **Performance is not a cliff.** A walk over a large tree within a stated
   factor of `walkdir`'s, measured and committed as a baseline. *Fails if:* past
   the threshold. `grep -r` and `find` over a source tree are the realistic
   shapes; per-entry `openat` is the cost this design trades for the anchoring.

## Risk

**Medium-high, and concentrated in one place: semantic drift.** `walkdir` is
mature and its edge behaviours — error-as-item, ordering under
`contents_first`, what `skip_current_dir` does after an error, loop reporting —
are load-bearing for four utilities. Reimplementing them is exactly the
hand-rolled hardening D3 exists to avoid, and this would be the *second* such
place after the symlink resolution in `fs.rs`. Gate 2 is the answer, and it is
worth building before the walker rather than after.

**The `chown` addition widens the trusted boundary.** `cap-std` has no
expression for it, so it is a raw `fchownat`. Small, but it is new
security-relevant surface in the crate whose job is to have none.

**Scope creep into `find`'s matchers.** `WalkEntry::from_walkdir` concentrates
the coupling, but `find` reads `ino()`, `depth()` and `path_is_symlink()`, and a
mismatch in any of those changes `-inum`, `-maxdepth` or `-type l` behaviour
without failing to compile.

**Rollback:** the crate and the four ports are revertible per-fork — each
consumer can go back to `walkdir` independently, since the API is mirrored. What
is not cheaply revertible is `Dir::chown`, and `find`'s registration is a
one-line change either way.

**Decision rule:** if gate 2 cannot be held — the differential diverges in ways
that are not obviously the vfs being *more* correct — stop and reconsider before
porting consumers. A walker that is subtly different from `walkdir` is worse
than an honestly unrouted one, because four utilities would then be quietly
wrong instead of visibly unconfined.


## Where it landed

Written after the fact. The plan above is left as written so the difference is
visible.

### Built as planned

Steps 0-3 and 5-7. The walker mirrors `walkdir`, descends by directory
capability, and yields virtual paths; `find`, `grep -r` and `cp -r` are ported
and `find` is bundled for the first time. Gate 2 was built before the walker,
which is the single decision that paid for itself.

### Built differently

**Step 4, `Dir::chown`, was not built, and `uucore::perms` keeps `walkdir`.**
The plan asserted `perms` blocks on it. Measuring found `perms` unreachable from
anything bundled — `chmod`, `chown` and `chgrp` are not in the coreutils set —
and on Linux it takes the vfs-rooted `DirFd` path regardless. Porting it needs a
routed `chown`, real trusted-boundary code, written for a caller that does not
exist. Recorded in `deny.toml` and `forks/RESIDUAL-PATCHES.md` rather than done.

**Gate 3 moved down a layer.** The plan put it on the utilities; an exit code
cannot separate "enumerated the tree, then failed every read" from "never
looked", and only the second is confinement. It is asserted on the walker
instead: a walk rooted outside the mount yields zero entries and one error.

**Gate 8, the performance baseline, was not built.** No number was measured, so
nothing is claimed. Per-entry `openat` is the cost the anchoring trades for, and
it is still unquantified.

### Built after the fact

**A Windows build check in `check build`.** Chasing the plan's wrong claim that
`Dir` is Unix-only found that `brush-vfs` did not compile for Windows at all.
Native-only checking is how that happened, so it stopped being native-only.

**`check deps` fails on a stale `wrappers` list.** `cargo-deny` reports both
unmatched and unused wrappers as warnings and exits 0 — how the list went stale
in the first place.

**`check lint` was repaired.** It runs `--all-features`; every clippy run this
session omitted that, so an integration test crate full of `unwrap`s had been
failing the gate since it was written.

### What the differential caught

Two bugs, both the kind reading would not have found. `path_is_symlink()` went
false once a link was followed, losing the distinction `find -type l` depends
on. And descending by parent handle is wrong for a followed symlink — `../..`
names somewhere above its parent, so cap-std refused it, correctly, and the loop
went unreported as an ordinary I/O error.

### Not built

`Dir::chown`. The performance baseline. `notify` (`uu_tail -f`), `fs_extra`,
`filetime` and `xattr` remain separate unroutable dependencies with their own
dispositions under D4.
