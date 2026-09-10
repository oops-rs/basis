# 0027 — Gateway failover lives below basis

> Status: Accepted · 2026-09-10
> Applies [`0005-mentra-coevolution-discipline.md`](0005-mentra-coevolution-discipline.md)
> and [`0018-the-runtime-owns-the-process.md`](0018-the-runtime-owns-the-process.md);
> consistent with [`0026-the-rebuild-half-of-reuse-is-deferred.md`](0026-the-rebuild-half-of-reuse-is-deferred.md).

## Context

A host that reaches its model through an AI gateway sometimes has more than
one. The case that raised this: two gateways in front of the same model, a
remote one that is live and a local one on the build machine, each with its
own key. The host wants to prefer one, wait out a blip on it, and rotate to
the next when the blip turns out not to be one — an ordered ring, one member
at a time, generalizing to N.

Nothing in the stack did this, and two things stood in the way of adding it
where a first reading would put it.

**Not a basis builder knob that mutates the provider.** ADR-0018 makes the
provider, its base URL, and its credential runtime-scoped and fixed at
`build`. ADR-0026 then *removed* ~900 lines of provider-rebuild machinery
because there was no honest way to mint a fresh provider from an existing one.
A `with_base_url_ring` that swapped the runtime's endpoint on failure is
runtime mutation of the provider, which is the position both ADRs just took
against.

**Not mentra's retry loop.** A provider definition's `base_url` is one
immutable field, 1:1 with a provider instance; every write site is a
constructor. The runtime's retry loop retries the same `provider.stream()`
call and has no hook for "same request, different endpoint". Teaching it one
would put an endpoint policy into a loop whose job is pacing.

Three facts measured while debugging the live deployment bound the design:

1. The two gateways take **different keys**; one gateway answers `401` to the
   other's. A member is therefore a whole provider with its own credential,
   not a URL.
2. A rotation **replays the transcript** to the new member. OpenAI-shaped
   reasoning items replayed to an xAI-backed gateway answer `422`. Members
   must front the same upstream serving the same model; nothing can validate
   that without a request, so it is a documented precondition.
3. **Nothing would have rotated.** A sick gateway answers `400`, which the
   runtime does not retry; an overloaded one answers `200` with the error
   inside the stream, which used to decode as a terminal `MalformedStream`.
   The mentra fix that maps a provider-side in-stream failure to `Retryable`
   is a prerequisite, and a `4xx` that is not a rate limit has to rotate at
   once rather than be waited out.

## Decision

**The ring is a mentra provider. basis states its members and registers it
through the seam it already has.**

1. **mentra-provider gains `gateway_ring::GatewayRing`**, a `Provider` whose
   members are providers. Every call goes to the current member; a streak of
   counted failures rotates to the next and *finishes the same call there*, so
   the runtime above never learns a rotation happened. Its policy — a failure
   threshold, and a cooldown after which the ring drifts back to the preferred
   member on probation, or none for a sticky ring — and its observer events are
   mentra's API. Per ADR-0005 this is mentra-shaped: any harness with two
   gateways wants it.

2. **basis gains `RuntimeBuilder::with_gateway_ring`**, with
   `with_gateway_ring_policy` and `with_gateway_ring_observer` beside it. It is
   `with_base_url` said more than once: each `GatewayMember` — a URL and its own
   key — is built exactly as a lone base URL is, in the wire `with_wire` names,
   under the id `with_provider` names, and the ring of them goes through
   `with_registered_provider`. basis touches the provider at `build` and never
   again; ADR-0018's lifecycle is untouched and ADR-0026's retirement stands.

3. **A ring is the endpoint answer.** `with_base_url` and `with_api_key` are
   refused beside it by name, as they are beside a host-supplied instance; a
   ring beside an instance is refused too. `with_provider` is still read, for
   the same reason a base URL reads it. An empty ring is refused rather than
   resolved from the environment as if unsaid; a policy or an observer stated
   with no members is inert.

4. **Members must front the same upstream serving the same model.** basis
   documents this at the knob and does not check it. A check would cost a
   request per member at build, against ADR-0018's build-does-not-connect rule,
   and would still not prove the transcript shapes agree.

5. **Rotation is visible only if the host asks.** basis has no logger. A turn
   answered by the fallback gateway reads exactly like one answered by the
   preferred one, so `with_gateway_ring_observer` exists for the host that will
   one day need to know which gateway answered.

## Consequences

- `RuntimeBuilder` gains three knobs and `basis::runtime` re-exports
  `GatewayMember`, `GatewayRingPolicy`, `GatewayRingEvent`, `GatewayMemberRef`,
  and `FailureKind`. `provider::ProviderError` gains `EmptyGatewayRing`;
  `AmbiguousProviderSource` can now name `with_gateway_ring`.
- The runtime's own retry schedule keeps pacing attempts. For a rotation to
  happen inside one turn the ring's failure threshold must not exceed the
  retry budget; the default threshold of five equals mentra's default budget,
  and a host that raised the budget for rate limits (basis's own defaults do)
  keeps rotation inside the turn.
- A Responses member is built with Hybrid `previous_response_id` chaining off,
  as every custom Responses endpoint in basis is, and each member owns its own
  session state; no continuation id crosses gateways. The ring's memory of who
  is answering is shared across fresh session scopes, because a gateway that is
  down is down for every conversation.
- basis depends on the mentra release carrying `gateway_ring`. Until it is
  published, the workspace carries a path patch to the sibling checkout, with
  the removal note that pattern has always carried.
- Not decided here: whether a gateway's health should be probed out of band
  rather than by the next real request. The next real request is the cheapest
  probe there is and the only one that measures what matters; an out-of-band
  probe is a proposal if the cost of a failed probe on a live turn ever bites.
