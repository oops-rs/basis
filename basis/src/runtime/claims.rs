//! Who holds what on a runtime several workspaces share.
//!
//! One mentra runtime carries one tool registry and one skill registry, while
//! what is being put on them came out of one repository and belongs to that
//! repository. ADR-0018 made that split; the ledgers here are what make it
//! survive a host opening five repositories on one runtime — and, harder,
//! opening *one* repository twice, which is what `basis-host` does when two
//! sessions supply different MCP servers.
//!
//! One shape between them: a name or a root is **claimed** before anything is
//! registered, an identical second claimant **joins** the first and is counted
//! rather than refused, and what was registered comes off when the last holder
//! goes. mentra's own hold on each registration lives inside the claim, because
//! the claim is the thing that knows how many workspaces are still serving it.
//!
//! Where they differ is what a collision *means*, and each says so on its own
//! claim: a bridged MCP server name is suffixed, and a declared tool's name is
//! refused. The third ledger of this shape — one interception chain per
//! audience — lives beside the rest of interception, in
//! [`super::interception`].

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

// `skill_root_key` is the identity mentra's own registry matches a skills root
// by, so the holder count below is keyed exactly the way upstream keys it —
// where basis used to carry a copy of the rule that could drift out of step.
use mentra::{
    skill_root_key,
    tool::{AudienceToolRegistration, ToolAudience, ToolNameCollision},
};

use crate::tools::declared::DeclaredToolSpec;

use super::Runtime;

/// One MCP server name held on this runtime's single tool registry, and what
/// was bridged under it.
///
/// The names matter as much as the owner, and only for one question: *which
/// `mcp__*` tools on this runtime belong to somebody else?* mentra's audience
/// ladder answers it for a workspace in another audience, and cannot answer it
/// for a sibling open of the **same directory** — two such opens share one
/// audience by construction (`SessionScope::audience`), which is exactly the
/// pair `basis-host` produces when one repository is opened twice with
/// different client-supplied servers. So the tool names live beside the claim,
/// and [`Runtime::foreign_mcp_tools`] is what a mint asks.
#[cfg(feature = "mcp")]
#[derive(Debug)]
pub(super) struct McpClaim {
    /// The claiming workspace root; only it can release the name.
    root: PathBuf,
    /// The `mcp__<server>__<tool>` names bridged under this server, in the
    /// order they took. Empty until the connection succeeds, and empty forever
    /// for a server that never came up.
    tools: Vec<String>,
}

/// One tool name registered on this runtime by a workspace still open.
///
/// Both of the bindings a *workspace* registers share this entry, because they
/// share one name space: a repository's declaration
/// ([`crate::tools::declared`]) and a native tool the host handed this open
/// ([`WorkspaceBuilder::with_tool`](crate::WorkspaceBuilder::with_tool)) are
/// two programs competing for one string on one registry, and a ledger per
/// binding would let them both take it — with only mentra's same-audience
/// collision left to report it, at install time, in words about neither.
///
/// `holders` rather than a bare owner because one root may be open twice — a
/// host that opens the same repository for two concurrent callers — and the
/// first of those to drop must not free a name the second is still serving.
/// The entry goes when the count reaches zero, together with the tool itself.
#[derive(Debug)]
pub(super) struct ToolNameClaim {
    root: PathBuf,
    holders: usize,
    /// What is answering to the name, and therefore what a second live open of
    /// the same root has to match before it may join.
    program: ClaimedProgram,
    /// mentra's own hold on the audience registration, which is what keeps
    /// the tool answering. Kept beside the claim rather than by the workspace
    /// because the claim is what counts holders: the second open of a root
    /// joins this registration instead of making its own, and the tool has to
    /// outlive the first of them to drop. `None` between the claim and the
    /// registration, and for every holder after the first.
    registration: Option<AudienceToolRegistration>,
}

