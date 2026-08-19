# 0015 — The CLI grammar: shorthand and explicit modes

> Status: Accepted · 2026-08-11; bare-mode rule refined by
> [`0017-structured-agent-concurrency.md`](0017-structured-agent-concurrency.md) · 2026-08-13;
> shorthand meaning restored by
> [`0020-spawn-routing-is-decided-by-the-environment.md`](0020-spawn-routing-is-decided-by-the-environment.md) · 2026-08-16
> Extends [`0002-acp-is-the-protocol.md`](0002-acp-is-the-protocol.md) and
> [`0007-acp-sessions-and-the-dispatch-loop.md`](0007-acp-sessions-and-the-dispatch-loop.md).

> **Refinement:** ADR-0017 supersedes the no-subcommand server rule. The
> explicit `-` stdin spelling and exit-code contract below remain; current ACP
> entry points are `basis serve --acp` and `basis serve --bridge`.
>
> **Correction (ADR-0020):** an earlier version of this note also claimed the
> positional prompt shorthand was untouched. That was wrong. The shorthand's
> *spelling* survived ADR-0017's `run` → `spawn` rename, but its *meaning* did
> not: "run it and print the answer" silently became "enqueue it and print a
> handle". ADR-0020 restores the original meaning at a shell and names the rule
> that decides it, so the sentence below — "the human path is the shortest
> path" — is true again.

## Context

With `watch` retired the binary has two real modes — the explicit ACP server and
the one-shot run — plus utilities. The remaining friction is human: the common
human invocation (`basis run "<prompt>"`) carries a subcommand the human never
needed. The v1 machine invocation (an editor spawning plain `basis`) is retained
in the history below; ADR-0017 makes the server transport explicit so a bare
prompt cannot become a long-lived server by accident.

The v1 trap needed deciding rather than discovering: bare `basis` spoke JSON-RPC
on stdio, and an editor connecting looked exactly like a shell pipe — non-TTY
stdin, no args. So `cat prompt.txt | basis` could not be auto-detected as "prompt
on stdin" without breaking the thing editors relied on. TTY-sniffing cannot
distinguish the two; nothing can, from the process's seat. ADR-0017 resolves the
ambiguity by making both operations explicit.

## Decision

The v1 grammar was:

```
basis                      # ACP server on stdio — v1 behavior, superseded by ADR-0017
basis "<prompt>" [flags]   # shorthand: identical to basis spawn
basis run "<prompt>"       # one-shot; `-` as prompt reads stdin
basis bridge               # websocket relay — v1 spelling, superseded by `serve --bridge`
basis fingerprint          # workspace hash, for skip-if-unchanged scripts
```

- **A positional argument that is not a known subcommand is a prompt**, and
  the invocation is `basis spawn` with that prompt. Flags pass through
  (`basis --json "hi"` ≡ `basis spawn --json "hi"`); `--` escapes a prompt that
  collides with a subcommand name.
- **The bare-server rule above is historical.** Current invocations are
  `basis` (usage), `basis spawn <PROMPT>` (or the retained `basis run` alias),
  `basis spawn -` for prompt stdin, and `basis serve --acp` / `basis serve --bridge`
  for ACP. The explicit server command keeps the two stdin meanings separate.
- **Exit codes are contract:** `0` success, distinct nonzero codes for run
  failure and for a tripped bound, so shell callers can branch without
  parsing. `--json` remains the structured detail.

## Consequences

- The human path is the shortest path (`basis "fix the failing test"`), the
  editor path names its transport (`basis serve --acp`), and the script path gets
  codes and `--json`. No mode detection, no cleverness — every ambiguity is
  resolved by explicit syntax.
- A prompt equal to a subcommand name (`basis run` the word) requires `--`;
  accepted cost, held to five reserved words.
- The grammar is the whole CLI. Any future subcommand proposal starts from
  "which of the two audiences needs this, and why is it not the host's code?"
