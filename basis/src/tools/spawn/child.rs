//! Who the child is — the seam a host's policy decides it through (D4).
//!
//! `spawn` has always minted a subagent as an exact clone of its parent: same
//! roster, same model, same system prompt, bounds inherited through
//! [`ToolContext::child_run_options`](mentra::tool::ToolContext::child_run_options).
//! That is the right default and it remains the default, byte for byte. What
//! it could not express is the host that wants a *cheap triage child beside a
//! full fixer*: delegation where the child's roster, model, or voice is a
//! decision rather than an inheritance.
//!
//! [`RuntimeBuilder::with_child_policy`](crate::RuntimeBuilder::with_child_policy)
//! is that decision's seam. The policy is a function from [`ChildContext`] —
//! what `spawn` knows about the delegation it is about to start — to a
//! [`ChildSpec`] — which of the three inherited facts to override. It is
//! consulted per delegation, in two places that must agree: once for the
//! approver's preview, so a remembered rule can match on what the child *will
//! be* (the additive `child` key in the structured input), and once at
//! execution, where the overrides are applied to mentra's
//! `DisposableSubagentTemplate` and the child is spawned from it. A policy
//! should therefore be a pure function of its context; one that answers
//! differently between the two consultations shows the approver a child it
//! will not spawn.
//!
//! # What a spec deliberately does not carry
//!
//! - **Bounds.** Deadline, budgets, cancellation and the provider-retry
//!   schedule already ride `child_run_options`, which is what makes a child's
//!   spend land on its parent's counter. A second spelling here would be a
//!   second bounds system for one child.
//! - **An `Append` for the system prompt.** [`ChildSpec::with_system`]
//!   replaces wholesale — the mapping onto mentra's
//!   `DisposableSubagentTemplate::with_system`, which is itself
//!   replace-wholesale. "Parent's prompt plus more" is not offered because it
//!   is not honest to offer: the parent's rendered prompt is sealed inside
//!   the template with no reader, so basis cannot compose with it — and a
//!   variant that basis would have to refuse at spawn time is worse than a
//!   narrower knob. A policy that wants parent-plus-more writes the whole
//!   text it means; mentra still appends its standard subagent instructions
//!   after whatever the child's system is, overridden or not, exactly as it
//!   does for every subagent.
//!
//! # What an override cannot escape
//!
//! The depth guard is checked before the policy is consulted, in the preview
//! and again at execution, so no spec lifts the structural floor. The spawned
//! child still runs on the parent's runtime, still hides mentra's `task`
//! intrinsic whatever profile it was given, and still sees `spawn` as its one
//! delegation door only if its roster offers it — a policy that narrows a
//! child's roster past `spawn` has built a leaf, which is a legitimate shape
//! (the same ruling as [`ToolRoster::only`]). And an override is a plain
//! replacement, not an enforced narrowing: mentra's template documents that a
//! wider profile hands the child more than its parent offers, so confining a
//! whole delegation chain is precisely what the policy function is for —
//! it is consulted again for the child's own children.

use std::{path::Path, sync::Arc};

use mentra::ModelInfo;
use serde_json::{Value, json};

use crate::workspace::ToolRoster;

/// What `spawn` knows about a delegation when it consults the child policy.
///
/// Accessors rather than public fields, so the context can grow a fact
/// without breaking every policy already written against it.
#[derive(Debug)]
pub struct ChildContext<'a> {
    prompt: &'a str,
    parent_agent_id: &'a str,
    workspace_dir: &'a Path,
}

impl<'a> ChildContext<'a> {
    pub(crate) fn new(prompt: &'a str, parent_agent_id: &'a str, workspace_dir: &'a Path) -> Self {
        Self {
            prompt,
            parent_agent_id,
            workspace_dir,
        }
    }

    /// The task the parent is delegating, exactly as the child will read it.
    pub fn prompt(&self) -> &str {
        self.prompt
    }

    /// The persisted agent id of the delegating parent.
    pub fn parent_agent_id(&self) -> &str {
        self.parent_agent_id
    }

