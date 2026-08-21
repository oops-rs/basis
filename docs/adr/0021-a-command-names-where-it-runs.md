# 0021 — A command names where it runs

> Status: Accepted · 2026-08-21
> Extends [`0016-one-delegation-surface.md`](0016-one-delegation-surface.md)
> — the parsed call gains a dimension, the number of doors does not —
> and [`0018-the-runtime-owns-the-process.md`](0018-the-runtime-owns-the-process.md)
> (an executor is process infrastructure, so a target is registered where one is);
> related: [`0013-the-host-owns-the-boundary.md`](0013-the-host-owns-the-boundary.md).

## Context

Every command a model runs today lands in one place. `RuntimeBuilder::build_with`
installs at most one executor — `EnvironmentExecutor` wrapping mentra's
`LocalRuntimeExecutor` (`basis/src/runtime/environment.rs`) — and
`RuntimeHandle::execute_shell_command`, which is the call `spawn` makes, reaches
exactly that one. *Where* is not a question the design has ever had to answer,
because there was one answer.

A host has arrived for whom there are two. basis runs inside a Linux container
on a macOS build machine: the repository is mounted, `cargo test` and `rg` and
`git` belong in the container, and a handful of commands do not exist there at
all. `xcodebuild`, `simctl`, `notarytool` and the code-signing keychain are on
the Mac, on the other side of the container boundary, reachable only through
something the host operates — in their case SSH to a forced command. The work
is one task. The commands are not one place.

The shape that suggests itself first is a second tool: `spawn` for here,
`host_spawn` for there. That is exactly the two doors ADR-0016 closed, rebuilt
by hand. Two names mean two side-effect declarations, two names at basis's
`ApprovalGate`, and — because remembered rules key on `{tool_name, pattern}` —
two rule namespaces for what an operator thinks of as one question: *may this
agent run this command*. Any per-command review would have to be built once per
door, and an operator who denied `curl … | sh` on one would find it allowed on
the other. The reason the first pair of doors was collapsed applies unchanged
to the second pair, and nothing about the new demand is a counterargument; it
is the same act with one more fact attached to it.

So the fact goes on the act. *Where a command runs* is a dimension of a `spawn`
call, in the same way *what mode it is* already is — read once at the boundary,
carried as a typed field, and dispatched on by everything downstream.

There is a second question underneath, and it is the one that decides how much
basis may build. An executor that reaches a Mac from inside a container is a
piece of a specific host's environment: which key, which user, which host, what
the far side re-validates, whether the far side is a machine at all. basis
cannot write that and cannot audit it. ADR-0013 already settled how basis
behaves when the honest thing is a pattern rather than an implementation — it
withdrew the Dockerfile and shipped `docs/containerization.md` — and the same
ruling applies here.

## Decision

**A command may name where it runs: `!@<target> <command>`. basis routes; the
host supplies the executor; basis claims nothing about what is on the far side.**

1. **One parse, still at the edge, still exactly once.** `!@name ` is read by
   the same function that reads `!` (`tools::spawn::parse`), and the typed call
   it produces gains one field: `Spawn { mode, body, target: Option<String> }`,
   where `None` is *here*. No consumer downstream re-inspects the string — the
   rule that made the `!` sugar safe is what makes this sugar safe, and it is
   the reason the target is a prefix rather than a second schema field the model
   would have to decide about on every call.

2. **The wire contract becomes `{mode, body, cwd, target}`**, with `"local"`
   where no target was named. Additive, in that word's strict sense: the three
   existing keys keep their spellings, their values and their order, so every
   remembered rule an operator has already written keeps matching exactly what
   it matched before. What is new is that the routing decision is *data* — the
   approver renders it, the audit trail keeps it, and a remembered rule can be
   written against it, with the caveat the Consequences make explicit and this
   ADR will not bury: reaching it in a glob means naming the `cwd` too.
   `"local"` rather than `null` or an absent key, because *local* is a value an
   operator will want to write a rule about, and a glob against a JSON `null`
   is a spelling that invites a mismatch nobody sees.

3. **Targets are registered on the runtime, by name.**
   `RuntimeBuilder::with_command_target(name, executor)`. Runtime scope is
   ADR-0018's answer and not a new one: an executor is process infrastructure,
   exactly as the fixed command environment beside it is, and a target that
   changed per repository would be a different machine per repository, which is
   not a thing a repository knows. The names live in one map; a later call with
   the same name replaces the earlier one, the same rule
   `with_command_environment` follows.

4. **An unregistered name is refused before the approver.** It reaches the
   model as `Tool execution denied: …`, naming the targets that do exist, and
   it never becomes a question for a person — the same ruling as the delegation
   depth floor, for the same reason: a routing destination that does not exist
   is not a thing an allow-rule should be able to conjure. It is refused a
   second time in `execute_mut`, because the preview is only reached when an
   authorizer is installed and a floor a missing authorizer removes is not a
   floor.

