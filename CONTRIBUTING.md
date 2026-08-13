# Contributing to lan

Start with the repository-level `AGENTS.md` and the relevant ADR. lan keeps
generic runtime capabilities in Mentra, transport/protocol code in adapters or
the binary, and `lan-core` free of TTY and transport dependencies.

Before opening a change, run the same gates as CI:

```sh
cargo fmt --all --check
cargo check --workspace --all-targets --all-features --locked
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-features --locked
```

Use a task-owned `CARGO_TARGET_DIR` when working concurrently. Keep commits
small and use Conventional Commit subjects (`fix(scope): ...`,
`feat(scope): ...`). Do not include credentials, local absolute paths, or
generated build artifacts in a change.
