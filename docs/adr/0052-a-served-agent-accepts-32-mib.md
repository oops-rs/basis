# 0052 — A served agent accepts 32 MiB

> Status: Accepted · 2026-11-19. Revises the request bound of
> [ADR-0041](0041-a-delegation-carries-files.md) §6.

## Context

ADR-0041 raised the A2A server's request bound to 16 MiB. The dial gateway
in front of served agents now accepts requests up to 32 MiB, so a caller
sending files through it can pass the gateway and still be refused by a basis
agent.

## Decision

The A2A server accepts request bodies up to 32 MiB, both through its
declared length check and while reading the body. The 30 second body
deadline stays.

## Consequences

A served agent holds up to 32 MiB of one request in memory while reading it.
A caller on a slow link must deliver the body within the deadline, about
1.1 MB/s at the full bound. What a basis caller sends with its own
delegations is still bounded at 6 MiB of files (ADR-0041).
