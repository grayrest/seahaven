# The residual patch set

D13's model is that forks are **generated, not maintained**: `cargo xtask
vendor-fork` re-vendors pristine upstream and re-applies the codemod, and the
size of what is left over per rebase is the health metric.

This file is that leftover. Everything listed here is hand-written, is **not**
reproduced by re-running the tooling, and is therefore **lost by a re-vendor** —
which happened once during this milestone and was caught only because
`cargo xtask check forks` went red. Re-apply from git after regenerating a fork,
and treat growth in this list as the signal it is meant to be.

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

## `uucore/src/lib/features/fs.rs` — `rustix` stat, and the cwd base

`FileInformation::from_path` used `rustix::fs::{stat,lstat}`; the type now uses
the `std::fs::Metadata` representation it already had for WASI. Separately,
`canonicalize` built its absolute base from the *host* process cwd while
checking existence against the namespace — now `ambient::current_dir()`.

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

## `findutils` — an exemption and a known divergence, not a patch

`find` is **not registered** in the bundled command set. Its `std::fs` sites are
routed, but its traversal is `walkdir`, which reads directories itself and is
banned in `deny.toml` for that reason. An unconfined `find` is worse than no
`find`. `known-test-failures.txt` records the one upstream test that diverges
under routing, macOS only.

## `uu_df` — an exemption, not a patch

Vendored with `--skip src/filesystem.rs`. That module canonicalizes mount
*device names* out of the host mount table: host introspection, not namespace
access, the same class as `uucore::fsext`. Routing it makes `df` report nothing,
which upstream's `test_dev_name_match` catches with `MountMissing`. `df` is
therefore **not confined**, by decision.
