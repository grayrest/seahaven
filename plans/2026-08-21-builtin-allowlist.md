# flatland: a default-deny builtin allowlist

D11. Every boundary built so far governs what code may *reach* — the vfs
decides what a builtin may open (D3, D6), and the closed world decides what may
be executed (D2). Neither says anything about what a builtin may *do* once
running, and the shell's own surface is the part nothing has touched.

**Status: implemented.** See "Where it landed" at the end for what was built,
what was built differently from this plan, and what was not built at all.

## The escapes are real, and measured

Both were named in the decision log from a reading of two files. They are
reproduced here against the shipped binary under the *strongest* policy the
tree can express — a single writable mount and a closed world — because a claim
that they matter is worth more when the configuration meant to stop them is
switched on.

`kill`, given a bare number, signals any process on the host:

```
$ /bin/sleep 300 &                       # a host process, not in the job table
$ brush --mount /work:$PWD/jail:rw --closed-world -c "kill -TERM $VICTIM"
brush-exit=0
VICTIM KILLED FROM INSIDE THE SANDBOX
```

`brush-builtins/src/kill.rs:122-125` resolves a job spec through the job table
and, failing that, parses the argument as a PID and hands it to
`sys::signal::kill_process`. There is no second check.

And an exported function in the environment becomes a shell function — the
Shellshock shape, at `brush-core/src/wellknownvars.rs:28`:

```
$ env 'BASH_FUNC_pwned%%=() { echo INJECTED-FUNCTION-RAN; }' \
      brush --mount /work:$PWD/jail:rw --closed-world -c 'pwned'
INJECTED-FUNCTION-RAN
```

**Neither is fixed by an allowlist**, and saying so up front avoids the plan's
worst outcome. A recipe runner needs `kill`, so `kill` will be on the list and
its bare-PID form is still an escape. `BASH_FUNC_*` is not a builtin at all —
it is environment inheritance, which no list of builtin names reaches. The two
escapes are the *evidence* for D11's argument that badness cannot be
enumerated; they are not its work item. Both need their own fix, and this plan
carries them because leaving a measured escape unrepaired while building the
structural answer to it would be the wrong order.

## `enable -n` is not the mechanism, and this is the sharp part

The registry already carries a `disabled` flag, and it already works at every
dispatch site — measured:

```
enable -n kill
kill -TERM $V        -> command not found: kill            (127)
builtin kill -TERM $V -> builtin: not a shell builtin: kill (1)
enable kill
kill -TERM $V        -> 0        # and the victim dies
```

So the flag is honoured consistently and is **script-controlled**:
`brush-builtins/src/enable.rs:59` sets `builtin.disabled = self.disable` for
any registered name, so a sandboxed script un-does any deny expressed that way
with four characters. D11's own correction names a second reason — `shell.rs:143`
marks `builtins` `serde(skip)`, and `brush-shell/src/entry.rs:523` rebuilds the
default set unconditionally after deserializing, so a per-policy disable is
reset on the far side of D24's broker.

Two independent resets on the same field. The conclusion is not "guard `enable`"
— that is the enumerated kill list D11 rejects — but that **a denied builtin
must not be in the registry at all**.

## What is already true

- **Two registries, not one.** `Shell::builtins` holds 61 registrations from
  `brush-builtins/src/factory.rs`. The bundled utilities live in a *second*,
  process-global registry (`brush-shell/src/bundled.rs:50`, an `OnceLock`)
  holding 86 names, dispatched from `main()` on re-exec (D5) before any shell
  or policy exists. `register_shims` (`bundled.rs:342`) bridges them, installing
  one shim `Registration` per bundled name into the shell — which is why a
  shell-side check reaches both sets, and why the re-exec path does not.
- **Three dispatch reads, and they already agree.** `commands.rs:395`
  (ordinary), `builtin_.rs:36` (`builtin NAME`), `command.rs:108` (`command`'s
  lookup). All three honour `disabled`, which the measurement above confirms.
  Four more readers only *list*: `completion.rs` ×6, `help.rs:156`,
  `type_.rs:178`, `enable.rs:68`.
