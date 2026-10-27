# 0047 — A request may carry fields basis does not model

> Status: Accepted · 2026-10-28
> Builds on [`0045-a-gateway-ring-is-configuration.md`](0045-a-gateway-ring-is-configuration.md).

## Context

dial's agents answer through `run.output`, which forces the answer tool.
DeepSeek behind the company LiteLLM refuses any forced `tool_choice` while
it is thinking, and nothing basis or mentra sends can turn thinking off: the
switch is a vendor parameter, and which spelling reaches the vendor depends
on the gateway. Measured on that LiteLLM:

- Chat Completions forwards `{"extra_body": {"thinking": {"type": "disabled"}}}`
  and refuses a top-level `thinking` as an unsupported parameter.
- Responses drops both, and honours `{"reasoning": {"effort": "none"}}`.

Each next vendor will have another such parameter. basis modelling them one
by one is a release per gateway quirk.

## Decision

1. **mentra's OpenAI-wire providers take request fields.**
   `ChatCompletionsProvider::with_request_fields` and
   `ResponsesProvider::with_request_fields` (mentra-provider 0.9.8) add
   top-level JSON fields to every request body, HTTP and websocket alike. The
   capability is generic to any harness, so it lives in mentra (ADR-0005).
2. **Fields only add.** A request on which mentra already set one of them
   fails, naming the field, before anything is sent. A silent override would
   change what the runtime believes it sent; a silent drop would hide the
   setting the host asked for. So `{"reasoning": …}` beside an `Effort` fails
   that turn instead of choosing between them.
3. **basis states them in two places.** `RuntimeBuilder::with_request_fields`
   for the endpoint basis builds — a lone base URL, or every ring member — and
   `GatewayMember::with_request_fields`, or `request_fields` on a member in
   `gateways` / `BASIS_GATEWAYS`, for the gateway-specific half. A field both
   name is refused at `build` naming the member.
4. **Only where basis builds the request.** Beside a host's own provider
   instance the fields are refused as an ambiguous provider source; for a
   preset with no base URL they are refused by name. Never dropped.

## Consequences

- A host reaches a vendor parameter with configuration, not a basis release.
- `ProviderError` gains `RequestFieldsWithoutBaseUrl` and
  `GatewayRequestFieldConflict`; exhaustive matches need arms.
- A ChatCompletions base URL with request fields is built as a provider-core
  instance (the ring member's shape) rather than through mentra's
  `with_openai_compatible`, which takes no fields.
- Not decided here: an `Effort::None`. Responses' `"none"` is reachable as a
  request field with no effort set; a first-class value would be a breaking
  change to mentra's `ReasoningEffort` and basis's `Effort`.