    /// The delegating agent's working directory — its workspace root, unless
    /// a host moved it.
    pub fn workspace_dir(&self) -> &Path {
        self.workspace_dir
    }
}

/// Which of a child's three inherited facts a policy overrides.
///
/// [`inherit`](Self::inherit) — every field unset — is today's behavior
/// byte for byte: the child is spawned as a plain clone of its parent, on the
/// exact code path a runtime with no policy uses. Each `with_*` overrides one
/// fact and leaves the others inherited; see the module docs for what a spec
/// deliberately does not carry, and for the `Append` that is deliberately not
/// offered.
#[derive(Debug, Clone, Default)]
pub struct ChildSpec {
    pub(crate) roster: Option<ToolRoster>,
    pub(crate) model: Option<ModelInfo>,
    pub(crate) system: Option<String>,
}

impl ChildSpec {
    /// The no-override value: the child is its parent's clone, as it has
    /// always been.
    pub fn inherit() -> Self {
        Self::default()
    }

    /// Offers the child `roster` instead of the parent's own.
    ///
    /// The same mapping [`ToolRoster`] has always resolved to — mentra's
    /// `ToolProfile`, applied to the child's config — so
    /// [`ToolRoster::only`]'s caveats hold here unchanged: an allow-list that
    /// omits `spawn` builds a child with no delegation door, and nothing is
    /// un-registered from the runtime underneath. mentra's `task` intrinsic
    /// stays hidden from a subagent whatever this roster says.
    ///
    /// **A roster can only narrow what the parent is offered, never widen
    /// it past its workspace.** On a shared [`Runtime`](crate::Runtime) one
    /// tool registry serves every open workspace, and
    /// [`Workspace`](crate::Workspace) hides each *sibling's* bridged
    /// `mcp__*` and declared tools from its own model at mint. Replacing the
    /// child's profile would drop those hides along with the parent's roster,
    /// so basis puts them back: the names the parent is denied are added to
    /// the resulting profile's `hidden_tools` whichever constructor built it.
    /// A [`hide`](ToolRoster::hide) roster simply gains them; an
    /// [`only`](ToolRoster::only) roster that named one **loses that name,
    /// silently** — `ToolProfile::allows` checks the denylist after the
    /// allow-list — which is the same rule, stated the same way, that
    /// [`ToolRoster::only`] already documents for a workspace's own roster
    /// colliding with a sibling's tool. One composition, one outcome.
    ///
    /// This says nothing about *narrowing*: a policy may hand a child a
    /// wider roster than its parent's within the workspace's own tools, and
    /// mentra's template documents that it does not check either. What it
    /// cannot do is reach another repository's.
    #[must_use]
    pub fn with_roster(self, roster: ToolRoster) -> Self {
        Self {
            roster: Some(roster),
            ..self
        }
    }

    /// Runs the child on `model` instead of the parent's own.
    ///
    /// A [`ModelInfo`](crate::ModelInfo) rather than a
    /// [`ModelSelector`](crate::ModelSelector), because that is what maps
    /// onto mentra's template honestly: the template takes the model as
    /// given, resolves its provider by the name the `ModelInfo` carries when
    /// the child is spawned, and looks a missing context window up in that
    /// provider's listing — a selector would need a resolution pass of
    /// basis's own ahead of all that, inventing a second resolution path for
    /// one field. The named provider must be registered on this runtime;
    /// basis-built runtimes register exactly one, whose id
    /// [`Runtime::provider`](crate::Runtime::provider) reports. A model
    /// naming an unregistered provider fails the spawn with mentra's error
    /// naming it, which the model reads as the tool call's failure.
    #[must_use]
    pub fn with_model(self, model: ModelInfo) -> Self {
        Self {
            model: Some(model),
            ..self
        }
    }

