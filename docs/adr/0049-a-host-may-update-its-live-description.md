# ADR-0049: A host may update its live description

Status: accepted

## Context

The protocol card was snapshotted when the router was built, although a hosted
agent can learn skills while its tasks continue. Restarting a listener solely to
change descriptive metadata needlessly interrupts discovery and streams.

## Decision

`A2aService::update_identity` atomically replaces descriptive identity. Each card
request takes one snapshot, including the extended-card projection. Endpoint,
authentication, supported transports and protocol capabilities still derive from
the running service. Extension validation precedes replacement. This is an
embedding operation, not an A2A method. The host owns authorization and persistence.

## Consequences

Existing routers immediately see accepted updates; no task or listener is replayed
or restarted. Existing configured identities remain the default after reopening
unless the host persists and supplies its learned identity. Live HTTP regression
covers both cards, unchanged security/capabilities and rejection without replacement.

## Verification

Workspace all-feature tests and strict Clippy pass. The focused live HTTP test
passes. Ablating the identity replacement makes that regression fail; restoring
it passes. Both RPC and HTTP+JSON take a fresh card snapshot per request.
