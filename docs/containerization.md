# basis — Containerization

> 2026-08-11 · Replaces the Dockerfile withdrawn by
> [ADR-0013](adr/0013-the-host-owns-the-boundary.md).
> [ADR-0004](adr/0004-kernel-enforced-confinement.md)'s claim is unchanged —
> the boundary is the kernel's, in-process checks are hygiene — but basis no
> longer ships an instance of it. This document is what it shipped instead.

## 1. Posture

**basis ships no container and claims no sandbox.** A bare-host run carries the
full authority of the user account that starts it: every file that account can
read, every file it can write, every command it can execute. Shell is part of
that authority and is on by default, because a harness that cannot run the
test suite does very little real work.

Nothing inside the process narrows this. The runtime policy's path roots and
the `.git/hooks` write-deny are hygiene — they shut the route a model reaches
for first, the file tools, and a shell redirect walks straight past them. They
are not a boundary and are never described as one.

The repository is inside that authority, not outside it. `AGENTS.md`,
`.basis/config.json`, `.basis/hooks.json`, `.basis/tools.json`, `.mcp.json` and
the skills roots are configuration, and configuration here names programs:
opening a workspace connects the MCP servers `.mcp.json` declares before a model
has said anything, and a `.basis/hooks.json` entry that lists no `tools` is asked
on every tool call, reads included. Both spawn with the authority above. What
each is *handed* is narrower: a hook and a declared tool run under mentra's
`BoundedCommand`, which clears the environment, so a hook sees only basis's
baseline (`PATH`, `HOME`, temp and locale) and a declared tool that plus the
variables its manifest names — the provider key this process read, and
anything the host exported, reach neither. A stdio `.mcp.json` server now uses
the same host-owned process discipline: Mentra clears the ambient environment,
restores the documented runnable baseline (`PATH`, `HOME`, `TMPDIR`, `TMP`,
`TEMP`, `LANG`, and `LC_ALL` on Unix; `PATH`, `PATHEXT`, `SystemRoot`,
`COMSPEC`, `TEMP`, and `TMP` on Windows), then layers the variables its config
explicitly names. A `.mcp.json` author must name every variable outside that
baseline, including provider credentials and proxy settings. The process is
grouped and its descendants are terminated together on disconnect or drop;
protocol frames and retained stderr stay bounded, and stderr is continuously
drained. That is hygiene and not a boundary: the server still has the host
account's filesystem, network, and account authority; what stops is the ambient
credential nobody decided to pass. Cloning a repository and opening it is
therefore the same act as running what it ships
([ADR-0013](adr/0013-the-host-owns-the-boundary.md)).

One refusal is deliberate and narrow: `base_url` in a *workspace*
`.basis/config.json` fails the open by name, because a redirected endpoint
carries the credential basis read out of the environment to a host the file
chose, and a leaked secret is bounded by nothing while a spawned program is
bounded by whatever confines the process. It buys no general immunity —
`.mcp.json`'s `${VAR}` expansion hands the variables it names to a program the
repository declared, which is the point of that key.

For a repository you have not read, the two honest moves are to open the
workspace with discovery off — `WorkspaceBuilder::without_discovery()`, which
probes none of those files; it is an embedding host's knob and the CLI carries
no flag for it — or to put the process inside one of the patterns below.

Isolation, where you want it, comes from the OS. The rest of this document is
the pattern basis used to ship as an image, written down so you can build it
yourself — and so that if you don't, you know exactly what you are running
instead.

Liveness is the OS's too
([ADR-0019](adr/0019-the-filesystem-is-the-coordination-surface.md)): no basis
process survives a completed invocation, and an asynchronous agent advances
only while a process is attached to it. Inside a container that means the
container has to outlive the `basis wait` doing the work — there is no daemon to
carry it, and backgrounding is `&`, `nohup`, tmux, `systemd-run`, or CI, on
whichever side of the container boundary you want it. Task state lives under
one global data directory: `BASIS_DATA_DIR` if set, else `XDG_DATA_HOME/basis`,
else the platform data home. The image below sets `XDG_DATA_HOME=/state`, so
that root resolves to `/state/basis` and rides the same volume as the store —
which is what makes a task spawned in one `docker run` resumable by the next.

## 2. An image to build from

Nothing below is basis-specific ceremony; it is the smallest image that gives
the next section something to mount. Two stages, so the Rust toolchain does
not ride along into the runtime.