- **D2's predicate cannot express this.** `ExternalExecution::permit`
  (`execpolicy.rs:92`) takes `command_name` and `argv1` — the launcher's path
  and the dispatch flag. The bundled utility's name is `argv[2]`, which the
  predicate never sees, so "this policy may not run `find`" is not currently
  sayable at the exec chokepoint.
- **There is no policy value.** `brush_vfs::Policy` is a unit struct namespacing
  `MountTable` constructors; it holds nothing. But `Shell` already carries two
  serde-skipped, fail-closed policy fields — `session` (`shell.rs:79`) and
  `external_execution` (`shell.rs:88`), each documented as fail-closed on
  deserialize with the caller obliged to reinstall. That is the pattern to
  follow, not to invent.
- **Under identity, every deny branch is dead code.** D8's ratio applies here
  exactly as it did to the vfs: the 2178-case compat suite will exercise the
  *allow* half and none of the *deny* half.

## The change

**Step 0 — a builtin policy value on `Shell`.** A third field beside `session`
and `external_execution`, with their properties: `serde(skip)`, fail-closed on
deserialize, and a caller obliged to reinstall. Two variants to start —
`Open` (every registration accepted; what identity and the compat suite get)
and `Allowlist(set)`. Not stored inside `brush_vfs::Session`: the namespace
crate has no business knowing builtin names, and D11's "inside the session
blob" is a statement about *what survives the broker*, which the fail-closed
pattern already answers.

**Step 1 — enforce at registration, not at dispatch.** `Shell::register_builtin`
consults the policy and refuses a denied name, so the registry never contains
it. This is the whole reason the step is cheap: it covers all three dispatch
reads and all seven listing readers without touching any of them, and it makes
`enable NAME` answer "not a shell builtin" — the behaviour already measured
above for an unregistered name — rather than needing a guard of its own.

The load-bearing part is **ordering**: a policy installed after registration
silently does nothing. Both construction routes must install before registering
(`instantiate_shell_from_args` and `instantiate_shell_from_file`), and the
ordering needs its own assertion rather than a comment, because the failure is
silent and looks exactly like success.

**Step 2 — the re-exec path.** Extend D2's predicate to see the dispatched
utility name, so `<launcher> --invoke-bundled find` is refused by the same
predicate that already gates the launcher. Without this, a denied bundled
utility stays reachable by composing the shim command directly, and the shell's
own refusal is decoration.

**Step 3 — `kill` bound to the job table.** The bare-PID form resolves through
`jobs` or is refused; a PID the shell did not start is not a job. Under `Open`
the current behaviour is kept, so the compat suite is unaffected.

**Step 4 — `BASH_FUNC_*` refused.** Not gated on the allowlist, because it is
not a builtin. Bash itself changed this shape after Shellshock; the question
for the owner is whether to refuse it always or only under a restrictive
policy, and the plan assumes the latter to keep the compat suite intact.

**Step 5 — the list itself.** *This is an owner decision and the plan does not
make it.* Default-deny means the starting set has to be written down, and it
determines what a recipe can do far more than any code here. The plan's job is
to make the list cheap to change and impossible to bypass, not to choose it.

## Gates

1. **Positive control.** A denied builtin is absent from the registry, and all
   four routes agree it does not exist: bare `NAME`, `builtin NAME`,
   `command -v NAME`, `enable NAME`. Four routes rather than one because the
   three dispatch reads are separate code paths that happen to agree today.
2. **`enable` cannot resurrect it.** The exact bypass measured above, as a
   regression test: `enable NAME; NAME` must still fail.
3. **The re-exec path refuses a denied utility** invoked directly with the
   dispatch flag, not merely through the shell.
4. **`kill` regression.** `kill <host pid>` fails under a restrictive policy and
   the host process survives — the measurement at the top of this document,
   inverted. `kill %1` still works.
5. **`BASH_FUNC_*` defines nothing** under a restrictive policy.
6. **The compat suite is unaffected** under identity, where the policy is
   `Open`. D8's shape.