/// Which of a workspace's two tool bindings holds a claimed name.
///
/// The variants exist to answer one question — *may a second live open of this
/// same root join the registration already here?* — and they answer it
/// differently because only one of them is data.
#[derive(Debug)]
enum ClaimedProgram {
    /// A declaration: a manifest entry, or a spec the host supplied. Fully
    /// resolved data, so a sibling open declaring the same thing is provably
    /// asking for the same program and joins.
    Declared {
        /// The complete resolved declaration the live registration executes.
        /// Supplied same-root holders compare against it before joining.
        ///
        /// Boxed so a [`Native`](Self::Native) claim — which has nothing to
        /// compare and nothing to store — does not carry room for a
        /// declaration it will never hold.
        spec: Box<DeclaredToolSpec>,
        /// How many holders supplied this declaration rather than reading it
        /// from a file — the count that decides whether a difference between
        /// two same-root opens is worth refusing.
        supplied_holders: usize,
    },
    /// A native tool the host handed *this* open. Nothing about it is
    /// comparable — a `dyn ExecutableTool` is compiled code closing over
    /// whatever the host had at the call site, which is the whole reason to
    /// use one over a declaration — so no sibling open ever joins it.
    ///
    /// **This settles only who may register the name, not who may reach it.**
    /// The two opens share one audience, so the sibling that asks for nothing
    /// is refused nothing here and would still resolve what this one
    /// registered. What answers that is the pair every binding in this
    /// position needs: the name is hidden at the sibling's mint
    /// ([`Runtime::foreign_native_tools`]) and its call is refused live by
    /// [`ForeignToolGuard`](super::agents::ForeignToolGuard).
    Native,
}

/// The ledger itself, shareable.
///
/// An `Arc` for the reason [`super::agents::AgentRegistry`] is one: the live
/// guard that judges a call reads it, and that guard hangs off a workspace's
/// interception chain, which mentra's runtime holds — so a guard holding the
/// [`Runtime`] would close a cycle through mentra's own registry and the
/// runtime would never drop. Sharing the map alone closes nothing.
#[derive(Debug, Clone, Default)]
pub(crate) struct ToolClaims(Arc<Mutex<HashMap<String, ToolNameClaim>>>);

impl ToolClaims {
    fn lock(&self) -> MutexGuard<'_, HashMap<String, ToolNameClaim>> {
        self.0.lock().expect("tool claim map poisoned")
    }

    /// Whether `name` is a native tool some live open supplied.
    ///
    /// The guard's whole question, asked per call rather than read off a
    /// snapshot. Deliberately not qualified by root: a native claim under
    /// *another* directory is in another audience and cannot reach a call here
    /// at all, and if one ever did, denying it is still the right answer.
    pub(crate) fn holds_native(&self, name: &str) -> bool {
        self.lock()
            .get(name)
            .is_some_and(|claim| matches!(claim.program, ClaimedProgram::Native))
    }

    /// Every native tool name claimed on `root` that is not one of `own`.
    ///
    /// What a mint hides. Only this root's claims can matter: one directory is
    /// one audience, so a sibling open of *this* root is the only holder whose
    /// registration mentra will resolve for this workspace's sessions.
    pub(crate) fn foreign_native_on(
        &self,
        root: &Path,
        own: &[String],
    ) -> std::collections::BTreeSet<String> {
        self.lock()
            .iter()
            .filter(|(name, claim)| {
                claim.root == root
                    && matches!(claim.program, ClaimedProgram::Native)
                    && !own.iter().any(|mine| mine == *name)
            })
            .map(|(name, _)| name.clone())
            .collect()
    }
}

#[cfg(feature = "mcp")]
impl McpClaim {
    fn new(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
            tools: Vec::new(),
        }
    }
}

/// Permission to register one workspace tool, granted by
/// [`Runtime::claim_declared_tool`] and spent by
/// [`Runtime::install_claimed_tool`].
///
/// A value rather than a `bool` so the association between *which name, under
/// which root* and *may register* cannot come apart: the two used to agree only
/// because the caller passed the same two strings to both calls, and nothing in
/// the type system said they had to.
#[derive(Debug)]
#[must_use = "a claimed name with nothing registered under it is a tool the model cannot call"]
pub(crate) struct ToolNamePermit {
    name: String,
    root: PathBuf,
}

