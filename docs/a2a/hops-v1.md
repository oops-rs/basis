# A2A extension: hops, v1

> URI: `https://github.com/oops-rs/basis/blob/main/docs/a2a/hops-v1.md`
> Status: Stable · decided by [ADR-0030](../adr/0030-a2a-is-spoken-in-full.md) §9

Agents that hand work to each other can loop: A calls B, B calls A. This
extension carries, with each message, how far the message is from the caller
that started the chain and when that chain stops waiting, so a loop ends at a
hop cap or the first deadline instead of costing a full deadline at every
hop. The key words MUST, MUST NOT, SHOULD and MAY are as in RFC 2119.

## Declaration

An agent that reads this extension lists it in its card's
`capabilities.extensions`, with `required: false`.

## The metadata

A message carries, in its `metadata`, under the extension's URI:

```json
{
  "metadata": {
    "https://github.com/oops-rs/basis/blob/main/docs/a2a/hops-v1.md": {
      "hop": 2,
      "deadline": "2026-10-10T12:00:00Z"
    }
  }
}
```

- `hop` (required in the object): a non-negative integral number — `2`, or
  `2.0` as a protobuf `Struct` writes every number — the number of
  agent-to-agent hands the message is from a top-level caller.
- `deadline` (optional): an RFC 3339 timestamp after which the sender no
  longer waits for the answer.

A message without the object is hop 0 with no deadline. This is what makes
the extension safe to ignore: an agent that does not speak it sends nothing,
and is treated as a top-level caller.

## What a client sends

A client that sends a message on behalf of work that itself arrived over A2A
at hop `h`, with effective deadline `d`:

- MUST send `hop` = `h + 1`;
- MUST send `deadline` = the earlier of its own deadline and `d`, when either
  exists. A deadline only ever shrinks across a hop.

A top-level caller — one whose work did not arrive over A2A — sends `hop` 1
and its own deadline, if it has one.

## What a server does

- It MUST refuse a message whose `hop` exceeds its cap, with `InvalidParams`
  (JSON-RPC `-32602`, HTTP 400) and a message that names the cap. basis's
  default cap is 8.
- It MUST refuse a message whose `deadline` has already passed.
- It MUST NOT give the task it starts longer than the caller's `deadline`: the
  task's own deadline is clamped to the earlier of the two.
- It SHOULD record the hop and the effective deadline with the task, and MAY
  report them in the task's `metadata` under the extension's URI, as basis
  does.
- The task's own delegations are the next hop: they send `hop` + 1 and the
  task's effective deadline, as a client above.

A malformed object — a `hop` that is not a non-negative integral number, a `deadline`
that is not RFC 3339 — is refused with `InvalidParams` rather than read as
absent.
