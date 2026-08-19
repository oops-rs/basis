# 0003 — Packages: shareable bundles of skills, templates, and hooks

> Status: Deferred — until the primitives (skills, templates, hooks, `.mcp.json`
> wiring) exist and are stable.
> Created: 2026-08-08
> Related: pi's packages (`packages.md`) as prior art.

## Summary

A directory convention + manifest for distributing a set of skills, prompt
templates, subprocess hooks, and MCP server configs as one installable unit, so a
team can share "how our agents work here" across repos.

## Motivation

pi packages bundle extensions/skills/prompts/themes and proved the demand. For basis
the equivalent bundle is pure data (Bet 4), which makes packages nearly free once
the primitives stabilize — but defining the manifest before the primitives settle
would lock their shapes prematurely.

## Properties any implementation must preserve

- A package is data only; anything executable arrives as an MCP server or
  subprocess hook, subject to the same confinement as everything else.
- Workspace-local config always overrides package content.
- No registry requirement: a package is a directory (git repo, tarball, path).
