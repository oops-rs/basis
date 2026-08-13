# lan — Proposal

> The *why* behind lan: the problem, the one idea, and the bets we made because of
> it. For *what it is* see [`README.md`](../README.md); for *how it's built* see
> [`ARCHITECTURE.md`](ARCHITECTURE.md); for ordered evolution ideas see
> [`proposals/`](proposals/); for the locked decisions see [`adr/`](adr/). This
> document is opinion with reasons, not a spec.

## 1. The problem

Coding agents ship as monolithic products: a TUI, a login, a brand. Using their
intelligence *inside your own thing* — a web page, an editor, a scheduled job, another
program — means either scraping a CLI built for humans or rebuilding the harness
yourself. Meanwhile the runtimes underneath (Mentra included) are libraries by design,
but the distance from "runtime" to "usable agent" is a pile of unwritten glue: context
conventions, session lifecycle, permission surfacing, a wire protocol, confinement.
Every application that embeds an agent rebuilds that glue — zentox did, and its
[feedback in the public Mentra repository](https://github.com/oops-rs/mentra) is a catalog of exactly this
distance.

The existing answers each fail in a specific way:

- **Full products** (Claude Code, codex, pi) carry a TUI and a product identity; the
  embeddable surface is an afterthought — codex's app-server is proprietary
  not-quite-JSON-RPC that even its own SDKs bypass; pi's RPC mode needs a bespoke
  client per integrator.
- **Bare runtimes** (mentra alone) leave every application to reinvent AGENTS.md
  loading, skills, session mapping, and protocol handling — glue that is generic in
  shape but rebuilt per app.
- **Domain-specific agents** (a "bug fixer", a "doc bot") bake the mission into code,
  so the next mission means the next fork.

## 2. The one idea

> A harness is a **library with a protocol front door**. lan packages the generic
> glue — context conventions, sessions, extension seams, confinement — over a proven
> runtime, and speaks the standard protocol so any client drives it. The intelligence
> is rented from the model; the presentation is owned by the client; the mission
> arrives as data.

The durable value is the glue done once, well: conventions in (AGENTS.md, skills,
`.mcp.json`), events out (one stream feeding ACP, JSONL, and any future surface), and
a kernel-enforced boundary around the workspace.

## 3. The bets

Stated as *what we believe → what it buys → what we therefore refuse to do*.

### Bet 1 — Library first, binary second
**Believe:** embedding is the primary case; the terminal is one client among many.
**Buys:** the crate is the SDK; the binary is a thin shell; Rust hosts embed
in-process with zero protocol overhead. **Refuse:** a TUI, themes, keybindings, or any
presentation opinion in the core. [ADR-0003]

### Bet 2 — Standard protocol over bespoke
**Believe:** ACP does to agents what LSP did to language servers; a protocol with
existing clients beats a better bespoke one with none. **Buys:** Zed, JetBrains,
acp-ui, acp-mobile work day one; the web UI is adopted, not built. **Refuse:** to
invent our own RPC (pi's client-per-integrator and codex's SDK-bypassed app-server are
the cautionary tales). [ADR-0002]

### Bet 3 — Rent the loop, own the glue
**Believe:** the agent loop, providers, tools, and persistence are mentra's problem,
already solved and tested. **Buys:** lan's effort goes to the only thing lan can be —
conventions, protocol, packaging; nous set the precedent
(`mentra is the loop`, the corresponding upstream runtime decision).
**Refuse:** to re-implement runtime machinery in lan to feel in control. [ADR-0001]

### Bet 4 — The core has no opinions
**Believe:** task-specific behavior is data — the prompt, the workspace, config —
never code. A periodic code-health loop, a nightly dependency bump, and an interactive
refactor are the same binary. **Buys:** one harness serves every mission; no fork per
domain. **Refuse:** task types, pipelines, or domain vocabulary in the core; a use
case that "needs" core code is an extension-seam gap to close generically.

### Bet 5 — Confinement is the kernel's job
**Believe:** prompts and in-process policy are not security boundaries; the workspace
guarantee must come from the OS. **Buys:** the read-only-root Docker pattern gives the
guarantee at near-zero cost today; codex's per-command native sandbox is the proven v2
path. **Refuse:** to sell in-process path checks as safety (they remain as *hygiene* —
`.git/hooks` write-deny — not as the boundary). [ADR-0004], amended by [ADR-0013]: the
belief stands, but lan documents the patterns ([`containerization.md`](containerization.md))
rather than shipping an image, and commands are on by default.

### Bet 6 — Co-evolve with mentra, keep the seam honest
**Believe:** same author on both sides is leverage, and a trap: gaps can be fixed
where they belong, or quietly worked around where they don't. **Buys:** generic
capability lands in mentra (session branching, compaction checkpoints, tool profiles);
lan stays thin; every gap is filed as a mentra issue even when fixed immediately, so
the API story stays legible to other mentra users. **Refuse:** lan-side workarounds
for mentra-shaped holes. [ADR-0005]

### Bet 7 — Earn every part
**Believe:** the failure mode of harnesses is breadth — extension machinery built
ahead of demonstrated need. **Buys:** extensions start at MCP servers + subprocess
hooks (process-isolated, any language); an embedded scripting layer (wasm/rhai) is
written down as a proposal, not built, until friction is shown. Deferred ideas live in
[`proposals/`](proposals/) with the properties they must preserve. **Refuse:** to keep
machinery that isn't pulling its weight.

## 4. The loop we're chasing

**Embed-by-default.** The bar: when the author (or anyone) needs agent capability in a
new context — a repo chore, a web page, an editor, a cron job — reaching for lan is
cheaper than wiring mentra by hand, and the missing piece surfaces as a mentra issue
or a lan proposal rather than app-local glue. Every run also stress-tests mentra from
the consumer's seat — the zentox feedback loop, made permanent.

## 5. What lan is not

- **Not a product.** No TUI, no brand experience; clients own presentation.
- **Not a mission.** Bug-fixing, doc-tending, dependency-bumping are prompts and
  workspace data, never lan features.
- **Not a runtime.** The loop, tools, and persistence are mentra; lan does not
  duplicate them.
- **Not a security product.** The boundary is the OS's (a container you run, later a
  native sandbox); lan's own checks are hygiene, and lan ships no boundary of its own.

## 6. Why this shape ages well

Models improve on their own schedule; protocols and conventions compound. AGENTS.md,
skills, and MCP are cross-agent conventions that get more valuable as more tools
speak them; ACP clients multiply independently of lan. A harness that owns exactly
the glue — and rents both the intelligence and the presentation — gets better for
free on both frontiers while staying small enough to embed anywhere.

---

*Pointers:* [`README.md`](../README.md) (what) · [`ARCHITECTURE.md`](ARCHITECTURE.md)
(how) · [`proposals/`](proposals/) (ordered evolution ideas) · [`adr/`](adr/)
(locked decisions) · [`p0-groundwork.md`](p0-groundwork.md) (research: zentox
requirements, pi prior art, mentra API reality check).
