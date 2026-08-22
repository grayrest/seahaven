# flatland's Roc platform

The Roc side of the platform milestone (`plans/2026-08-22-platform.md`, step 7):
`main.roc` (the platform header), `Host.roc` (the hosted-effect declarations),
and one module per exposed name. rocjust builds against this in place of
`basic-cli`.

## Provenance

The module sources (`Path.roc`, `Cmd.roc`, `Env.roc`, the stream modules,
`OsStr.roc`, `IOErr.roc`, `Regex.roc`, `Signal.roc`, `Tty.roc`, `Random.roc`,
`Utc.roc`, `InternalDateTime.roc`) are adapted from the maintainer's `basic-cli`
fork, which flatland exists to replace as rocjust's platform. `Host.roc` and
`main.roc` are trimmed to flatland's confined surface: the network effects are
gone by D9, and `sqlite`, `sleep`, `locale`, `http`, `tcp` and the file-reader
handle are simply not in rocjust's surface. What remains is the ~50 effects the
thirteen exposed modules reference.

## What checks this

`cargo xtask check platform` runs `roc check platform/main.roc`, which
type-checks the platform standalone -- no built host, no link. Like `forks/`,
these files are invisible to `cargo`, so they need their own gate or they rot.
The gate skips with a note when `roc` is not installed.

## What is not here yet

The Rust host (`hosted_*` and `roc_alloc`/`roc_crashed`, marshalling Roc's
refcounted `Str`/`List` and routing every effect through `brush-platform`), the
`roc glue` step, the `staticlib` link, and running rocjust against it -- that is
step 9.
