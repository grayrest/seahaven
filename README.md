# seahaven

A platform for building constrained [Roc](https://www.roc-lang.org/) CLI applications.
Roc apps, on their own, have no access to the outside world. If we give them a fake
POSIX environment that can only write to a fixed set of directories or run
a fixed set of programs in theory there's no way for them to break out. This
platform was built with [roc-just](https://github/grayrest/roc-just) as the initial
client. The idea was that if a build system can work on the platform then most
other apps should be expressable. The api is very close to `basic-cli` but it
currently lacks sqlite and network features.

seahaven is a fork of [`brush`](https://github.com/reubeno/brush), a bash- and
POSIX-compatible shell in Rust. brush is the substrate: its parser, builtins,
and a bundled set of coreutils become the confined command surface an app runs
against, so a Roc program can shell out — run a recipe, a pipeline, a utility —
without the ability to run *arbitrary* programs or touch files outside its
namespace. 

## Is this actually secure?

**No.** It's an exploration of what a constrained platform would need to look like.
The author is not a security expert and this has not been tested in earnest.

## What confinement means today

- **A composed virtual root.** This project builds on wasmtime's `cap-std` virtual
  file system. By default the platform is constrained to the nearest parent with
  a source control root. Additional access policies are allowed including an
  identity policy that allows full access to everything (used for testing brush
  compatibility). The platform works with the real filesystem within its virutalized
  root.

- **A closed world of execution.** Under `--closed-world` (the default) there is no
  arbitrary external execution: the only things that run are the bundled
  utilities inside the binary and the app re-invoking *itself*.

- **Bundled utilities.** `cat`, `ls`, `grep`, `find`, `sed`, and the
  rest ship inside the binary as forks of `uutils/{coreutils,findutils,grep}`
  and `sed`, with their filesystem access rewritten to the vfs by a
  signature-preserving codemod.

- **A session broker.** A utility runs in its own child process; the
  parent hands it the confined namespace over an `SCM_RIGHTS` handshake,
  authenticated by kernel peer credentials, so the child is confined too
  rather than falling back to ambient authority.

## The pieces

| Crate | Role |
| --- | --- |
| `brush-vfs` | The virtual filesystem: mounts, the virtual-path grammar, the session handle, and the `ambient` façade that routed utility code calls. |
| `brush-platform` | The `PlatformEffects` trait every hosted effect routes through, and `VfsPlatform`, its vfs-backed implementation with session facts and an executor. |
| `brush-roc-host` | The Roc host: a `staticlib` the Roc compiler links, marshalling Roc's refcounted `Str`/`List`/`OsStr` and forwarding all 55 hosted effects into `brush-platform`. |
| `brush-broker-exec` | The broker-backed `Executor`: spawns a confined child and serves it the session (D24). |
| `brush-shell` | The shell entrypoint, the bundled-command registry and dispatch trampoline (D30), and the confined recipe runner. |
| `brush-core`, `brush-parser`, `brush-builtins` | brush's shell engine, parser, and builtins. |
| `brush-coreutils-builtins`, `forks/` | The bundled utilities and their generated forks. |
| `platform/` | The Roc side of the platform: `main.roc`, `Host.roc`, and one module per exposed name (`Path`, `Cmd`, `Env`, the stream modules, …). |

## The Roc platform surface

This is intended as a *mostly* drop-in replacement for `basic-cli`. The main
API difference is that paths are `Str` because the scripting environment is
intended to be cross platform. This means there are paths that are inaccessible
on some platforms and those paths are dropped/invisible.

Missing features:

* `Http` - no network support yet
* `Tcp` - no network support yet
* `Url` - no network support yet
* `Sqlite` - Removed in the interest of reducing API surface
* `Sleep` - Not in use by the current client projects
* `Locale` - Not in use by the current client projects
* `File` - Not in use by the current client projects

## Building

The confined host and its Roc link are documented in
[`brush-roc-host/README.md`](brush-roc-host/README.md). In brief, on macOS:

```sh
cargo xtask sysroot                                     # framework stubs the roc linker consumes (once)
CARGO_PROFILE_RELEASE_LTO=off cargo build --release     # -> libhost.a
# then `roc build --opt=speed` an app against platform/main.roc
```

`cargo xtask check` builds the workspace, the excluded host, and the platform so
none of them rot.

## Relationship to brush, and license

seahaven tracks brush and preserves its behaviour on the default, unconfined
(identity) path — brush remains a working bash/POSIX shell, and confinement is
opt-in axes (`--closed-world`, `--mount`) layered on top. Credit for the shell,
its parser, and the compatibility suite belongs upstream to
[brush](https://github.com/reubeno/brush).

MIT licensed, like brush — see [`LICENSE`](LICENSE).
