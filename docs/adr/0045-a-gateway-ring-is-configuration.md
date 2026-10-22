# 0045 — A gateway ring is configuration

> Status: Accepted · 2026-10-22
> Builds on [`0027-gateway-failover-lives-below-basis.md`](0027-gateway-failover-lives-below-basis.md);
> keeps [`0018-the-runtime-owns-the-process.md`](0018-the-runtime-owns-the-process.md)'s
> lifecycle.

## Context

ADR-0027 made a gateway ring a builder answer: `with_gateway_ring` and a
list of `GatewayMember`s. That is the whole mechanism, and it is right, but
it can only be reached from code. Five hosts on one build machine reach
their model through the same pair of gateways, and what each had to do to
get a ring measured the gap:

- Two hosts wrote their own config block, their own member parsing, and
  their own observer `match` over `GatewayRingEvent`, line for line.
- Two read `MODEL_BASE_URL` themselves and called `with_base_url`; a ring
  would have meant writing the first host's code a third and fourth time.
- One is `dial serve`, configured entirely by its environment through
  basis's own `BASIS_BASE_URL` lookup. It had no way to state a ring at all.

Two further facts from the same deployment bound the design:

1. **Members name the same model differently.** The unified gateway serves
   `openai/gpt-5.6-sol`; a Codex relay serves the same model as
   `gpt-5.6-sol` and answers the namespaced id with a `400`. One host wrote
   a provider wrapper to rewrite the id per member. Without it a ring of
   those two always fails on one of them.
2. **Every member takes its own key, and keys must not live in files.**
   ADR-0027 already made a member a URL and a key; basis's config file has
   no `api_key` key at all, and the hosts' own rule is that a credential is
   a path, never a value.

## Decision

1. **The global `config.json` gains `gateways`, and `BASIS_GATEWAYS`
   carries the same JSON.** A list of members, or `{"members": [...]}` with
   `failure_threshold`, `cooldown_secs` or `sticky` beside them. One parser
   serves both spellings so they cannot drift. Global file only and never
   beside `base_url`, for `base_url`'s reason: it redirects the traffic the
   credential rides on.

2. **A member names where its key lives.** `api_key_env` and
   `api_key_file` in data; `GatewayMember::with_api_key_env` and
   `with_api_key_file` in code. The key is read at `build`, and a source
   that is unset, empty or unreadable is a build error naming the member —
   not a keyless request whose `401` the ring would rotate past, hiding the
   mistake behind a working fallback.

3. **A member may strip a model-id prefix.** `strip_model_prefix` /
   `GatewayMember::with_model_prefix_stripped`, built on mentra's new
   `StripModelPrefix` provider (mentra owns it per ADR-0005: any harness
   with a mixed ring needs it). An id without the prefix is sent unchanged
   and the member answers for it. The ring's precondition is unchanged —
   the members serve the same model; only the spelling differs.

4. **Precedence follows the existing ladder.** A host instance, then a ring
   in code, then a base URL in code, then the file's `gateways`, then
   `BASIS_GATEWAYS`, then the rest of the environment. The file's ring is
   held apart from the code's until `build`, so `with_config` and
   `with_base_url` give the same answer in either order. A policy or
   observer the code stated joins a ring from data; a policy in the data
   fills only an unsaid one. `BASIS_GATEWAYS` beside a base-URL variable,
   or a `with_api_key` beside a ring from data, is refused by name rather
   than ranked.

5. **Logging stays the host's, and becomes one line.** basis still has no
   logger. mentra's `GatewayRingEvent` and `GatewayMemberRef` implement
   `Display`, so an observer is `|event| warn!("{event}")`.

## Consequences

- A host configured only by its environment — `dial serve` — gets a ring
  with no code change. Its rotations stay invisible until it attaches an
  observer, which is one line now.
- `Config` gains `gateways`, `ConfigError` gains `WorkspaceGateways`, and
  `ProviderError` gains `GatewayCredentialMissing`,
  `GatewayCredentialUnreadable`, `InvalidGateways` and
  `GatewaysBesideBaseUrl`; exhaustive matches need arms.
- `GatewayMember` is `PartialEq` and its `Debug` prints where a key lives —
  a variable name or a path — and still never a value.
- basis depends on the mentra release carrying `StripModelPrefix` and the
  `Display` impls; until it is published the workspace carries the
  development path patch.
- Not decided here: a ring per model. A runtime has one provider, so every
  model it asks for goes to the same ring; a host whose catalog mixes
  vendors behind one gateway still keeps that member out of the ring for
  models it cannot serve, in code.
