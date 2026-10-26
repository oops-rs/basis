# CLI release profile packaging

Source request: make `cargo install` use the existing optimized release profile,
add CI coverage for the packaging regression, and assess the safety of the change.

## Facts and constraints

- Cargo workspace builds read the root profile. The published CLI is a standalone
  package, and its current normalized manifest omits that profile.
- The existing contract is `strip = true`, `opt-level = "s"`, `lto = true`, and
  `codegen-units = 1`. Panic unwinding, features, and runtime behavior stay intact.
- An isolated packaging probe confirmed that a profile in the member manifest is
  retained in the archive, despite being ignored during workspace builds.
- Verification must inspect the archive Cargo actually produces, rather than only
  comparing the two source manifests. Unpublished sibling versions must not make
  an archive-manifest check depend on registry availability.

## Implementation plan

1. **Preserve the profile** — Rust developer; `Cargo.toml` and
   `basis-cli/Cargo.toml`. Repeat the release profile for standalone packaging,
   explain why both copies exist, and remove stale size/throughput claims.
   Acceptance: the generated CLI archive contains the four intended settings.
   Commit before starting the next slice.
2. **Prevent regression** — Rust test writer; a focused script under `scripts/`
   and `.github/workflows/ci.yml`. Run the archive-manifest check in CI and reject
   missing or drifting settings. Acceptance: the real package passes; isolated
   removal/drift ablations fail with actionable diagnostics. Commit this slice.
3. **Validate and review** — primary agent and Rust reviewer. Build/install an
   extracted CLI archive into a temporary install root without profile overrides;
   smoke-test it and record its size. Review the diff and CI compatibility.

## Ordering and verification

One demand, executed serially: slice 2 depends on slice 1's manifest contract;
slice 3 consumes both. No parallel edits or new worktree are needed. The existing
checkout is `/Users/wendell/Developer/oops-rs/basis`. No Rust source/API changes or
new dependencies are intended. A full Rust source sweep is unnecessary for this
manifest/CI-only change; run the packaging checks and archive installation directly.

The profile's tradeoffs remain longer release builds, stripped symbol information,
and potentially different throughput. This change makes install behavior agree
with the workspace; it does not establish a performance benchmark or adopt
`panic = "abort"`.
