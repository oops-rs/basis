# basis — Command targets

> 2026-08-21 · The pattern document for
> [ADR-0021](adr/0021-a-command-names-where-it-runs.md), in the same shape as
> [containerization.md](containerization.md) and for the same reason
> ([ADR-0013](adr/0013-the-host-owns-the-boundary.md)): the honest thing here is
> a pattern rather than an implementation, so basis documents one and ships
> none.

## 1. What a target is

A command a model runs normally lands wherever basis is running. A **target**
is a second place it can land, named by the host:

```
!cargo test -q            # here
!@mac xcodebuild -list    # on whatever the host registered as `mac`
```

`spawn` is still the model's one door. *Where* is a dimension of the call, not a
second tool — a second tool would be a second name at the approval gate and a
second namespace of remembered rules for one question (ADR-0016).

The case this exists for: basis running inside a Linux container on a macOS
build machine. The repository is mounted, so `cargo test`, `rg` and `git`
belong in the container; `xcodebuild`, `simctl` and the signing keychain are
not in the container at all.

**basis ships no executors.** It routes by name to code the host wrote. What a
target can reach is whatever that code can reach, and §5 is the paragraph you
should read before you decide that sentence is a formality.

## 2. Registering one

```rust
use basis::Runtime;

let runtime = Runtime::builder()
    .with_command_target("mac", MacExecutor::new(/* … */))
    .build()?;
```

Names are `[A-Za-z0-9_-]+` and may not be `local`, which is what the wire
contract calls a command that named no target. A bad name is a
`RunError::CommandTarget` from `build()` — not a panic — so a host reading its
targets out of its own configuration can report one the way it reports every
other bad setting. Registering the same name twice keeps the last executor, the
same rule `with_command_environment` follows.

Targets are runtime-scoped (ADR-0018): every workspace on that runtime reaches
the same set. A host that wants two workspaces to differ gives each its own
runtime through `WorkspaceBuilder::with_runtime_builder`.

Registering at least one target changes two model-visible things, and nothing
else. The `spawn` description gains a paragraph naming the prefix and the
registered names, and the serialized call an approver sees gains a `target`
key. A runtime with no targets is told nothing about the prefix at all — a
model must not be taught a door that is not there.

## 3. What the executor receives

```rust
#[async_trait::async_trait]
impl mentra::runtime::RuntimeExecutor for MacExecutor {
    async fn run(
        &self,
        request: mentra::runtime::CommandRequest,
    ) -> Result<mentra::runtime::CommandOutput, String> {
        // request.spec    — CommandSpec::Shell { command }
        // request.cwd     — advisory; see below
        // request.timeout — already clamped by the runtime's policy
        // request.env     — the runtime's fixed command environment, merged
        // request.target  — Some("mac"): the name it was routed under
        // request.max_output_bytes_per_stream
    }
}
```

Four of those are worth saying out loud.

**The timeout is already clamped.** basis does not ask the executor to enforce
a policy; mentra resolved the request's timeout against the runtime's default
and ceiling before anything routed. Honor it — a target that runs past it is
the one participant in the chain with no bound on it.

**The environment is already merged.** Whatever `with_command_environment` set
is on the request before routing, so a target and the local executor see the
same pairs. This is deliberate: two environments that had to be kept in step
would drift, and nobody would notice the day they did.

**`cwd` is advisory.** It is a path in *basis's* filesystem — inside the
container, in the worked case. basis sends it because an approver cannot judge
a command without knowing where it was meant to run, and it translates nothing.
Deciding what that path means on your machine — map it, ignore it, refuse a
command whose cwd you cannot map — is the executor's job.

**The target name stays on the request.** One executor registered under two
names can tell which it was called as. If it serves only some of the names it
receives, it must **refuse** the rest rather than run them: mentra's own
`LocalRuntimeExecutor` does exactly this, because a command a host addressed to
a build machine silently executing somewhere else is the failure a target
exists to prevent.

## 4. A worked example: SSH to a forced command

Nothing below is basis-specific. It is the smallest thing that gets a command
from inside a container onto the Mac hosting it, with the Mac deciding what may
run rather than trusting whatever arrives.

**On the Mac**, in `~/.ssh/authorized_keys` — one line, one key:

```
command="/usr/local/libexec/basis-target",restrict,from="192.168.65.0/24" ssh-ed25519 AAAA… basis-container
```

- `command="…"` is the forced command: whatever the client asks for is ignored
  as a program, and this wrapper runs instead. The client's requested command
  arrives to it in `SSH_ORIGINAL_COMMAND`.
- `restrict` turns off port forwarding, agent forwarding, X11 and PTY
  allocation — everything the connection does not need. It is the modern
  spelling of the four `no-*-forwarding` options.
- `from="…"` limits the key to the address range the container appears from.

Use a key that exists for this and nothing else, and give the wrapper its own
account rather than one that can `sudo`.

**The wrapper re-validates argv.** This is the part that carries the security
of the arrangement, and the reason the forced command exists at all:

