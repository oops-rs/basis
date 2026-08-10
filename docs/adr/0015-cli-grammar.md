# 0015 — The CLI grammar: bare is ACP, everything else is run

> Status: Accepted · 2026-08-11
> Extends [`0002-acp-is-the-protocol.md`](0002-acp-is-the-protocol.md) and
> [`0007-acp-sessions-and-the-dispatch-loop.md`](0007-acp-sessions-and-the-dispatch-loop.md).

## Context

With `watch` retired the binary has two real modes — the ACP server and the
one-shot run — plus utilities. The remaining friction is human: the common
human invocation (`lan run "<prompt>"`) carries a subcommand the human never
needed, while the common *machine* invocation (an editor spawning plain `lan`)
must stay exactly as it is, because zero-config spawning is what ADR-0002
bought.

One trap needs deciding rather than discovering: bare `lan` speaks JSON-RPC on
stdio, and an editor connecting looks exactly like a shell pipe — non-TTY
stdin, no args. So `cat prompt.txt | lan` cannot be auto-detected as "prompt
on stdin" without breaking the thing editors rely on. TTY-sniffing cannot
distinguish the two; nothing can, from the process's seat.

## Decision

```
lan                      # ACP server on stdio — what an editor spawns
lan "<prompt>" [flags]   # shorthand: identical to lan run
lan run "<prompt>"       # one-shot; `-` as prompt reads stdin
lan bridge               # websocket relay for browser ACP clients
lan fingerprint          # workspace hash, for skip-if-unchanged scripts
```

- **A positional argument that is not a known subcommand is a prompt**, and
  the invocation is `lan run` with that prompt. Flags pass through
  (`lan --json "hi"` ≡ `lan run --json "hi"`); `--` escapes a prompt that
  collides with a subcommand name.
- **Bare `lan` remains the ACP server.** Prompt-from-stdin is explicit:
  `lan run -`. The server converts the trap into a signpost — if its first
  input line is not JSON-RPC, it exits with an error naming the fix
  (`expected an ACP client on stdio; did you mean 'lan run -'?`) instead of
  waiting silently on prose.
- **Exit codes are contract:** `0` success, distinct nonzero codes for run
  failure and for a tripped bound, so shell callers can branch without
  parsing. `--json` remains the structured detail.

## Consequences

- The human path is the shortest path (`lan "fix the failing test"`), the
  editor path is byte-identical to before, and the script path gets codes and
  `--json`. No mode detection, no cleverness — every ambiguity is resolved by
  explicit syntax.
- A prompt equal to a subcommand name (`lan run` the word) requires `--`;
  accepted cost, held to five reserved words.
- The grammar is the whole CLI. Any future subcommand proposal starts from
  "which of the two audiences needs this, and why is it not the host's code?"
