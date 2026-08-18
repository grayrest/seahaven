# Codemod spike: `uu_cat` and `uu_ls` — findings

Run 2026-08-18 against `uu_cat` 0.10.0 (1 util, ~900 lines) and `uu_ls` 0.10.0
(6,568 lines), the small and large ends of the fork set. Sources extracted from
the cargo registry cache; no code was written to the repo.

**The question:** D34 asserts that the `std::fs` → vfs rewrite is
*signature-preserving*, and D13 rests the whole "forks are generated, not
maintained" model on it. The assertion was untested.

**The answer: it holds — conditional on one design choice that was not
previously stated, with two carve-outs that are decisions rather than rewrites.**

## The binding constraint: a path-based, std-typed facade

The vfs must expose `vfs::read_dir(path)`, `vfs::metadata(path)`,
`vfs::open(path)` — **taking paths and returning `std` types** — and must not
require callers to hold `Dir` handles.

`uu_ls`'s `PathData` (`ls.rs:802-818`) carries `p_buf: Cow<'a, Path>` and
re-opens by absolute path throughout: `fs::read_dir(path_data.path())`
(`ls.rs:1217`), `fs::canonicalize(path.path())` (`display.rs:1249`),
`path.path().read_link()` (`colors.rs:326`). A `Dir`-handle API would force that
struct to carry a mount handle and thread it through ~15 call sites in four
files — a restructure, not a rewrite, and D34's rule would fail immediately.

This is sound, not a compromise. `cap_std::fs::File::into_std()` exists
(verified), so the facade can open beneath a mount with cap-std and hand back a
plain `std::fs::File`. **Confinement comes from resolution, not from the handle
type** — once a descriptor is opened beneath the mount it carries no ambient
authority. Returning `std::fs::File` / `std::fs::Metadata` also means zero type
propagation into caller code, which is what makes the rewrite an identifier
swap.

**Action: state this in the plan.** `brush-vfs` presents a `std::fs`-shaped
path API; `Dir` handles are internal.

## The `DirEntry` risk did not materialise

The predicted breaking issue was `cap_std::fs::DirEntry` having no `path()`
method, unlike `std::fs::DirEntry`. `uu_ls` stores a `DirEntry`
(`ls.rs:808`, `:835`) but uses only `file_name` (`:851`) and `file_type` — with
a comment at `:868` explaining that preferring `d_type` avoids a stat. It never
calls `DirEntry::path()`. Every `.path()` in the crate is `PathData::path()`,
its own accessor.

## Site counts

| crate | lines | fs-shaped sites (production) | density |
|---|---|---|---|
| `uu_cat` | ~900 | 12 | ~1.3% |
| `uu_ls` | 6,568 | 132 | ~2.0% |

At ~2%, coreutils at ~100 utilities is plausibly 2,000–3,000 sites. Large, but
that is what a codemod is for.

`uu_cat` is entirely mechanical: `File::open(path)` (`cat.rs:386`) flows into
`InputHandle<R: FdReadable>`, so a generic absorbs the type — and with the
std-typed facade the type does not even change. `std::process::exit(13)`
(`cat.rs:717`) needs nothing under D5's process boundary. The `OpenOptions`
cluster in `platform/unix.rs` is inside `#[cfg(test)]`.

## The codemod needs two visitors, not one

- **Free functions** — `fs::read_dir(p)` → `vfs::read_dir(p)`. A trivial `syn`
  path rewrite.
- **Inherent `Path` methods** — `p.metadata()`, `p.exists()`, `p.is_dir()`,
  `p.read_link()`, `p.symlink_metadata()` → `vfs::metadata(p)` and friends. A
  *method-call* rewrite against a receiver whose type must be inferred as
  `Path`/`PathBuf`. **This is the bulk of `uu_ls`'s 132 sites.** An earlier draft
  claimed clippy could not see these and that the codemod and the ban shared a
  blind spot; that was **wrong** — `disallowed_methods` matches resolved
  def-paths and catches inherent methods. See `C26`.

Both are signature-preserving. Neither is a one-liner.

## Two carve-outs: decisions, not rewrites

1. **`fs::canonicalize`** — 5 sites in `uu_ls`. Under a virtual root this must
   return a *virtual* canonical path. That is a semantics call (and it is the
   same one `cd -P` / `pwd -P` force in `brush-builtins`), not something a
   codemod can decide.
2. **`uucore::fsxattr::has_acl`** (`display.rs:37`, used at `display.rs:971`,
   `:1370`) → `xattr::list_deref` on a path. **cap-std has no xattr API.** This
   is `C4` confirmed concretely in the second utility examined, not a
   theoretical dependency-tree concern. Either `ls -l`'s ACL indicator is
   dropped, or xattr access needs its own path-resolution route.

## The find that changes D13

**`uucore` already ships the abstraction point.**
`uucore-0.10.0/src/lib/features/safe_traversal.rs` is 1,464 lines implementing
`DirFd` over `openat`/`fstatat`/`unlinkat`/`mkdirat`/`fchmodat`/`fchownat`, with
`SymlinkBehavior`, `open_subdir`, `stat_at`, `metadata_at`, `read_dir`,
`open_file_at`, and `create_dir_all_safe`. Its header says: *"TOCTOU-safe
filesystem operations for recursive traversal."*

Upstream has independently built a dir-fd-anchored filesystem layer for the same
reason we want one. Three qualifications: it is **Unix-only**, it is `nix`-based
rather than cap-std, and **`uu_ls` does not use it** — it is currently adopted
only where TOCTOU mattered enough (recursive delete/copy paths).

This reframes D13's upstream ask substantially. It is no longer "please accept a
new filesystem abstraction into uucore" — a large, speculative proposal — but
"please route filesystem access through the abstraction you already built, and
let its root be injectable." That is a far easier sell, it aligns with a goal
upstream already holds, and every utility migrated to `DirFd` upstream is a
utility our codemod no longer has to touch.

**Action: rewrite D13's upstream plan around `safe_traversal::DirFd`.**

## Verdict

D34's hypothesis survives. The fork can be generated. But three things must be
written down that were not:

1. `brush-vfs` presents a **path-based, std-typed** API; `Dir` handles never
   escape it.
2. The codemod needs an **inherent-method visitor**, not just a path visitor,
   and that half is the majority of the work.
3. `canonicalize` semantics and xattr/ACL access are **owner decisions** that
   block the codemod on those sites.

Cost of the spike: one session. It would have been considerably more expensive
to learn (1) after building a `Dir`-handle API.