```sh
#!/bin/sh
# /usr/local/libexec/basis-target — the Mac decides what may run.
set -eu

set -- $SSH_ORIGINAL_COMMAND
case "${1:-}" in
  xcodebuild|xcrun|simctl|notarytool) ;;
  *) echo "refused: $1 is not a permitted command" >&2; exit 126 ;;
esac

cd /Users/build/checkout
exec "$@"
```

Treat that as a sketch of the *shape*, not a drop-in: a real wrapper decides
deliberately about word splitting, about which flags of a permitted program are
themselves an escape hatch (`xcodebuild -runDestination` and friends can run
arbitrary scripts), and about where it puts the checkout.

**The executor, inside the container**, is then a thin thing:

```rust
use async_trait::async_trait;
use mentra::runtime::{CommandOutput, CommandRequest, CommandSpec, RuntimeExecutor};

struct SshTarget {
    user_at_host: String,
    key: std::path::PathBuf,
}

#[async_trait]
impl RuntimeExecutor for SshTarget {
    async fn run(&self, request: CommandRequest) -> Result<CommandOutput, String> {
        if request.target.as_deref() != Some("mac") {
            return Err("this executor serves only the `mac` target".to_string());
        }
        let CommandSpec::Shell { command } = &request.spec;

        // BatchMode: never prompt — there is no human on this end.
        // StrictHostKeyChecking=yes: a changed host key is a failure, not a
        // question. Both are the difference between an error and a hang.
        let output = tokio::process::Command::new("ssh")
            .args(["-o", "BatchMode=yes", "-o", "StrictHostKeyChecking=yes"])
            .arg("-i")
            .arg(&self.key)
            .arg(&self.user_at_host)
            .arg("--")
            .arg(command)
            .output()
            .await
            .map_err(|error| format!("could not reach the mac target: {error}"))?;

        Ok(CommandOutput {
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            success: output.status.success(),
            status_code: output.status.code(),
            timed_out: false,
            stdout_truncated: false,
            stderr_truncated: false,
        })
    }
}
```

Two things it does not do, which a production one must: enforce
`request.timeout` (wrap the `output()` in `tokio::time::timeout` and report
`timed_out`), and cap output at `request.max_output_bytes_per_stream`. The
local executor does both, and a target that skips them is the one place a run's
bounds stop applying.

## 5. What this is and is not

**A target is exactly as trusted as the executor the host wrote.** basis
performs no confinement, verifies nothing about the far side, and does not know
what a name resolves to. Approval is policy, not confinement (ADR-0013), and
routing a command elsewhere adds no boundary — it moves the command.

**basis never calls a target "the host".** It has no way to know whether it is
one. On Docker Desktop, `docker exec` and `nsenter` targets reach the **Linux
VM the daemon runs in** — not macOS. A target built on either of those, named
`mac` or `host`, will run Linux commands on a Linux machine and report success,
which is worse than failing: the name says one thing and the destination is
another. If you want macOS, the connection has to leave the VM, which is what
§4 spends an SSH hop on.

**Nothing here is described as a sandbox.** A target does not narrow authority;
it relocates it. The command runs with whatever authority the executor's own
process, connection, or remote account holds — and on the SSH pattern above,
what narrows it is the forced command and the wrapper's allowlist on the *Mac's*
side, enforced by the Mac. That is a boundary because the far end owns it, not
because basis routed to it.

**Every guard that applies to a command still applies.** A targeted command is
`Mode::Command`: it reaches the approver, matches remembered rules, runs the
workspace's hooks, and is refused outright by `--no-shell`. Naming a target is
not a way past any of them.

**A remembered rule can pin the destination.** The routing decision rides in
the same serialized object every other key does, so an operator who wants the
line drawn per machine can draw it — and, as with every other pattern rule, it
answers ahead of the approver, so an allowlisted target costs no model round
trip:

```
**"target":"mac"**      # this machine, any command
**"body":"xcodebuild *","target":"mac"**
```

`**` and `*` mean the same thing here: mentra 0.18.2 matches these patterns as
data, so a wildcard runs over `/` like any other character and JSON's
punctuation is literal. basis writes `**` out of habit and for continuity with
rules stored under the older spelling. On mentra 0.18.1 and earlier this did
not work at all — patterns were matched with a path globber, so with an
absolute `cwd` no pattern on `target` or `mode` could ever match, silently.
That is why basis requires 0.18.2.

**And a bare rule now covers every target at once.** An `AllowForSession`
answer is stored with no pattern, so it covers delegations, local commands and
every destination together. Telling them apart means writing a pattern, which
is the trade ADR-0016 named, with one more dimension on it.

## 6. What is not supported yet

- **Background tasks are local-only.** `spawn` starts none, and mentra's
  background path takes no target.
- **No fan-out.** One command names one place.
- **No per-workspace targets.** One runtime, one set.
- **No discovery beyond the names.** The model is told which targets exist and
  nothing about what is on them; anything it should know about a target belongs
  in the workspace's context documents.