5. **`cwd` is advisory for a target.** basis resolves the working directory as
   it always has and puts it in the request, because an approver cannot judge a
   command without knowing where it was meant to run. What the path *means* on
   the far side is the executor's to decide: the same repository may be mounted
   at another prefix, or checked out separately, or not exist at all. basis does
   not translate it, does not verify it, and does not pretend the resolved path
   is a path on the target.

6. **basis ships no executors.** ADR-0013's posture, applied to the same class
   of problem: the pattern is documented (`docs/targets.md` — a worked SSH
   forced-command example, what the executor receives, and what it is
   responsible for) and no instance is shipped. A host that registers no target
   is unchanged in every respect, including that no target vocabulary is
   mentioned to the model.

7. **A targeted command is still `Mode::Command`.** Every guard that already
   applies to a command applies to this one, on the same path and in the same
   order: the dispatcher's `ShellAccess::Denied` refusal, the policy's
   `allow_shell_commands(false)`, the approver, the rule store, the hooks. There
   is no route by which naming a target reaches an executor that a plain command
   could not have reached — `--no-shell` shuts off `!@mac ls` exactly as it
   shuts off `!ls`.

8. **The model is told about `!@` only when a target is registered.** The tool's
   description and its input schema name the prefix and the registered target
   names when there is at least one, and say nothing about either when there is
   none. A model must not be taught a door that does not exist: the best case is
   a wasted call, and the worse case is a model that reads an unexplained
   refusal as an invitation to guess names.

9. **Background tasks are local-only in this release.** mentra's
   `start_background_task` takes no target and `spawn` does not start one; the
   dimension is added where the demand is, and widening it later is additive.

## Consequences

- **A target reaches whatever that executor's process can reach, and basis
  never calls it anything else.** Not "the host", not "outside the container",
  not "sandboxed". A `docker exec` or `nsenter` target on Docker Desktop reaches
  the Linux VM the daemon runs in, not macOS, and a host who believed otherwise
  would have written the honest-sounding thing and got the other one. A target
  is as trusted as the executor the host wrote, and the authority it grants is
  that executor's, whatever basis's docs say. This is ADR-0013's honesty clause
  with a second surface to apply to, and the reason `docs/targets.md` says it in
  its own words rather than linking to them.
- **Hosts gain one builder call and lose nothing.** A host that registers no
  target sees the same executor stack it saw before, the same description, the
  same wire contract minus one key that reads `"local"`.
- **Approvers that read `structured_input` by field are unaffected; one that
  compares the whole object is not.** `basis/examples/reviewed_shell.rs` reads
  by field and is the shape basis documents. An approver that wants to answer
  differently per destination now can, and one that ignores the key keeps
  answering as it did — which is the same trade the `cwd` key made when it was
  added.
- **The rule collapse ADR-0016 named gets one more dimension.** A bare rule on
  `spawn` — no pattern — now covers delegations, local commands and every
  target at once. Telling them apart was already a matter of writing a pattern
  against the parsed call, and the new key is one more thing such a pattern can
  say. More expressive, less obvious, exactly as before.
- **An operator can be surprised by which machine a remembered allow covers.**
  `**"body":"rm -rf build"**` written while thinking about a container now
  matches the same command on the Mac. The mitigation is that the target is in
  the same serialized object and can be pinned in the same pattern; the cost is
  that it has to be, deliberately.
- **And pinning it costs more than it should, because of a trap that predates
  this ADR.** mentra globs a rule pattern against the serialized input with
  `glob-match`, which is a *path* matcher: **no wildcard crosses `/`**, `**`
  included unless it stands as a whole path segment. Keys serialize in order,
  so `cwd` — an absolute path, full of slashes — sits between the start of the
  string and both `mode` and `target`. `**"target":"mac"**` therefore matches
  nothing at all, silently, and the operator sees a reviewer they believed they
  had bypassed rather than an error. The spelling that works names the
  directory:

  ```
  **"cwd":"/work/repo","mode":"command","target":"mac"}
  ```

  which for a rule that grants a whole machine is the stricter thing to write
  anyway. This is not new — ADR-0016 already claimed a pattern could match the
  parsed `mode`, and for the same reason it cannot — but ADR-0021 is the first
  decision to *depend* on the claim, so it is recorded here and pinned by a
  test rather than left as folklore. It is mentra-shaped under
  [ADR-0005](0005-mentra-coevolution-discipline.md): a matcher meant for paths
  is being run over JSON, any harness storing structured rules would hit it,
  and the fix belongs upstream.
- **The docs churn is real and load-bearing.** The `!` convention is documented
  in the README, in `ARCHITECTURE.md`'s hooks migration section, and in the
  tool's own description, and a hook that wants to distinguish commands by
  destination reads the same parsed input everything else does.
- **What this does not decide.** No per-workspace targets (a host that wants two
  workspaces to differ gives each its own runtime, the same answer the command
  environment gets). No discovery surface beyond the names in the description.
  No fan-out — one command names one place. And no basis-shipped executor for
  any transport, which is point 6 and not an omission.
