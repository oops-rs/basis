# 0048 — A hosted task knows its id, what its caller was vouched for, and its own files

> Status: Accepted · 2026-11-06
> Builds on [`0033`](0033-a-host-may-run-the-tasks-it-serves.md) and
> [`0036`](0036-a-server-may-authenticate-its-own-way.md).

## Context

A service that runs its own tasks (Tiamat, over basis-swift) wanted to serve
them through `basis-a2a` instead of a second A2A server of its own. Three
things it already did could not cross the `TaskHost` seam:

1. **The id callers see.** The server minted the A2A id after `start`
   returned, so the host could not tell its task — or the programs that task
   runs — the id its caller will quote back.
2. **What the authenticator verified.** A gateway signs a call chain per
   request. An `Authenticator` saw the headers but could only return a name,
   and the only per-message data reaching the host was the message's
   `metadata`, which the caller writes. Folding the chain into the name would
   split one caller's tasks across names.
3. **Files a task produced.** A completion's parts are the one `result`
   artifact. A task that writes a report and a data file, or fails after
   producing evidence, had no way to serve them as named artifacts.

## Decision

1. **`HostedStart::task_id`.** The server mints the A2A id before calling
   `start` and hands it over. The host's own id still never reaches a caller.
2. **`Principal::with_attribute`.** An authenticator may attach key/value
   attributes to the principal it returns. They reach a hosted task as
   `HostedStart::attributes`, for that message only. Ownership stays by name:
   attributes never decide whose task is whose, and nothing else in the crate
   reads them. The server's own tokens attach none.
3. **`TaskHost::artifacts`**, defaulting to none. A host's artifacts are served
   after the `result` artifact wherever a task's artifacts are — `GetTask`,
   `SendMessage`, `ListTasks` with `includeArtifacts`, `CancelTask`, a
   stream's snapshot and its end — in any state. The id `result` is reserved
   and every artifact needs a part; a host that breaks either has failed, and
   its caller is told only that.

All three are additive: `HostedStart` is non-exhaustive, `artifacts` has a
default, and `Principal::new` is unchanged.

## Consequences

- A host can key its task by the caller-visible id. If it crashes between
  `start` and the index write, its task exists and the caller was never
  answered; the host must not replay it, as for any unacknowledged start.
- A per-request attribute is not stored by the server. A host that needs it
  later persists it with its task.
- A stream sends a host's artifacts once, when the task settles; a task paused
  on its caller shows them in `GetTask`, not in the stream that closed.

## Ablation

Each mechanism was removed in turn; the named test failed and the restored
suite passed: minting after `start` (`the_host_is_handed_the_id_its_callers_see`),
dropping attributes (`…vouched_for_and_not_what_the_caller_wrote`), not
projecting host artifacts (`…served_in_any_state_after_the_result`), skipping
the contract check (`…breaks_the_contract_is_the_hosts_failure`), and a stream
that drops them (`a_stream_ends_with_the_result_then_the_hosts_artifacts…`).
