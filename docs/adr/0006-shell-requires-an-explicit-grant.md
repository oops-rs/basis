# 0006 — Shell execution requires an explicit grant

> Status: **Superseded by [`0013-the-host-owns-the-boundary.md`](0013-the-host-owns-the-boundary.md)** · 2026-08-11
> (Accepted 2026-08-10.) Shell is now enabled by default; the shipped image and
> its special-cased grant are withdrawn. The `.git` carve-out and the
> argument against environment inference survive in 0013.
> Extends [`0004-kernel-enforced-confinement.md`](0004-kernel-enforced-confinement.md).

## Context

P1 shipped with `RuntimePolicy::workspace_bounded`, which leaves mentra's
`allow_shell_commands` at `false`. The effect went unnoticed until a live run
was inspected: the model calls `shell`, gets

```
Shell command execution is disabled by the runtime policy.
```

and burns turns recovering. A coding-agent harness that cannot run `cargo
test`, `git log`, or a build can do very little real work, so this is not a
detail — it is most of the value.

mentra is right to default it off. Its own note says a working-directory check
cannot confine a process that runs on the host: once a command executes, the
policy's path roots are advisory. That is ADR-0004's position restated from the
runtime's side.

So the question is not *whether* to enable shell but **who is entitled to say
the boundary exists**.

Three candidates:

1. **basis infers it** from `mentra::detect_environment()`, which reports
   `Host` / `Docker` / `Container` / `ContinuousIntegration`.
2. **The operator states it**, explicitly, per run.
3. **The image author states it**, once, for an environment they built.

Option 1 is tempting and wrong. Detecting Docker proves basis is *inside* a
container; it proves nothing about how that container was run. `docker run -v
/:/host` is a container with no boundary at all. Inferring a guarantee from a
signal that does not establish it is exactly the "selling in-process checks as
safety" that ADR-0004 refuses — it would just move the pretence one layer out.

## Decision

**Shell and background execution are denied unless explicitly granted.** The
grant is an act by whoever knows the boundary holds, never an inference by basis.

- `RunConfig::shell` defaults to `ShellAccess::Denied`.
- `basis run --allow-shell`, or `BASIS_ALLOW_SHELL=1`, grants it.
- basis's Docker image sets `BASIS_ALLOW_SHELL=1` in its own environment. That is
  the image author granting it for an environment they control: read-only root
  filesystem, workspace as the sole writable mount. The grant travels with the
  thing that makes it true.
- When shell is granted and `detect_environment()` reports `Host`, the run
  emits a warning notice. Detection is used to *inform*, never to decide.

## Consequences

- The default stays safe and the failure mode stays legible: a user who wants
  commands is told to ask for them, rather than discovering the limit through a
  model's confused recovery.
- `basis run --allow-shell` on a laptop is a real grant of real authority, and
  says so. That is the honest shape: the user, not basis, is asserting that
  losing the workspace is acceptable.
- Inside the official image, shell is on with no flag, because there the
  boundary is the kernel's and the image author can vouch for it.
- A host embedding the crate gets `ShellAccess::Denied` unless it opts in, so
  no embedder inherits command execution by surprise.
- ~~`.git/hooks` write-deny is not expressible~~ — **closed.** mentra 0.17 added
  `RuntimePolicy::with_denied_write_root`, and basis denies `.git/hooks` and
  `.git/config` by default. Verified live: the builtin `files` tool is refused
  with a reason the model reads and acts on. The limit is equally verified —
  `sh -c 'echo hi > .git/hooks/pre-commit'` still lands, because nothing parses
  shell. Hygiene that closes the route a model takes, never a boundary.
- The original note, kept because the reasoning still holds for anything else
  of this shape: `RuntimePolicy` had allow-roots but no deny-roots. Recorded here
  so it is not mistaken for done; it needs a mentra change or the native
  sandbox of [`proposals/0002`](../proposals/0002-native-sandbox.md).
