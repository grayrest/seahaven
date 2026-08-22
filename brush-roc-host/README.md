# brush-roc-host

The Rust host for flatland's Roc platform (step 9 of
`plans/2026-08-22-platform.md`). A `staticlib` the Roc compiler links with a
compiled Roc app to produce the final binary.

Excluded from the cargo workspace, like `forks/`: it is linked by `roc` rather
than by cargo, and its generated glue (`src/roc_platform_abi.rs`, from `roc
glue`) is unsafe-heavy `extern "C"` code the workspace lint wall is not written
for. `cargo xtask check platform` builds it so it does not rot.

## What is here (the whole marshalling layer)

- The generated ABI glue, vendored from `roc glue` (`src/roc_platform_abi.rs`).
- The Roc runtime symbols -- `roc_alloc` and friends -- forwarding to the glue's
  `DefaultAllocators`, so the exported symbols Roc calls and the host-side glue
  helpers share **one** allocator. (A host-built value freed by Roc, or the
  reverse, must meet the same allocator or the free reads a header that is not
  there.)
- `main`, which installs a session and calls the compiled `roc_main` with a real
  `argv` marshalled into a `RocList<OsStr>`.
- **All 55 hosted effects**, defined and exporting:
  - the **scalar** ones in `lib.rs` (`env_pid`, `signal_take`, the `tty_*`
    constants, ...), and
  - the **refcounted** ones -- everything taking or returning a Roc
    `Str`/`List`/`OsStr` -- in `fs_effects`, `env_effects`, `io_effects`,
    `cmd_effects`, `misc_effects`. Each unmarshals its owned arguments (releasing
    each exactly once), calls the session through `brush-platform`, and marshals
    the result back. The three crossing-rules live once in `marshal`.
- **Host-side round-trip tests** (`src/host_tests.rs`): each builds the owned Roc
  value the compiler would pass, drives the effect, and reads the result back.
  They catch structural marshalling bugs and -- through the system allocator's
  abort on a double free -- release-discipline bugs. Run with `cargo test`.

## What remains (the link)

- The **broker-backed `Executor`** (D2's predicate, D24's broker), which crosses
  into `brush-core`; until it is wired, `Cmd` execution is `Unsupported`,
  uniformly.
- The **link and run**: producing the platform's `targets/libhost.a`, building
  rocjust against it, and running its differential harness end to end.

The remaining piece exercises the **true ABI** -- the calling convention across
the language boundary, and leak-freedom under a real workload -- which the
host-side tests reach up to but not through. Miri over `cargo test` is the
intended leak gate for the marshalling once the glue's manual allocation
arithmetic is vetted against it.

## Regenerating the glue

    roc glue <RustGlue.roc> src/ ../platform/main.roc
