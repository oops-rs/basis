# 0036 — A server may authenticate its own way

> Status: Accepted · 2026-10-13
> Amends [`0030-a2a-is-spoken-in-full.md`](0030-a2a-is-spoken-in-full.md)
> §8 — named bearer tokens become the default authenticator, not the only one.
> Preserves [`0028-a2a-is-the-agent-to-agent-protocol.md`](0028-a2a-is-the-agent-to-agent-protocol.md)
> §6 — a non-loopback bind still needs a server that authenticates someone.

## Context

ADR-0030 §8 gave `basis-a2a` named bearer tokens: each token's name is the
principal its tasks belong to, and a loopback server with no token is one
anonymous principal.

1. **A gateway does not hand out static tokens.** dial puts one A2A gateway in
   front of every agent and, for each request it forwards, signs a 60-second
   EdDSA JWT for that one agent: who the user is, or which anonymous device,
   plus the device in its own header. The served agent must accept those and
   nothing else, checked offline against the gateway's key set.
2. **Checking one is not basis's knowledge.** The claims, the issuer, the key
   set and when to refresh it are the gateway's. Linking a JWT library into
   `basis-a2a` for one deployment would be an opinion in the core (PROPOSAL
   Bet 4); the checker belongs with the gateway.
3. **The seam already exists.** Every request passes the gate, which finds
   the caller before a byte of the body is read and hands the handler a
   `Principal`. Everything after — ownership, quotas, stream caps, the
   extended card — reads only that `Principal`.
4. **The card must say what is asked for.** A caller reads the card's
   security schemes to know what to present; a JWT is HTTP bearer with
   `bearerFormat: "JWT"`, not the operator token the default declares.
5. **Gateway principals are not plain names.** dial's are `usr_…` and
   `anon:dev_…`; `Principal::new` refused `:`. Names are written only into
   the JSON task index and logs, never into a path.

## Decision

**`basis-a2a` decides who a request is from with an `Authenticator`. Named
tokens are the default one; an operator may give its own with
`ServeConfig::with_authenticator`.**

1. **The trait.** `authenticate(&RequestHead) -> BoxFuture<Result<Principal,
   Refusal>>`: the request's method, URI, version and headers, never its body.
   It is async because a key set may need a fetch, and boxed, as `TaskHost`
   is, so a server holds it as `Arc<dyn Authenticator>`. The gate calls it in
   the place the token comparison was — after the concurrency limit and the
   `Host` check, before the body — so a request awaiting it holds an
   in-flight place, and an authenticator bounds its own I/O. The public card
   is never passed to it.
2. **Refusals say only which.** `Refusal::Unauthenticated` is `401` with the
   authenticator's `WWW-Authenticate` challenge (`Bearer` by default);
   `Refusal::Forbidden` is `403`. Neither carries a body: what was wrong with
   a credential is not told to whoever sent it.
3. **The authenticator declares the card's security.** `security()` returns a
   `Security` — schemes by name, and requirements built so that each names a
   declared scheme, every scheme an alternative — or `None` for a server that
   authenticates nobody. It is read once, when the service opens. The card
   renders it through the same v1.0 `SecurityRequirement` shape on the wire as
   before.
4. **`None` is held to what a tokenless server is.** A server whose
   authenticator declares nothing binds only to loopback, serves no extended
   card, and serves push or asks its caller only when anonymous callers are
   trusted — exactly the rules a tokenless server already had.
5. **Every principal is a principal.** One built with `Principal::new` is
   authenticated: it owns its tasks apart from every other (ADR-0030 §8) and
   may be served the extended card, whether the authenticator calls it a user
   or an anonymous device. `Principal::new` accepts `:` beside letters,
   digits, `-`, `_` and `.`.
6. **One authenticator per server.** The default is the configured tokens —
   or, with none, the anonymous caller — byte for byte the server ADR-0030
   described. A custom one replaces the tokens rather than joining them, and
   `open` refuses a config with both rather than ignore either.

## Consequences

- A server behind a gateway accepts exactly the gateway's tokens, with the
  checker living in the gateway's repository; `basis-a2a` gains no JWT
  dependency.
- Existing servers, `basis serve --a2a`, and every card and `401` they produce
  are unchanged.
- The `http` crate's request head is in `basis-a2a`'s public API, re-exported
  as `RequestHead`; it was already there through `router`'s axum `Router`.
- An operator who wants tokens and something else at once composes them in
  one authenticator of its own. The default is not public; it would become so
  when a host needs to wrap it.
