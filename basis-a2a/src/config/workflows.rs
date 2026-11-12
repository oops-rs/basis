//! A served agent's workflows: the skills its card lists, each with the
//! tools and skills behind it.
//!
//! An agent that offers several distinct capabilities may be held to some of
//! them per caller. The decision is not this server's: something in front of
//! it — a gateway that signs a token for every request — says which
//! workflows a request may use, and the server's [`Authenticator`] hands
//! that set on with [`Principal::with_workflows`]. What this module holds is
//! the other half: what each workflow *is* on this agent, so a run can be
//! built with the tools and skills of the allowed ones and nothing else.
//!
//! One configuration is the source for three things that must agree: the
//! card's `skills[]`, the extension the card declares to promise the
//! enforcement, and the roster each run is given.
//!
//! [`Authenticator`]: crate::Authenticator
//! [`Principal::with_workflows`]: crate::Principal::with_workflows

use std::collections::BTreeSet;

use a2a::{AgentExtension, AgentSkill};

/// The tool a model loads a skill with. Offered to a run exactly when the
/// run is allowed at least one skill.
const LOAD_SKILL: &str = "load_skill";

/// One workflow: a skill on the card, and what the agent uses to do it.
#[derive(Debug, Clone, PartialEq)]
pub struct Workflow {
    pub(crate) skill: AgentSkill,
    pub(crate) tools: Vec<String>,
    pub(crate) skills: Vec<String>,
}

impl Workflow {
    /// A workflow listed on the card under `id`, with the name and
    /// description a caller choosing among them reads.
    ///
    /// The id is what a gateway grants and a token names, so it does not
    /// change once in use: a renamed id is a new workflow.
    pub fn new(
        id: impl Into<String>,
        name: impl Into<String>,
        description: impl Into<String>,
    ) -> Self {
        Self {
            skill: AgentSkill {
                id: id.into(),
                name: name.into(),
                description: description.into(),
                tags: Vec::new(),
                examples: None,
                input_modes: None,
                output_modes: None,
                security_requirements: None,
            },
            tools: Vec::new(),
            skills: Vec::new(),
        }
    }

    /// Adds a tag to the card's skill.
    #[must_use]
    pub fn with_tag(mut self, tag: impl Into<String>) -> Self {
        self.skill.tags.push(tag.into());
        self
    }

    /// Adds an example prompt to the card's skill.
    #[must_use]
    pub fn with_example(mut self, example: impl Into<String>) -> Self {
        self.skill
            .examples
            .get_or_insert_with(Vec::new)
            .push(example.into());
        self
    }

    /// The tools a run needs for this workflow, by the names the model
    /// calls them: a builtin such as `read`, a declared tool, or a bridged
    /// `mcp__<server>__<tool>`. Names are exact.
    ///
    /// A run is offered the tools of every workflow its caller is allowed
    /// and no others, so a tool that would let a model do another
    /// workflow's work by a different route — `spawn` on an agent granted a
    /// shell, say — belongs only to the workflows that may do that work.
    #[must_use]
    pub fn with_tools(mut self, tools: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.tools.extend(tools.into_iter().map(Into::into));
        self
    }

    /// The workspace skills a run may list and load for this workflow, by
    /// name. `load_skill` is offered with them; it need not be named as a
    /// tool.
    #[must_use]
    pub fn with_skills(mut self, skills: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.skills.extend(skills.into_iter().map(Into::into));
        self
    }

    /// The workflow's id: its skill's, on the card.
    pub fn id(&self) -> &str {
        &self.skill.id
    }
}

/// Every workflow a served agent offers, and the extension its card declares
/// to promise it runs only the ones a request is allowed
/// ([`ServeConfig::with_workflows`](crate::ServeConfig::with_workflows)).
///
/// With this set the server:
///
/// - lists each workflow as a card skill, in the order given, and declares
///   the extension, optional;
/// - refuses a request whose [`Principal`](crate::Principal) carries no
///   workflows, and one that is allowed none of these;
/// - builds each `basis-tasks` run with only the allowed workflows' tools and
///   skills, plus the [shared](Self::with_shared_tools) ones — on a
///   conversation's first message and on every later one, whatever its
///   earlier runs were given;
/// - records which workflows each task was built with, and stops the task
///   and reports it failed, with the reason, once a `GetTask` or a reply
///   arrives whose principal is not allowed one of them. A cancel is never
///   judged, and a narrower set on any other request withdraws nothing.
///
/// For a server whose tasks a host runs, the tools and skills mean nothing
/// and are refused; the host is handed the allowed set in
/// [`HostedStart::workflows`](crate::HostedStart::workflows) and does its
/// own dispatch inside it.
#[derive(Debug, Clone, PartialEq)]
pub struct Workflows {
    pub(crate) extension: AgentExtension,
    pub(crate) workflows: Vec<Workflow>,
    pub(crate) shared_tools: Vec<String>,
    pub(crate) shared_skills: Vec<String>,
    pub(crate) contact: Option<String>,
}

