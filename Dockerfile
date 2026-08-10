# lan — Lightweight Agent Nucleus
#
# The image exists to make ADR-0004's boundary real: the workspace is the only
# thing the agent can write, and the kernel enforces it rather than a path
# check inside the process. That is why LAN_ALLOW_SHELL is set here (ADR-0006)
# — the image author can vouch for an environment they built, which is the one
# case where granting commands without a flag is honest.
#
# Build:
#   docker build -t oops/lan:latest .
#
# Run (see docs/ARCHITECTURE.md §4 for why each flag is here):
#   docker run --rm -it \
#     --read-only --tmpfs /tmp \
#     --security-opt no-new-privileges \
#     -v "$PWD":/workspace:rw \
#     -v lan-state:/state \
#     -e ANTHROPIC_API_KEY \
#     oops/lan:latest run "explain the module layout"

FROM rust:1.88-slim-bookworm AS build

WORKDIR /src

# git is needed at runtime by the agent and at build time by cargo for any
# git dependency; pkg-config and libssl-dev are for the provider's TLS.
RUN apt-get update \
    && apt-get install --no-install-recommends --yes \
        pkg-config libssl-dev \
    && rm -rf /var/lib/apt/lists/*

COPY Cargo.toml Cargo.lock ./
COPY lan/Cargo.toml lan/Cargo.toml

# Build the dependency graph against a stub so the expensive layer is cached
# independently of lan's own sources.
RUN mkdir -p lan/src \
    && echo 'fn main() {}' > lan/src/main.rs \
    && echo '' > lan/src/lib.rs \
    && cargo build --release --locked --bin lan \
    && rm -rf lan/src

COPY lan lan
# cargo skips a rebuild when only mtimes moved, so make the real sources newer
# than the stub artifacts.
RUN touch lan/src/main.rs lan/src/lib.rs \
    && cargo build --release --locked --bin lan

FROM debian:bookworm-slim

# The agent is a coding agent: git and a shell are the point of the image.
# ca-certificates is required to reach any provider over TLS.
RUN apt-get update \
    && apt-get install --no-install-recommends --yes \
        ca-certificates git \
    && rm -rf /var/lib/apt/lists/*

COPY --from=build /src/target/release/lan /usr/local/bin/lan

# An unprivileged user, so a container run without --read-only still cannot
# rewrite the image's own binaries.
RUN useradd --create-home --uid 10001 lan \
    && mkdir -p /workspace /state \
    && chown lan:lan /workspace /state
USER lan

# Commands are granted here and only here: the workspace is the sole writable
# mount and the kernel is what enforces it (ADR-0006).
ENV LAN_ALLOW_SHELL=1

# mentra puts its SQLite store under the platform data-local dir, which is
# under $HOME by default — unwritable once the root filesystem is read-only.
# Point XDG_DATA_HOME at the state volume so the store lands somewhere that
# survives both --read-only and --rm.
ENV HOME=/home/lan
ENV XDG_DATA_HOME=/state

# Where lan looks for a global AGENTS.md and skills/ (see lan::context).
ENV LAN_CONFIG_DIR=/config

WORKDIR /workspace
ENTRYPOINT ["lan"]
CMD ["--help"]
