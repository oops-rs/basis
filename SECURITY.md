# Security policy

lan runs with the authority of the user who starts it. The shell executor is
not a sandbox; use operating-system isolation when a boundary is required.

The local lifecycle service binds only to loopback and requires a per-service
capability token stored in a user-private descriptor directory. Do not copy
that token into issue reports or logs.

To report a vulnerability, open a private security report through the GitHub
repository's Security tab. Include the affected commit, platform, reproduction
steps, and whether a credential or workspace data may have been exposed. Do
not disclose an unpatched vulnerability publicly.