impl Workflows {
    /// No workflows yet, declaring the extension `uri` on the card: the
    /// promise, in whatever protocol decides the allowed set, that this
    /// agent runs only what a request is allowed.
    pub fn declaring(uri: impl Into<String>) -> Self {
        Self {
            extension: AgentExtension {
                uri: uri.into(),
                description: Some(
                    "Each skill on this card is a workflow. A request may use only the \
                     workflows its gateway allows its caller, and this agent runs nothing \
                     outside them."
                        .to_string(),
                ),
                required: Some(false),
                params: None,
            },
            workflows: Vec::new(),
            shared_tools: Vec::new(),
            shared_skills: Vec::new(),
            contact: None,
        }
    }

    /// Adds a workflow. The card lists them in the order they are added.
    #[must_use]
    pub fn with_workflow(mut self, workflow: Workflow) -> Self {
        self.workflows.push(workflow);
        self
    }

    /// Tools every workflow uses: offered to a run whichever workflows it
    /// is allowed. The same as naming them on each workflow.
    #[must_use]
    pub fn with_shared_tools(mut self, tools: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.shared_tools.extend(tools.into_iter().map(Into::into));
        self
    }

    /// Workspace skills every workflow uses, as
    /// [`with_shared_tools`](Self::with_shared_tools) is for tools.
    #[must_use]
    pub fn with_shared_skills(
        mut self,
        skills: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        self.shared_skills
            .extend(skills.into_iter().map(Into::into));
        self
    }

    /// Whom a caller closed out of a workflow is told to ask: the agent's
    /// owner, in words a person can act on ("the iOS platform team, in
    /// #ios-platform"). Without one, a refusal says "this agent's owner".
    #[must_use]
    pub fn with_contact(self, contact: impl Into<String>) -> Self {
        Self {
            contact: Some(contact.into()),
            ..self
        }
    }

    /// The URI of the extension the card declares.
    pub fn extension_uri(&self) -> &str {
        &self.extension.uri
    }

    /// Every workflow's id, in card order.
    pub fn ids(&self) -> impl Iterator<Item = &str> {
        self.workflows.iter().map(Workflow::id)
    }

    /// The card's skills: one per workflow, in the order added.
    pub(crate) fn card_skills(&self) -> Vec<AgentSkill> {
        self.workflows
            .iter()
            .map(|workflow| workflow.skill.clone())
            .collect()
    }

    /// Whom a refusal tells the caller to ask.
    pub(crate) fn contact(&self) -> &str {
        self.contact.as_deref().unwrap_or("this agent's owner")
    }

    /// The workflows of `granted` this agent offers, in card order. Ids it
    /// does not offer are left out: a token may still name a skill the card
    /// listed a minute ago.
    pub(crate) fn offered(&self, granted: &[String]) -> Vec<String> {
        self.ids()
            .filter(|id| granted.iter().any(|granted| granted == id))
            .map(str::to_string)
            .collect()
    }

    /// The workflows that are not in `allowed`, in card order.
    pub(crate) fn closed(&self, allowed: &[String]) -> Vec<&Workflow> {
        self.workflows
            .iter()
            .filter(|workflow| !allowed.iter().any(|id| id == workflow.id()))
            .collect()
    }

    /// The tools and skills a run allowed `allowed` is built with: the
    /// shared ones and those of each allowed workflow, and `load_skill`
    /// exactly when there is a skill to load.
    pub(crate) fn roster(&self, allowed: &[String]) -> (BTreeSet<String>, BTreeSet<String>) {
        let open = self
            .workflows
            .iter()
            .filter(|workflow| allowed.iter().any(|id| id == workflow.id()));
        let mut tools: BTreeSet<String> = self.shared_tools.iter().cloned().collect();
        let mut skills: BTreeSet<String> = self.shared_skills.iter().cloned().collect();
        for workflow in open {
            tools.extend(workflow.tools.iter().cloned());
            skills.extend(workflow.skills.iter().cloned());
        }
        // Decided here rather than left to what a workflow named: the skill
        // roster already bounds what it can load, and a run with no skill
        // has nothing for it to return.
        if skills.is_empty() {
            tools.remove(LOAD_SKILL);
        } else {
            tools.insert(LOAD_SKILL.to_string());
        }
        (tools, skills)
    }

