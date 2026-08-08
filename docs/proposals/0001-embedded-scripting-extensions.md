# 0001 — Embedded scripting layer for extensions

> Status: Deferred — written down per Bet 7 (earn every part).
> Created: 2026-08-08
> Related: [`../ARCHITECTURE.md`](../ARCHITECTURE.md) §3,
> [`../adr/0001-mentra-is-the-runtime.md`](../adr/0001-mentra-is-the-runtime.md)

## Summary

If MCP servers + subprocess hooks prove too coarse for extension authors, add an
in-process scripting layer (wasm component model or rhai) with access to the
lifecycle events and tool registration that pi gives its TypeScript extensions.

## Motivation

pi's in-process TS extensions can do things process-isolated mechanisms can't do
cheaply: intercept and *modify* tool calls with shared state, render custom UI,
maintain in-memory state across events without serialization. lan's v1 answer is
MCP (custom tools) + subprocess hooks (interception via JSON in/out). The bet is
that this covers most real extensions; this proposal exists so the escalation path
is designed, not improvised.

## Trigger

Adopt only on demonstrated friction: an extension that a real user attempted and
could not express (or could only express with painful subprocess round-trips)
through MCP + hooks. Collect these cases here as they occur.

## Properties any implementation must preserve

- Extensions cannot widen the confinement boundary (ADR-0004): scripting runs
  inside the same policy scope as builtin tools.
- The crate API remains the first-class surface; scripting is sugar over it, not a
  second API.
- Startup cost stays negligible when no extensions are configured.