```dockerfile
FROM rust:1.88-slim-bookworm AS build

WORKDIR /src

# pkg-config and libssl-dev are for the provider's TLS.
RUN apt-get update \
    && apt-get install --no-install-recommends --yes \
        pkg-config libssl-dev \
    && rm -rf /var/lib/apt/lists/*

COPY . .
RUN cargo build --release --locked --bin basis

FROM debian:bookworm-slim

# The agent is a coding agent: git and a shell are the point of the image.
# ca-certificates is required to reach any provider over TLS.
RUN apt-get update \
    && apt-get install --no-install-recommends --yes \
        ca-certificates git \
    && rm -rf /var/lib/apt/lists/*

COPY --from=build /src/target/release/basis /usr/local/bin/basis

# An unprivileged user, so a run that forgets --read-only still cannot
# rewrite the image's own binaries.
RUN useradd --create-home --uid 10001 basis \
    && mkdir -p /workspace /state \
    && chown basis:basis /workspace /state
USER basis

ENV HOME=/home/basis

# Both durable stores default under $HOME — mentra's sessions to the platform
# data-local dir, basis's task state to the platform data home — and $HOME is
# unwritable once the root filesystem is read-only. Point XDG_DATA_HOME at the
# state mount so both land somewhere that survives --read-only and --rm.
ENV XDG_DATA_HOME=/state

WORKDIR /workspace
ENTRYPOINT ["basis"]
CMD ["--help"]
```

Two lines there are load-bearing rather than decorative. `chown` on `/state`
is what a fresh named volume inherits its ownership from, so without it uid
10001 gets a root-owned mount and the store fails to open. `XDG_DATA_HOME` is
the difference between session and task state landing on the volume and
landing on a read-only path.

A `.dockerignore` holding `/target` and `/.git` is worth adding beside it —
build artifacts are rebuilt inside the image anyway, and history is not needed
to compile.

```sh
docker build -t basis:local .
```

The image resolves Mentra 0.18 from crates.io, so `COPY . .` is a complete
build context. No sibling checkout is required.

## 3. The read-only-root pattern

```sh
docker run --rm \
  --read-only --tmpfs /tmp \
  --security-opt no-new-privileges \
  -v "$PWD":/workspace:rw \
  -v basis-state:/state \
  -e ANTHROPIC_API_KEY \
  basis:local run "run the tests and tell me what broke"
```

Flag by flag:

- `--read-only` makes the container's root filesystem immutable. This is the
  whole mechanism; everything else supports it.
- `--tmpfs /tmp` gives back the one writable scratch path that compilers,
  `git`, and most tooling assume exists.
- `--security-opt no-new-privileges` stops a setuid binary inside the image
  from raising privileges beyond the unprivileged user it started as.
- `-v "$PWD":/workspace:rw` is the sole writable mount, and the reason the
  pattern is worth running at all.
- `-v basis-state:/state` is a named volume for the session store and, under
  `/state/basis`, for durable task state. Without it, `--rm` and `--read-only`
  together leave the agent nowhere to write, and it fails at startup rather
  than degrading quietly — and a task handle printed by one container would
  name an agent directory the next container has never seen.
- `-e ANTHROPIC_API_KEY` passes the provider key from your environment. Pass
  it at run time, never `ENV` it into the Dockerfile — image layers are
  readable by anyone who can pull the image.

Inside, a command that reaches past the workspace is refused by the kernel
rather than by basis:

```
/bin/sh: 1: cannot create /etc/breach.txt: Read-only file system
```

## 4. What this protects, and what it does not

**It protects the host filesystem outside the workspace mount.** The refusal
above comes from the kernel, and it applies to a shell redirect exactly as it
applies to a file tool, which is precisely what an in-process check cannot
claim.

**It does not protect the workspace.** Everything under `/workspace` is fully
writable by design — that is the job you asked for. Uncommitted work is the
thing genuinely at risk in any run, containerized or not, and version control
is the undo, not the container.

**It does not close network egress.** The container can reach whatever the
host can, because the provider APIs are on the far side of that connection.
An allowlist proxy is the contained later addition if you want egress
narrowed; nothing in this pattern narrows it today.

**And it is only as good as the mount set you pass.** `docker run -v /:/host`
is a container with no boundary at all. The flags above are a claim about a
specific invocation, not about containers in general — which is also why basis
does not sniff its environment and infer safety from it. Being inside a
container proves nothing about how that container was run.

## 5. A native layer, later

[`proposals/0002-native-sandbox.md`](proposals/0002-native-sandbox.md)
describes a per-command OS wrapper — Seatbelt on macOS, bubblewrap plus
seccomp on Linux — that would offer some of this without a daemon, an image,
or mount ceremony. It remains a possible future and an **optional** layer if
it lands: a knob for operators who want it, not a return to denying commands
by default.