7. **The gate is not vacuous.** Assert the registered-builtin count actually
   drops under the restrictive policy. Four of the foundation milestone's nine
   gates could not fail when first written; that is the failure mode to design
   against, not to discover afterwards.

## Risk

**A list rots, and default-deny rots in the safer direction.** A newly added
builtin is denied until someone adds it, which is an annoying failure rather
than a silent hole — but the error must say *why* a builtin is missing, or the
next person debugs a phantom.

**Registration-time enforcement moves the bug from dispatch to ordering.** The
check itself is trivial and hard to get wrong; installing the policy too late
is easy to get wrong and produces no symptom under identity. Gate 7 exists
specifically for this.

**Step 2 widens D2's predicate**, which is the one piece of the closed world
that has already survived adversarial review. Widening a predicate that
currently takes two arguments and answers a yes/no is where a confused deputy
would be reintroduced; the argument to add is the *name being dispatched*, not
a caller-supplied claim about it.

## What stays behind

D21's environment policy beyond the `BASH_FUNC_*` case; D24's broker, which is
what makes "inside the session blob" more than a fail-closed default; D29's
approval store, which is how a policy's list would ever be widened by consent
rather than by editing a constant.

## Where it landed

Written after the fact. The plan above is left as written so the difference is
visible.

### Built as planned

Steps 0–5 and gates 1–7. `BuiltinPolicy` sits beside `Session` and
`ExternalExecution` on `Shell` with their properties — `serde(skip)`,
fail-closed, caller reinstalls. Enforcement is at registration, covering both
insertion routes: the builder's map is filtered in `Shell::new`, and
`register_builtin` / `register_builtin_if_unset` refuse a denied name, which is
what reaches `register_shims`. D2's predicate gained `argv2` and the policy, so
`<launcher> --invoke-bundled NAME` is refused for a denied `NAME`. `kill`'s
bare-PID form resolves through the job table, `BASH_FUNC_*` is dropped, and
`--restrict-builtins` selects the list.

The ordering hazard was removed rather than asserted about.
`Shell::set_builtin_policy` prunes the registry as it installs, so a policy
arriving after registration is still correct — which the deserialize path needs,
since it re-registers the default set on a shell that fails closed. The builder
field still exists and is still the right place, because `BASH_FUNC_*`
inheritance is decided during construction and a later install would arrive too
late for that one decision.

### Built differently

**`--deny-builtin` was added, and it is not a convenience.** The bundled
utilities are admitted wholesale, because which of them exist is a build-time
question and a hard-coded copy of that list would rot toward a shell that cannot
run `cat`. That left no way to deny a bundled utility at all, which would have
made D11's claim about "five forked projects" unrealizable and gate 3
unreachable from the shell.

**The list denies 27 of 63 shell builtins**, and the reasons are recorded by
class rather than per name. `exec` is allowed, against the instinct: its program
form is already governed by D2's predicate, and denying it would only remove
`exec 3>&1`.

### Built after the fact

**A case pinning that denial without a closed world is not denial.** Removing a
builtin promotes the name to an external lookup, so `--restrict-builtins
--deny-builtin find` runs the host's `/usr/bin/find`. That reads as a bug and is
not one — the builtin policy governs which builtins exist, not what may be run —
but it is exactly the wrong assumption to leave a reader holding, so it is
asserted in both directions and stated on the flag.

### Not built

**Nothing bounds a permitted builtin beyond `kill`.** The two escapes D11 names
are fixed; the argument that motivated them — that a permitted builtin can still
do too much — applies to others that were not audited.

**`is_open()` is the wrong question, knowingly.** `kill` and `BASH_FUNC_*` ask
the builtin policy whether it is restrictive because that is the closest thing
the shell has to "am I sandboxed". When D24 gives a session a policy object,
that is where both belong.

**The escapes were re-measured but the child is still unconfined.** A bundled
utility dispatched through the shim runs in a fresh process under the identity
namespace, so `--mount` does not reach it — D2 already records this as D24's
job, and nothing here changes it.
