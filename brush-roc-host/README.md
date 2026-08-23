# brush-roc-host

The Rust host for seahaven's Roc platform (step 9 of
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

- **The broker-backed `Executor` is wired.** `main` is this binary's own bundled
  trampoline (D30): it registers the coreutils (`brush_shell::bundled`) and takes
  the `--invoke-bundled` dispatch fast path before any Roc code, and the
  installed session carries a `brush_broker_exec::BrokerExecutor` over the same
  namespace. So a confined `Cmd` runs a bundled utility by re-invoking this
  binary, served this session's mounts (D24) — the executor itself is proven end
  to end in `brush-broker-exec`'s tests.

## The link, end to end

rocjust builds against this platform and runs. Point rocjust's `app/main.roc`
at `../../flatland/platform/main.roc`, then:

    cargo xtask sysroot                      # macOS only, once (see below)
    CARGO_PROFILE_RELEASE_LTO=off cargo build --release   # -> target/release/libhost.a
    cp target/release/libhost.a ../platform/targets/arm64mac/libhost.a
    cd ../../rocjust/app && roc build --opt=dev main.roc

The result runs `just` confined: parsing, evaluation and the CLI all work; a
recipe's commands run through the embedded brush shell (bundled utilities and
builtins), and an arbitrary external program is refused (D2). Against upstream
just's suite it scores **1706 / 1834** — the ~128 failures are the boundary
itself (interactive `choose`/`confirm`, recipes shelling out to real tools).

### The macOS sysroot

`cargo xtask sysroot` builds `platform/targets/macos-sysroot`: framework TBD
stubs symlinked from the local SDK. roc's linker (`findPlatformSysroot`)
auto-adds `-framework X` for each framework present, which is how a native
framework gets linked (there is no `roc build` flag for it). The host reaches
CoreFoundation (`chrono -> iana-time-zone`, via `reedline`), so it is declared
there. The symlinks are machine-local (gitignored); rerun after an SDK update.

`--opt=dev` and `LTO=off` keep roc's compile and the archive link within a
constrained machine's memory; a roomier host can drop both.

## Regenerating the glue

    roc glue <RustGlue.roc> src/ ../platform/main.roc
