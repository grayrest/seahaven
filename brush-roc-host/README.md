# brush-roc-host

The Rust host for flatland's Roc platform (step 9 of
`plans/2026-08-22-platform.md`). A `staticlib` the Roc compiler links with a
compiled Roc app to produce the final binary.

Excluded from the cargo workspace, like `forks/`: it is linked by `roc` rather
than by cargo, and its generated glue (`src/roc_platform_abi.rs`, from `roc
glue`) is unsafe-heavy `extern "C"` code the workspace lint wall is not written
for. `cargo xtask check platform` builds it so it does not rot.

## What is here (the foundation, and it compiles)

- The generated ABI glue, vendored from `roc glue`.
- The Roc runtime symbols -- `roc_alloc` and friends -- via libc.
- `main`, which installs a session and calls the compiled `roc_main`.
- The **scalar effects** (no refcounted arguments): `env_pid`, `env_num_cpus`,
  `env_tz_offset`, `signal_install_handler`, `signal_take`, and the `tty_*`
  constants. Each routes through `brush-platform`, proving the effect-wiring
  pattern against the real generated types.

## What remains (the bulk of step 9)

- The **refcounted effects** -- everything taking or returning a Roc `Str` or
  `List`: the filesystem, stdio, env-string and `Cmd` effects. Each follows the
  scalar pattern with `RocStr`/`RocList` marshalling and the per-argument
  refcount discipline the glue documents.
- The **real argument list**: `main` currently passes an empty `RocList<OsStr>`;
  marshalling `argv` is the same `List` marshalling the effects need.
- The **broker-backed `Executor`** (D2's predicate, D24's broker), so `Cmd`
  actually runs a confined child.
- The **link and run**: producing the platform's `targets/libhost.a`, building
  rocjust against it, and running its differential harness.

This is dominated by ABI marshalling that is only validated at runtime under the
linked binary, which is why it is staged rather than landed in one pass.

## Regenerating the glue

    roc glue <RustGlue.roc> src/ ../platform/main.roc
