//! Declared subprocess tools — the workspace binding of the tool contract.
//!
//! ADR-0012: **one contract per seam, and transports are adapters.** The
//! contract is mentra's `ExecutableTool`, and it has three bindings — a host
//! registering a tool in Rust, an MCP server basis connects to
//! (`crate::mcp`, behind a cargo feature), and this one: a data file in the
//! workspace declares a name, a description, a JSON schema and a command, and
//! basis wraps that command as a tool the model can call. pi's "CLI tools
//! instead of MCP", typed and schema-checked.
//!
//! It is a *core* feature and not part of the `mcp` feature, deliberately.
//! Custom tools were never MCP's to own; MCP was one of the ways to reach them,
//! and a build with `default-features = false` still has this.
//!
//! # The use case it shipped against
//!
//! Held, not built, until there was one — the rule Phase D set itself
//! (PROPOSAL.md Bet 7, `docs/REDESIGN.md`). The one that arrived: a production
//! Rust host needed Jenkins operations available to the model as tools. With no
//! registration surface at all, they became shell scripts invoked through
//! [`spawn`](crate::tools::spawn)'s command mode — and because a command mode
//! takes *one string*, the SQL queries and free-text questions those scripts
//! act on ended up base64-encoded inside the command line, to survive shell
//! quoting on the way through.
//!
//! That is the shape of the problem this fixes, and it is worth naming
//! precisely: the model was writing a shell command, so every value it carried
//! had to be escaped by a model that cannot be relied on to escape anything,
//! and the workaround for that was an encoding the model had to perform
//! correctly instead. Here the model fills in a JSON schema, basis serializes
//! it, and the program reads an object from its stdin. There is no shell on the
//! path, so there is nothing to quote and nothing to encode around quoting.
//!
//! # The manifest
//!
//! Tools may be supplied directly as typed [`DeclaredToolSpec`] values on
//! [`ToolsConfig`], or discovered from `.basis/tools.json` in the workspace and
//! `tools.json` in the global config directory. The typed list is already
//! final — basis does not expand `${VAR}` inside it — and outranks workspace,
//! then global declarations of the same name. The file locations are hooks'
//! locations, for hooks' reasons: JSON because the wire contract is already
//! JSON, `.basis/` because that is where basis's other workspace data lives.
//!
//! ```json
//! {
//!   "schema": 1,
//!   "tools": {
//!     "jenkins_job": {
//!       "description": "Trigger a Jenkins job and return its build number.",
//!       "input_schema": {
//!         "type": "object",
//!         "properties": { "job": { "type": "string" } },
//!         "required": ["job"]
//!       },
//!       "command": ["./.basis/tools/jenkins", "trigger"],
//!       "env": { "JENKINS_TOKEN": "${JENKINS_TOKEN}" },
//!       "side_effect": "external",
//!       "timeout_ms": 60000
//!     }
//!   }
//! }
//! ```
//!
//! An object keyed by name, like `.mcp.json`'s `mcpServers` and unlike
//! `hooks.json`'s array — because the name *is* the tool here. Two hooks may
//! share a name and both still run; two tools may not, so the format is one
//! where saying it twice is not expressible.
//!
//! `command` is an argv array, never a shell string, so nothing in a tool's
//! input can be reinterpreted as shell syntax. A relative program path resolves
//! against the workspace root, a bare name is left to `PATH`, and `cwd` — when
//! given — resolves against the root too. `${VAR}` expands in `command`, `cwd`
//! and `env`, with `${VAR:-default}` as in a shell, which is how a credential
//! reaches the program without being written in a file people commit.
//!
//! # What the program's environment is made of
//!
//! Three layers, outermost first, each overriding the one before it for a name
//! they share:
//!
//! 1. **basis's baseline.** The program is spawned through mentra's
//!    `BoundedCommand`, which clears the environment before setting what it
//!    was given — the same discipline a `spawn` command runs under — and basis
//!    passes back only what makes a program runnable: `PATH`, `HOME`, the
//!    temp and locale variables, each listed with its reason in
//!    `crate::subprocess`. Nothing else this process holds arrives: a
//!    credential reaches a program by being named in the two layers below,
//!    never by being in the air.
//! 2. **The runtime's fixed command environment**, from
//!    [`RuntimeBuilder::with_command_environment`](crate::RuntimeBuilder::with_command_environment).
//!    A host saying where its service lives is saying it about *every* process
//!    the runtime spawns, and a declared tool's program is one of those.
//! 3. **The manifest's `env`**, which is this tool's own statement and
//!    therefore the last word: between two statements about one name, the more
//!    specific one holds — the same direction in which a workspace's
//!    `tools.json` already beats the global one.
//!
//! None of the three is printed anywhere. `env` values are redacted from every
//! `Debug`, and neither layer appears in the approver's preview: the command
//! and its arguments are how a spawn is understood, while the environment is
//! where the credential is.
//!
//! What is *not* in the format is as deliberate: no `enabled` flag (a tool
//! nobody wants is a tool nobody declares), and no way to say a tool is
//! read-only — see [`SideEffect`].
//!
//! # What the program is handed, and what it answers with
//!
//! One JSON object on stdin: the input the model produced, matching the schema
//! the manifest declared, and nothing wrapped around it. basis invents no
//! envelope, because an envelope would be a second schema nobody declared — the
//! file's own `schema` field versions the *manifest*, and the payload's shape
//! is the tool author's to state.
//!
//! *Matching* is now enforced rather than hoped for: mentra reads each call
//! against the schema its tool published before it authorizes anything, so a
//! missing `required` field, a string where the schema said number, a value
//! outside an `enum`, or — under `additionalProperties: false` — a property
//! the schema never named, is refused with the field named and the program
//! never started. The check is deliberately partial (it ignores keywords it
//! does not implement rather than failing a valid call), so a schema is still
//! a statement to the model first and a gate second: a program that cares
//! about a constraint mentra does not implement still validates its own
//! stdin.
//!
//! Whatever the program prints on stdout is the result the model reads. Not
//! parsed, not interpreted: a program that wants to answer in JSON prints JSON
//! and the model reads JSON. The JSON contract is on the input side, where it
//! buys the thing that motivated the binding.
//!
//! A non-zero exit is a tool *error*, carrying the program's own stderr — the
//! same fail-loud voice as [`crate::hooks`], and for the same reason: what a
//! failure says is the only thing telling the model what to do next. So is a
//! program that cannot be started, one killed by a signal, and one that
//! outstays its deadline.
//!
//! # A declared tool is code from the workspace
//!
//! `.basis/tools.json` is workspace data, so cloning a repository and running
//! basis on it can register tools that repository chose, whose programs that
//! repository ships. That is the same exposure as [`crate::hooks`] and
//! [`crate::shell`], and it is bounded the same way — by whatever confines the
//! process (ADR-0004), not by a check in here.
//!
//! What *is* checked in here is the approval story, because that is basis's own
//! to get right. Every declared tool is consequential ([`SideEffect`] cannot
//! say otherwise), so every call reaches whatever
//! [`Approver`](crate::approval::Approver) the run installed, and what that
//! approver is shown is the command about to run rather than the name a
//! repository gave it (see [`DeclaredTool`]'s `authorization_preview`). A name
//! basis or mentra already answers to cannot be claimed at all, so no manifest
//! can quietly become `spawn`.

mod manifest;
mod registry;
mod tool;

pub use manifest::{
    DEFAULT_GLOBAL_TOOLS_FILE, DEFAULT_TOOL_TIMEOUT, DEFAULT_WORKSPACE_TOOLS_FILE,
    DeclaredToolError, DeclaredToolSpec, SideEffect, TOOLS_SCHEMA_VERSION, ToolsConfig,
    ToolsSource, discover, load,
};
pub use tool::DeclaredTool;

pub(crate) use manifest::load_supplied;
pub(crate) use registry::DeclaredTools;
