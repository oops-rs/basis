# Security policy

lan runs with the authority of the user who starts it. The shell executor is
not a sandbox; use operating-system isolation when a boundary is required.

The local lifecycle service binds only to loopback and requires a per-service
capability token stored in a user-private descriptor directory. Do not copy
that token into issue reports or logs.

To report a vulnerability, first check the repository's Security tab for a
private vulnerability-reporting form and use it when it is available. That
feature is optional and may be disabled for this repository. If no private
form is available, contact a maintainer through a private GitHub message or
another private channel published by the oops-rs organization. Do not include
vulnerability details in a public issue. If no private route can be found,
open only a public issue asking for a private contact, without describing the
vulnerability or attaching credentials, tokens, or exploit material.

Include the affected commit, platform, reproduction steps, and whether a
credential or workspace data may have been exposed once a private channel is
established. Do not disclose an unpatched vulnerability publicly.