impl ToolNamePermit {
    /// Records a first holder and hands back the permission it owes a
    /// registration for. Called with the claim map already locked, which is
    /// what makes "nobody holds this name" and "this caller now does" one
    /// step.
    fn issue(
        claims: &mut HashMap<String, ToolNameClaim>,
        name: &str,
        root: &Path,
        program: ClaimedProgram,
    ) -> Self {
        claims.insert(
            name.to_string(),
            ToolNameClaim {
                root: root.to_path_buf(),
                holders: 1,
                program,
                registration: None,
            },
        );
        Self {
            name: name.to_string(),
            root: root.to_path_buf(),
        }
    }
}

impl ToolNameClaim {
    /// Why another repository's live open owns this name, in the words of
    /// whichever binding took it — a reader who has to free the name needs to
    /// know which of the two files or call sites to look at.
    fn taken_elsewhere(&self) -> String {
        let binding = match self.program {
            ClaimedProgram::Declared { .. } => "declares a tool",
            ClaimedProgram::Native => "supplies a native tool",
        };
        format!(
            "the workspace at {} is open on this runtime and {binding} by that name",
            self.root.display()
        )
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum DeclaredToolOrigin {
    File,
    Supplied,
}

impl Runtime {
    /// Claims an MCP server name on this runtime's tool registry for the
    /// workspace at `root`, returning the name that took effect.
    ///
    /// Bridged tools are namespaced `mcp__<server>__<tool>` on one registry,
    /// so a name two workspaces both configure would collide: the second
    /// claimant gets a deterministic suffix derived from its root instead, and
    /// reports it through [`Workspace::mcp_servers`](crate::Workspace::mcp_servers).
    #[cfg(feature = "mcp")]
    pub(crate) fn claim_mcp_server(&self, name: &str, root: &Path) -> String {
        let mut claims = self.mcp_claims.lock().expect("mcp claim map poisoned");

        if !claims.contains_key(name) {
            claims.insert(name.to_string(), McpClaim::new(root));
            return name.to_string();
        }

        // `-` cannot appear in the `__` separators mentra parses on, so a
        // suffixed name still round-trips through `parse_mcp_tool_name`.
        let mut effective = format!("{name}-{}", root_suffix(root));
        let mut attempt = 2_u32;
        while claims.contains_key(&effective) {
            effective = format!("{name}-{}-{attempt}", root_suffix(root));
            attempt += 1;
        }

        claims.insert(effective.clone(), McpClaim::new(root));
        effective
    }

    /// Records what a connected server actually bridged under the name it
    /// claimed, so a sibling open can be told which names are not its own.
    ///
    /// Separate from the claim because the two happen at different times: a
    /// name is claimed before the connection is attempted — the manager
    /// namespaces every tool by it — and what came back is only known after.
    /// Only the owning root may write, for
    /// [`release_mcp_claim`](Self::release_mcp_claim)'s reason.
    #[cfg(feature = "mcp")]
    pub(crate) fn record_bridged_tools(&self, name: &str, root: &Path, tools: Vec<String>) {
        let mut claims = self.mcp_claims.lock().expect("mcp claim map poisoned");
        if let Some(claim) = claims.get_mut(name)
            && claim.root == root
        {
            claim.tools = tools;
        }
    }

    /// Every `mcp__*` name on this runtime whose server is not one of `own`.
    ///
    /// What a mint hides from its model. Two sources, because two kinds of
    /// `mcp__*` registration are reachable from a workspace's session and
    /// neither is covered by mentra's audience ladder:
    ///
    /// - **A sibling open of the same directory.** Its bridged tools are
    ///   registered for the audience this workspace also resolves in — one
    ///   directory is one audience — so mentra reports them `Visible`. That is
    ///   the pair `basis-host` deliberately produces when two ACP sessions open
    ///   one repository with different `mcpServers`, and without this the
    ///   session that supplied none could list *and call* the other's
    ///   authenticated server.
    /// - **A host tool registered globally under an `mcp__`-shaped name**
    ///   ([`RuntimeBuilder::with_tool`]). A global is visible to every
    ///   audience on purpose; a name shaped like a bridged tool of a server
    ///   this workspace never configured is not what that rule is for.
    ///
    /// Hiding rather than refusing, and by name: these tools belong to
    /// somebody still open and still serving them. A name in `hidden_tools` is
    /// neither offered nor invokable (`Agent::name_is_allowed`), which is the
    /// property that matters — a model that guessed the name gets the same
    /// answer as one that was never shown it.
    #[cfg(feature = "mcp")]
    pub(crate) fn foreign_mcp_tools(&self, own: &[String]) -> std::collections::BTreeSet<String> {
        let mine = |server: &str| own.iter().any(|owned| owned == server);
        let mut foreign = std::collections::BTreeSet::new();

        for (server, claim) in self
            .mcp_claims
            .lock()
            .expect("mcp claim map poisoned")
            .iter()
        {
            if mine(server) {
                continue;
            }
            foreign.extend(claim.tools.iter().cloned());
        }

        for descriptor in self.mentra.tools() {
            let name = &descriptor.provider.name;
            if let Some((server, _)) = mentra::mcp::parse_mcp_tool_name(name)
                && !mine(server)
            {
                foreign.insert(name.clone());
            }
        }

        foreign
    }

    /// Releases a claim [`claim_mcp_server`](Self::claim_mcp_server) granted.
    /// Only the owning root can release, so one workspace's drop cannot free a
    /// name another still serves.
    #[cfg(feature = "mcp")]
    pub(crate) fn release_mcp_claim(&self, name: &str, root: &Path) {
        let mut claims = self.mcp_claims.lock().expect("mcp claim map poisoned");
        if claims.get(name).is_some_and(|claim| claim.root == root) {
            claims.remove(name);
        }
    }

    /// Claims a declared tool's name for the workspace at `root`, or says who
    /// holds it.
    ///
    /// Refused rather than suffixed, which is where this parts company with
    /// [`claim_mcp_server`](Self::claim_mcp_server). A bridged tool's name is
    /// already synthetic (`mcp__<server>__<tool>`), so renaming one on a
    /// collision costs nothing; a declared tool's name is what the model calls,
    /// what an operator writes in a remembered rule, and what a
    /// `.basis/hooks.json` entry matches on, so a silently renamed one is a
    /// guard that silently stops matching.
    ///
    /// The check that matters is the first-time one: mentra's registry is a map
    /// and `register_tool` *replaces*, so without this a workspace file could
    /// declare a tool called `spawn` and take over the name basis's own tool —
    /// and every rule an operator ever wrote about it — answers to.
    ///
    /// `Ok(Some(claim))` means this caller is the name's *first* live holder and
    /// owes the runtime a registration, which
    /// [`install_claimed_tool`](Self::install_claimed_tool) takes the claim to
    /// perform. `Ok(None)` means a sibling open of the same root already
    /// registered it, and the tool on the runtime is the one that open is
    /// serving. One name is one program, so the second open of a repository
    /// joins the registration rather than replacing it under the first open's
    /// running agents.
    ///
    /// The claim is a value rather than a `bool` so that "which name, under
    /// which root" travels with the permission to register instead of being
    /// re-derived at the install: the two used to agree only because the caller
    /// passed the same strings twice.
    pub(crate) fn claim_declared_tool(
        &self,
        root: &Path,
        spec: &DeclaredToolSpec,
        origin: DeclaredToolOrigin,
    ) -> Result<Option<ToolNamePermit>, String> {
        let name = &spec.name;
        let mut claims = self.tool_claims.lock();

        match claims.get_mut(name) {
            Some(claim) if claim.root != root => Err(claim.taken_elsewhere()),
            Some(claim) => match &mut claim.program {
                // A native tool is nobody's to join; see [`ClaimedProgram`].
                ClaimedProgram::Native => Err(
                    "another live open of this workspace supplied a native tool under that name"
                        .to_string(),
                ),
                ClaimedProgram::Declared {
                    spec: held,
                    supplied_holders,
                } if **held != *spec
                    && (*supplied_holders > 0
                        || matches!(origin, DeclaredToolOrigin::Supplied)) =>
                {
                    Err(
                        "another live open of this workspace supplied different configuration \
                         under that name"
                            .to_string(),
                    )
                }
                ClaimedProgram::Declared {
                    supplied_holders, ..
                } => {
                    if matches!(origin, DeclaredToolOrigin::Supplied) {
                        *supplied_holders += 1;
                    }
                    claim.holders += 1;
                    Ok(None)
                }
            },
            None if self.registers_tool(name) => {
                Err("this runtime already offers a tool by that name".to_string())
            }
            None => Ok(Some(ToolNamePermit::issue(
                &mut claims,
                name,
                root,
                ClaimedProgram::Declared {
                    spec: Box::new(spec.clone()),
                    supplied_holders: usize::from(matches!(origin, DeclaredToolOrigin::Supplied)),
                },
            ))),
        }
    }

    /// Claims a name for a native tool the host supplied to the workspace at
    /// `root`, or says who holds it.
    ///
    /// The declared claim above with one rule changed and the reason stated on
    /// [`ClaimedProgram::Native`]: there is no `Ok(None)` here, because there
    /// is no joining a native tool. Every other refusal is the same refusal —
    /// a name this runtime already answers to globally (`spawn`, a mentra
    /// builtin, a [`RuntimeBuilder::with_tool`](crate::RuntimeBuilder::with_tool)
    /// global) is not a name a workspace may take, and neither is one another
    /// repository open on this runtime is already serving.
    pub(crate) fn claim_native_tool(
        &self,
        root: &Path,
        name: &str,
    ) -> Result<ToolNamePermit, String> {
        let mut claims = self.tool_claims.lock();

        match claims.get(name) {
            Some(claim) if claim.root != root => Err(claim.taken_elsewhere()),
            Some(_) => {
                Err("another live open of this workspace already answers to that name".to_string())
            }
            None if self.registers_tool(name) => {
                Err("this runtime already offers a tool by that name".to_string())
            }
            None => Ok(ToolNamePermit::issue(
                &mut claims,
                name,
                root,
                ClaimedProgram::Native,
            )),
        }
    }

    /// Puts a claimed tool on the registry, for the claiming workspace's
    /// audience alone.
    ///
    /// Audience-scoped rather than global because both bindings that come here
    /// are a *workspace's*: a declaration is a repository's statement about a
    /// program, and a native tool is what one host open handed one workspace.
    /// On a runtime serving five repositories, a global registration would
    /// offer either to the other four's models. mentra's resolution ladder
    /// answers that for basis now — a name held only by another audience
    /// resolves to `Hidden`, so it is neither listed nor reachable by guessing
    /// it. Nothing here freezes a roster to get that: mentra rebuilds a
    /// visible set per round from the live registry, so a tool registered
    /// after an agent was minted is offered to that agent's own audience and
    /// to no other, and the exact-agent rung above it — where `read_tool_result`
    /// registers itself — keeps answering either way.
    ///
    /// The guard goes into the claim, which is the thing that knows how many
    /// workspaces are holding this name; dropping the claim drops the guard and
    /// takes the tool off the registry in the same breath.
    ///
    /// **Which claim is not a question this has to ask.** It takes the
    /// [`ToolNamePermit`] the claim granted and does the whole of the work
    /// under the claim lock: the entry is found *before* anything is
    /// registered, so there is no window in which a guard exists that nothing
    /// holds, and no re-lookup by a string the caller had to pass twice. A
    /// claim that is gone by the time it is spent is an `Err` and not a silent
    /// success: the alternative reports a live declared tool that nothing on
    /// the runtime answers to.
    pub(crate) fn install_claimed_tool<T>(
        &self,
        audience: &ToolAudience,
        claim: ToolNamePermit,
        tool: T,
    ) -> Result<(), String>
    where
        T: mentra::tool::ExecutableTool + 'static,
    {
        let mut claims = self.tool_claims.lock();

        // Unreachable in practice — the claim map serializes every opener on
        // this runtime and nothing between the claim and here releases one.
        // Said out loud anyway, because the failure it would otherwise become
        // is a name the workspace reports as live with no program behind it.
        // The permit is `#[must_use]`, so the other half of that failure — a
        // claimed name nobody ever registers under — cannot pass review either.
        let Some(entry) = claims
            .get_mut(&claim.name)
            .filter(|entry| entry.root == claim.root)
        else {
            return Err(
                "the claim on that name was released while this workspace was opening".to_string(),
            );
        };

        let registration = self
            .mentra
            .try_register_tool_for_audience(audience.clone(), tool)
            .map_err(|collision: ToolNameCollision| {
                format!(
                    "something registered a tool called '{}' on this runtime while this \
                     workspace was opening",
                    collision.name
                )
            })?;

        // **What was claimed and what was registered are checked against each
        // other, not assumed equal.** basis reads a tool's descriptor to learn
        // the name it must claim; mentra reads its own to learn the key it
        // registers under. For an ordinary tool those are the same string
        // twice, but `descriptor()` is a caller's method and nothing makes it
        // pure — so a tool that answered differently the second time would
        // otherwise sit on the registry under a name no claim covers, missing
        // every rule this ledger exists to enforce, `mcp__` included.
        //
        // mentra 0.26 is what makes the check possible: the registration hands
        // back the exact snapshot it used, so this compares the two rather
        // than re-asking the tool a third time and trusting that answer.
        // Dropping the registration unregisters precisely that generation.
        let registered = &registration.descriptor().provider.name;
        if *registered != claim.name {
            let registered = registered.clone();
            drop(registration);
            return Err(format!(
                "its descriptor named '{}' when the name was claimed and '{registered}' when it \
                 was registered; a tool has to be the same tool both times",
                claim.name
            ));
        }

        entry.registration = Some(registration);
        Ok(())
    }

    /// Releases a claim [`claim_declared_tool`](Self::claim_declared_tool)
    /// granted, taking the tool off the runtime when the last holder goes.
    pub(crate) fn release_declared_tool(
        &self,
        name: &str,
        root: &Path,
        origin: DeclaredToolOrigin,
    ) {
        self.release_tool_claim(name, root, matches!(origin, DeclaredToolOrigin::Supplied));
    }

    /// Releases a claim [`claim_native_tool`](Self::claim_native_tool)
    /// granted. Always the last holder, because nothing joins a native claim.
    pub(crate) fn release_native_tool(&self, name: &str, root: &Path) {
        self.release_tool_claim(name, root, false);
    }

    /// The one release both bindings share.
    ///
    /// Only the owning root can release, so one workspace's drop cannot free a
    /// name another still serves. Removing the claim is what makes the claim
    /// map and mentra's registry say the same thing: the registration guard
    /// goes with it, so a released name is free because nothing answers to it
    /// any more, rather than free-with-a-stale-entry-behind-it.
    fn release_tool_claim(&self, name: &str, root: &Path, supplied: bool) {
        let mut claims = self.tool_claims.lock();

        let Some(claim) = claims.get_mut(name) else {
            return;
        };
        if claim.root != root {
            return;
        }

        claim.holders = claim.holders.saturating_sub(1);
        if let ClaimedProgram::Declared {
            supplied_holders, ..
        } = &mut claim.program
            && supplied
        {
            *supplied_holders = supplied_holders.saturating_sub(1);
        }
        if claim.holders == 0 {
            // Under the claim lock, so no other claimant can see the name free
            // while the tool is still registered: the removed claim owns the
            // registration guard, and dropping it here is the unregister.
            claims.remove(name);
        }
    }

    /// Registers a workspace's skills roots and counts it as a holder of each.
    ///
    /// mentra 0.24 made registration all-or-nothing and gave a host
    /// `unregister_skills_dirs` to take a root back, which is what lets a
    /// workspace stop leaving its skills on a runtime that outlives it. What
    /// mentra cannot know is *how many* workspaces asked for a root: a root is
    /// one entry upstream however often it is registered, and on a shared
    /// runtime (ADR-0018) every workspace registers the same two user-scoped
    /// roots. Unregistering on the first drop would take the user's own skills
    /// away from every repository still open, so the count lives here — the
    /// same ledger, and for the same reason, as
    /// [`claim_declared_tool`](Self::claim_declared_tool). Upstream says as
    /// much itself: [`mentra::skill_root_key`]'s own doc tells a host counting
    /// several holders of one root to capture that key and hold it, which is
    /// what the map below is.
    ///
    /// The registration happens under the holder lock, so nothing can observe
    /// a root counted but absent, or free one between the register and the
    /// count. An `Err` leaves both sides untouched: mentra commits nothing,
    /// and no holder is recorded.
    pub(crate) fn register_skill_roots(
        &self,
        roots: &[PathBuf],
    ) -> Result<(), mentra::SkillLoadError> {
        let mut holders = self
            .skill_root_holders
            .lock()
            .expect("skill root holder map poisoned");

        self.mentra.register_skills_dirs(roots)?;
        for root in roots {
            *holders.entry(skill_root_key(root)).or_insert(0) += 1;
        }
        Ok(())
    }

    /// Releases the holds [`register_skill_roots`](Self::register_skill_roots)
    /// recorded, taking a root off the runtime when its last holder goes.
    ///
    /// A root nobody else holds leaves mentra's registry entirely: the skills
    /// it contributed stop being listed to the model, `load_skill` refuses
    /// them, and a name this root had shadowed resolves to the weaker root
    /// again. Dropping the last root of all also withdraws `load_skill`, which
    /// the next workspace to open restores.
    ///
    /// Under the holder lock, like the declared-tool release above, so no
    /// other opener can see a root free while its skills are still registered.
    pub(crate) fn release_skill_roots(&self, roots: &[PathBuf]) {
        let mut holders = self
            .skill_root_holders
            .lock()
            .expect("skill root holder map poisoned");

        for root in roots {
            let key = skill_root_key(root);
            let Some(count) = holders.get_mut(&key) else {
                continue;
            };
            *count = count.saturating_sub(1);
            if *count == 0 {
                holders.remove(&key);
                self.mentra.unregister_skills_dir(&key);
            }
        }
    }

    /// Every native tool name claimed on `root` that this open did not supply.
    ///
    /// What a mint hides, beside [`foreign_mcp_tools`](Self::foreign_mcp_tools)
    /// and for the third case of the same problem: mentra's audience ladder
    /// hides another *directory's* tools, and two live opens of one directory
    /// share one audience by construction, so a native tool the other open
    /// supplied resolves here as readily as its own.
    pub(crate) fn foreign_native_tools(
        &self,
        root: &Path,
        own: &[String],
    ) -> std::collections::BTreeSet<String> {
        self.tool_claims.foreign_native_on(root, own)
    }

    /// The ledger, for the live guard that judges a call by it.
    pub(crate) fn tool_claims(&self) -> ToolClaims {
        self.tool_claims.clone()
    }

    /// The descriptor of the workspace tool live under `name`.
    ///
    /// Read off basis's own hold on the registration, because mentra exposes no
    /// reader for an audience's tools: `Runtime::tools` and
    /// `Runtime::tool_descriptor` both walk the global map only (an upstream
    /// candidate), so an audience-registered tool is invisible to both.
    /// `#[cfg(test)]` because the only caller is the test that pins *which*
    /// program a name is serving when one repository is open twice.
    #[cfg(test)]
    pub(crate) fn claimed_tool_descriptor(
        &self,
        name: &str,
    ) -> Option<mentra::tool::RuntimeToolDescriptor> {
        self.tool_claims
            .lock()
            .get(name)?
            .registration
            .as_ref()
            .map(|registration| registration.descriptor().clone())
    }

    /// Whether mentra's registry already answers to `name` globally — a
    /// builtin, basis's own `spawn`, or a host tool.
    ///
    /// Globals only, which is the question worth asking: an audience-scoped
    /// name belonging to another workspace is already refused by the claim map
    /// above, and one belonging to *this* workspace is refused by mentra's own
    /// same-audience collision check when the registration is attempted.
    fn registers_tool(&self, name: &str) -> bool {
        self.mentra
            .tools()
            .iter()
            .any(|descriptor| descriptor.provider.name == name)
    }
}

/// Eight hex characters of FNV-1a over the workspace root: stable across
/// processes, so the same collision resolves to the same name every run.
#[cfg(feature = "mcp")]
fn root_suffix(root: &Path) -> String {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;

    let mut hash = OFFSET;
    for byte in root.as_os_str().as_encoded_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(PRIME);
    }

    format!("{:08x}", (hash >> 32) as u32 ^ hash as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "mcp")]
    #[test]
    fn a_taken_server_name_is_suffixed_and_a_released_one_is_free_again() {
        use std::path::Path;

        let runtime = Runtime::builder()
            .with_base_url("http://127.0.0.1:1/v1")
            .with_api_key("test-key")
            .with_ephemeral_history()
            .build()
            .expect("builds");

        let first = runtime.claim_mcp_server("fs", Path::new("/repo/one"));
        let second = runtime.claim_mcp_server("fs", Path::new("/repo/two"));
        let again = runtime.claim_mcp_server("fs", Path::new("/repo/two"));

        assert_eq!(first, "fs", "the first claimant keeps the plain name");
        assert_ne!(second, "fs", "the second must not collide in the registry");
        assert!(second.starts_with("fs-"), "{second}");
        assert_ne!(again, second, "every live claim is its own namespace");

        // Only the owner can free a name.
        runtime.release_mcp_claim("fs", Path::new("/repo/two"));
        runtime.release_mcp_claim(&second, Path::new("/repo/one"));
        assert_eq!(
            runtime.claim_mcp_server("fs", Path::new("/repo/three")),
            format!("fs-{}", root_suffix(Path::new("/repo/three"))),
            "a name someone else holds stays held"
        );

        runtime.release_mcp_claim("fs", Path::new("/repo/one"));
        assert_eq!(
            runtime.claim_mcp_server("fs", Path::new("/repo/four")),
            "fs",
            "a released name is claimable plain again"
        );
    }