    /// Replaces the child's system prompt wholesale with `system`.
    ///
    /// The whole of the child's own voice: the parent's rendered prompt —
    /// context files, memory index, host say — does not travel with it.
    /// mentra appends its standard subagent instructions after this text, as
    /// it does for every subagent. There is deliberately no append variant;
    /// the module docs say why.
    #[must_use]
    pub fn with_system(self, system: impl Into<String>) -> Self {
        Self {
            system: Some(system.into()),
            ..self
        }
    }

    /// Whether this spec changes nothing — the answer that keeps the
    /// delegation on the plain, template-free spawn path.
    pub(crate) fn is_inherit(&self) -> bool {
        self.roster.is_none() && self.model.is_none() && self.system.is_none()
    }

    /// Whether applying this spec replaces the child's cloned `ToolProfile`,
    /// which is what makes the parent's per-mint hides basis's to restore.
    pub(crate) fn overrides_roster(&self) -> bool {
        self.roster.is_some()
    }

    /// The `child` key of the approver's preview, or `None` when there is
    /// nothing to say.
    ///
    /// Present only when the policy overrode something, so a runtime with no
    /// policy — or one whose policy answered [`inherit`](Self::inherit) —
    /// presents the exact `{mode, body, cwd, target}` shape every remembered
    /// rule was written against. When present, it describes what the child
    /// *will be*, which is what a rule about delegation wants to match:
    /// `roster` as `{"offered": [..]}` for an allow-list or `{"hidden": [..]}`
    /// for a denylist, `model` as the overriding id, `system` as the word
    /// `"replaced"` — never the text, because a preview travels further than
    /// a glance and a system prompt can carry anything the host knows.
    pub(crate) fn preview_value(&self) -> Option<Value> {
        if self.is_inherit() {
            return None;
        }

        let mut child = serde_json::Map::new();
        if let Some(model) = &self.model {
            child.insert("model".to_string(), json!(model.id));
        }
        if let Some(roster) = &self.roster {
            let profile = roster.as_profile();
            let described = match &profile.allowed_tools {
                Some(offered) => json!({ "offered": offered }),
                None => json!({ "hidden": profile.hidden_tools }),
            };
            child.insert("roster".to_string(), described);
        }
        if self.system.is_some() {
            child.insert("system".to_string(), json!("replaced"));
        }

        Some(Value::Object(child))
    }
}

/// How [`SpawnTool`](super::SpawnTool) stores the policy: shared, because the
/// tool is cloned into mentra's registry behind an `Arc` and the preview and
/// execute paths both read it.
pub(crate) type ChildPolicy = Arc<dyn Fn(&ChildContext<'_>) -> ChildSpec + Send + Sync>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inherit_says_nothing_to_the_approver() {
        // The additive contract: with nothing overridden there is no `child`
        // key at all, so the preview is byte-identical to a policy-free
        // runtime's and every remembered rule keeps matching.
        assert_eq!(ChildSpec::inherit().preview_value(), None);
        assert!(ChildSpec::inherit().is_inherit());
    }

    #[test]
    fn every_override_is_described_and_the_system_text_is_not() {
        let spec = ChildSpec::inherit()
            .with_roster(ToolRoster::only(["read", "grep"]))
            .with_model(ModelInfo::new("cheap-model", "openai"))
            .with_system("secret internal triage instructions");

        let child = spec.preview_value().expect("overrides are described");
        assert_eq!(child["model"], "cheap-model");
        assert_eq!(child["roster"], json!({ "offered": ["grep", "read"] }));
        assert_eq!(
            child["system"], "replaced",
            "the fact travels; the text does not"
        );
        assert!(
            !child.to_string().contains("secret"),
            "a preview travels further than a glance: {child}"
        );
    }

    #[test]
    fn a_hide_roster_is_described_as_what_it_hides() {
        let spec = ChildSpec::inherit().with_roster(ToolRoster::hide(["write"]));

        let child = spec.preview_value().expect("an override is described");
        let hidden = child["roster"]["hidden"]
            .as_array()
            .expect("a denylist roster lists what it hides");
        assert!(hidden.iter().any(|name| name == "write"), "{child}");
    }
}