    /// What a run's model is told when some workflows are closed to its
    /// caller, so it says so rather than answer a request for one some
    /// other way. Not the enforcement — the roster is — and `None` when
    /// every workflow is open.
    pub(crate) fn note(&self, allowed: &[String]) -> Option<String> {
        let closed = self.closed(allowed);
        if closed.is_empty() {
            return None;
        }
        let named = |workflow: &Workflow| format!("`{}` ({})", workflow.id(), workflow.skill.name);
        let open: Vec<String> = self
            .workflows
            .iter()
            .filter(|workflow| allowed.iter().any(|id| id == workflow.id()))
            .map(named)
            .collect();
        let closed: Vec<String> = closed.into_iter().map(named).collect();
        Some(format!(
            "[From this agent's host, not from the caller. This request may use only these \
             workflows: {}. These are closed to this caller: {}. If the request needs a closed \
             workflow, do not attempt it another way: say which workflow is closed to them, by \
             its id, and that {} can open it.]",
            open.join(", "),
            closed.join(", "),
            self.contact()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workflows() -> Workflows {
        Workflows::declaring("urn:example:workflows:v1")
            .with_shared_tools(["read"])
            .with_workflow(
                Workflow::new("build", "Build", "Runs builds.")
                    .with_tools(["jenkins_build"])
                    .with_skills(["release"]),
            )
            .with_workflow(Workflow::new("ask", "Ask", "Answers questions.").with_tools(["grep"]))
            .with_contact("the platform team")
    }

    fn ids(ids: &[&str]) -> Vec<String> {
        ids.iter().map(|id| (*id).to_string()).collect()
    }

    #[test]
    fn a_run_gets_the_shared_tools_and_those_of_its_workflows_only() {
        let (tools, skills) = workflows().roster(&ids(&["ask"]));
        assert_eq!(tools, BTreeSet::from(["grep".into(), "read".into()]));
        assert!(skills.is_empty());

        let (tools, skills) = workflows().roster(&ids(&["build", "ask"]));
        assert_eq!(
            tools,
            BTreeSet::from([
                "grep".into(),
                "jenkins_build".into(),
                "load_skill".into(),
                "read".into()
            ])
        );
        assert_eq!(skills, BTreeSet::from(["release".into()]));
    }

    #[test]
    fn load_skill_follows_the_skills_whatever_a_workflow_named() {
        let named = Workflows::declaring("urn:example:workflows:v1")
            .with_workflow(Workflow::new("ask", "Ask", "Answers.").with_tools(["load_skill"]));
        let (tools, _) = named.roster(&ids(&["ask"]));
        assert!(tools.is_empty(), "no skill to load, so no tool to load it");
    }

    #[test]
    fn a_granted_set_is_read_in_card_order_and_only_for_what_is_offered() {
        assert_eq!(
            workflows().offered(&ids(&["ask", "retired", "build"])),
            ids(&["build", "ask"])
        );
        assert!(workflows().offered(&ids(&["retired"])).is_empty());
    }

    #[test]
    fn the_note_names_what_is_closed_and_whom_to_ask_and_is_absent_when_nothing_is() {
        let note = workflows().note(&ids(&["ask"])).expect("build is closed");
        assert!(note.contains("`ask` (Ask)"), "{note}");
        assert!(
            note.contains("closed to this caller: `build` (Build)"),
            "{note}"
        );
        assert!(note.contains("the platform team"), "{note}");
        assert_eq!(workflows().note(&ids(&["build", "ask"])), None);
    }

    #[test]
    fn the_card_lists_one_skill_per_workflow_in_order() {
        let skills = workflows().card_skills();
        assert_eq!(
            skills
                .iter()
                .map(|skill| skill.id.as_str())
                .collect::<Vec<_>>(),
            ["build", "ask"]
        );
        assert_eq!(skills[0].description, "Runs builds.");
    }
}