    /// The case mentra's audience ladder cannot answer: two live opens of one
    /// directory share one audience, so a sibling's bridged tools resolve
    /// `Visible` for either of them. What tells them apart is which servers
    /// each open actually configured, which is what this asks.
    #[cfg(feature = "mcp")]
    #[test]
    fn a_bridged_tool_is_foreign_to_every_open_that_did_not_configure_its_server() {
        use std::path::Path;

        let runtime = Runtime::builder()
            .with_base_url("http://127.0.0.1:1/v1")
            .with_api_key("test-key")
            .with_ephemeral_history()
            .build()
            .expect("builds");

        // One repository, opened twice: the first client supplied an
        // authenticated server, the second supplied none. Same root, so the
        // same audience.
        let root = Path::new("/repo");
        let server = runtime.claim_mcp_server("prod-db", root);
        runtime.record_bridged_tools(&server, root, vec!["mcp__prod-db__query".to_string()]);

        assert_eq!(
            runtime
                .foreign_mcp_tools(&[])
                .into_iter()
                .collect::<Vec<_>>(),
            ["mcp__prod-db__query"],
            "the open that configured no servers must not be offered the other's"
        );
        assert!(
            runtime
                .foreign_mcp_tools(std::slice::from_ref(&server))
                .is_empty(),
            "and the open that configured it keeps it"
        );

        // Released with its workspace: a name nothing serves is nobody's to
        // hide.
        runtime.release_mcp_claim(&server, root);
        assert!(runtime.foreign_mcp_tools(&[]).is_empty());
    }

