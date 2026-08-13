# lan — Containerization

> 2026-08-11 · Replaces the Dockerfile withdrawn by
> [ADR-0013](adr/0013-the-host-owns-the-boundary.md).
> [ADR-0004](adr/0004-kernel-enforced-confinement.md)'s claim is unchanged —
> the boundary is the kernel's, in-process checks are hygiene — but lan no
> longer ships an instance of it. This document is what it shipped instead.

## 1. Posture

**lan ships no container and claims no sandbox.** A bare-host run carries the
full authority of the user account that starts it: every file that account can
read, every file it can write, every command it can execute. Shell is part of
that authority and is on by default, because a harness that cannot run the
test suite does very little real work.

Nothing inside the process narrows this. The runtime policy's path roots and
the `.git/hooks` write-deny are hygiene — they shut the route a model reaches
for first, the file tools, and a shell redirect walks straight past them. They
are not a boundary and are never described as one.

Isolation, where you want it, comes from the OS. The rest of this document is
the pattern lan used to ship as an image, written down so you can build it
yourself — and so that if you don't, you know exactly what you are running
instead.

## 2. An image to build from

Nothing below is lan-specific ceremony; it is the smallest image that gives
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
RUN cargo build --release --locked --bin lan

FROM debian:bookworm-slim

# The agent is a coding agent: git and a shell are the point of the image.
# ca-certificates is required to reach any provider over TLS.
RUN apt-get update \
    && apt-get install --no-install-recommends --yes \
        ca-certificates git \
    && rm -rf /var/lib/apt/lists/*

COPY --from=build /src/target/release/lan /usr/local/bin/lan

# An unprivileged user, so a run that forgets --read-only still cannot
# rewrite the image's own binaries.
RUN useradd --create-home --uid 10001 lan \
    && mkdir -p /workspace /state \
    && chown lan:lan /workspace /state
USER lan

ENV HOME=/home/lan

# mentra puts its session store under the platform data-local dir, which is
# under $HOME — unwritable once the root filesystem is read-only. Point
# XDG_DATA_HOME at the state mount so the store lands somewhere that survives
# both --read-only and --rm.
ENV XDG_DATA_HOME=/state

WORKDIR /workspace
ENTRYPOINT ["lan"]
CMD ["--help"]
```

Two lines there are load-bearing rather than decorative. `chown` on `/state`
is what a fresh named volume inherits its ownership from, so without it uid
10001 gets a root-owned mount and the store fails to open. `XDG_DATA_HOME` is
the difference between session state landing on the volume and landing on a
read-only path.

A `.dockerignore` holding `/target` and `/.git` is worth adding beside it —
build artifacts are rebuilt inside the image anyway, and history is not needed
to compile.

```sh
docker build -t lan:local .
```

The image resolves Mentra 0.18 from crates.io, so `COPY . .` is a complete
build context. No sibling checkout is required.

## 3. The read-only-root pattern

```sh
docker run --rm \
  --read-only --tmpfs /tmp \
  --security-opt no-new-privileges \
  -v "$PWD":/workspace:rw \
  -v lan-state:/state \
  -e ANTHROPIC_API_KEY \
  lan:local run "run the tests and tell me what broke"
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
- `-v lan-state:/state` is a named volume for the session store. Without it,
  `--rm` and `--read-only` together leave the agent nowhere to write, and it
  fails at startup rather than degrading quietly.
- `-e ANTHROPIC_API_KEY` passes the provider key from your environment. Pass
  it at run time, never `ENV` it into the Dockerfile — image layers are
  readable by anyone who can pull the image.

Inside, a command that reaches past the workspace is refused by the kernel
rather than by lan:

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
specific invocation, not about containers in general — which is also why lan
does not sniff its environment and infer safety from it. Being inside a
container proves nothing about how that container was run.

## 5. A native layer, later

[`proposals/0002-native-sandbox.md`](proposals/0002-native-sandbox.md)
describes a per-command OS wrapper — Seatbelt on macOS, bubblewrap plus
seccomp on Linux — that would offer some of this without a daemon, an image,
or mount ceremony. It remains a possible future and an **optional** layer if
it lands: a knob for operators who want it, not a return to denying commands
by default.