    /// A host tool registered globally under an `mcp__`-shaped name is visible
    /// to every audience by the rule that makes globals global — which is not
    /// the rule a name shaped like somebody's bridged server tool should get.
    #[cfg(feature = "mcp")]
    #[test]
    fn a_global_tool_shaped_like_a_bridged_one_is_foreign_to_every_workspace() {
        use mentra::tool::{
            ParallelToolContext, RuntimeToolDescriptor, ToolDefinition, ToolExecutor, ToolResult,
        };
        use serde_json::{Value, json};

        struct HostAdmin;

        impl ToolDefinition for HostAdmin {
            fn descriptor(&self) -> RuntimeToolDescriptor {
                RuntimeToolDescriptor::builder("mcp__internal__admin")
                    .description("the host's own tool")
                    .input_schema(json!({"type": "object"}))
                    .build()
            }
        }

        #[async_trait::async_trait]
        impl ToolExecutor for HostAdmin {
            async fn execute(&self, _ctx: ParallelToolContext, _input: Value) -> ToolResult {
                Ok("administered".to_string())
            }
        }

        let runtime = Runtime::builder()
            .with_base_url("http://127.0.0.1:1/v1")
            .with_api_key("test-key")
            .with_ephemeral_history()
            .with_tool(HostAdmin)
            .build()
            .expect("builds");

        assert_eq!(
            runtime
                .foreign_mcp_tools(&[])
                .into_iter()
                .collect::<Vec<_>>(),
            ["mcp__internal__admin"],
            "no workspace configured a server called `internal`"
        );
    }
}
