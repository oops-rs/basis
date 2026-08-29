//! The commands a client offers: basis's own, and the workspace's templates.
//!
//! ACP already has the slot: a client that receives `AvailableCommandsUpdate`
//! offers the names in its own UI, and sends back whatever the person typed
//! after one — as text, on the next `session/prompt`, since ACP has no
//! separate "invoke a command" method. So this module is both halves of one
//! convention: which names are advertised, and which of them basis answers
//! itself rather than sending on to the model.
//!
//! Nothing here decides *when* to send the update — the ACP server owns the
//! session lifecycle — and nothing here runs anything;
//! [`turn`](crate::server::turn) is where a recognized built-in is acted on,
//! and where a template's name is [expanded](expand) into the prompt it
//! stands for before the model sees it.
//!
//! # Built-ins, and what happens when a workspace picks the same name
//!
//! basis answers exactly one command itself today, `/compact`, and it is
//! always offered: it acts on the session rather than on the workspace, so
//! there is no repository in which it does not apply.
//!
//! **A built-in wins, and the template of that name is not advertised.** The
//! rule has to point one way or the other — two commands with one name in the
//! list is a coin flip the client makes — and this is the direction whose loss
//! is recoverable. A template author whose `compact.md` is shadowed can rename
//! the file; a person whose only way to compact a conversation over ACP has
//! been silently replaced by somebody else's prompt cannot do anything at all.
//! `docs/conventions.md` states it where the template convention is stated.

use agent_client_protocol::schema::v1::{
    AvailableCommand, AvailableCommandInput, UnstructuredCommandInput,
};

use basis::{PromptPart, Template, templates::invocation};

/// The name of the one command basis answers itself.
pub(crate) const COMPACT: &str = "compact";

/// What a prompt is asking basis to do rather than asking the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Builtin<'a> {
    /// Summarize the conversation so far. The argument, when there is one,
    /// says what to keep and is added to mentra's standing requirements.
    Compact { instructions: Option<&'a str> },
}

/// The built-in a prompt opens with, if it opens with one.
///
/// Reads the `/name args` convention through
/// [`basis::templates::invocation`], which is the same parser the shell uses,
/// so a name typed in an editor and a name typed at a terminal are one rule.
///
/// A `/name` that is not a built-in returns `None`. A template's name is
/// answered next door, by [`expand`], once the turn lock — which is where the
/// templates live — is held.
pub(crate) fn builtin(prompt: &str) -> Option<Builtin<'_>> {
    match invocation(prompt)? {
        (COMPACT, arguments) => Some(Builtin::Compact {
            instructions: (!arguments.is_empty()).then_some(arguments),
        }),
        _ => None,
    }
}

/// The prompt a `/name …` line stands for, when `name` is one of `templates`.
///
/// The other half of [`available_commands`]: a client that offers `/review`
/// sends back `/review the diff` as text, and the model must read the body
/// the author wrote with the argument substituted — the same rewrite the shell
/// performs on `basis spawn /review …`, through the same
/// [`Template::render`], so the two surfaces cannot disagree about what a
/// template says.
///
/// Only the prompt's opening text is read, and only when it is text: a
/// `/review` typed after a screenshot is prose about reviewing. A name that
/// matches nothing goes to the model as typed, where the shell refuses it. The
/// shell has stdin as the escape for a prompt that begins with a literal `/`;
/// an editor has nothing but this request, and the names were the client's to
/// offer in the first place.
pub(crate) fn expand(parts: Vec<PromptPart>, templates: &[Template]) -> Vec<PromptPart> {
    let rendered = match parts.first() {
        Some(PromptPart::Text(text)) => invocation(text).and_then(|(name, arguments)| {
            templates
                .iter()
                .find(|template| template.name == name)
                .map(|template| template.render(arguments))
        }),
        _ => None,
    };

    match rendered {
        Some(prompt) => std::iter::once(PromptPart::text(prompt))
            .chain(parts.into_iter().skip(1))
            .collect(),
        None => parts,
    }
}

/// Every command this session offers: basis's own first, then the workspace's
/// templates in the order discovery gave them, which is by name.
///
/// The `input` field is set only for a template that declared an
/// `argument-hint`. ACP describes the hint as text "to display when the input
/// hasn't been provided yet", which is the author's words shown verbatim;
/// leaving it unset is how basis says the author declared none, rather than
/// inventing a placeholder they never wrote.
///
/// Every template accepts arguments regardless — see
/// [`Template::render`](basis::Template::render). The hint governs what a
/// client displays, not what basis will substitute.
pub fn available_commands(templates: &[Template]) -> Vec<AvailableCommand> {
    builtins()
        .into_iter()
        .chain(
            templates
                .iter()
                .filter(|template| template.name != COMPACT)
                .map(available_command),
        )
        .collect()
}

fn builtins() -> Vec<AvailableCommand> {
    vec![
        AvailableCommand::new(
            COMPACT,
            "Summarize the conversation so far and continue from the summary",
        )
        .input(AvailableCommandInput::Unstructured(
            UnstructuredCommandInput::new("what to keep (optional)"),
        )),
    ]
}

fn available_command(template: &Template) -> AvailableCommand {
    AvailableCommand::new(template.name.as_str(), template.description.as_str()).input(
        template
            .argument_hint
            .as_deref()
            .map(|hint| AvailableCommandInput::Unstructured(UnstructuredCommandInput::new(hint))),
    )
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use basis::ContextScope;

    fn template(name: &str, hint: Option<&str>) -> Template {
        Template {
            name: name.to_string(),
            description: format!("does {name}"),
            argument_hint: hint.map(str::to_string),
            body: "body".to_string(),
            path: PathBuf::from(format!("/templates/{name}.md")),
            scope: ContextScope::Workspace,
        }
    }

    /// The templates a call to `available_commands` produced, without the
    /// built-ins every list starts with.
    fn from_templates(templates: &[Template]) -> Vec<AvailableCommand> {
        let commands = available_commands(templates);
        assert_eq!(
            commands[0].name, COMPACT,
            "basis's own commands come first: {commands:?}"
        );
        commands[1..].to_vec()
    }

    #[test]
    fn a_template_becomes_a_command_with_its_name_and_description() {
        let commands = from_templates(&[template("review", None)]);

        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].name, "review");
        assert_eq!(commands[0].description, "does review");
    }

    #[test]
    fn a_template_without_a_hint_declares_no_input() {
        let commands = from_templates(&[template("review", None)]);

        assert_eq!(commands[0].input, None);
    }

    #[test]
    fn a_hint_becomes_unstructured_input() {
        let commands = from_templates(&[template("review", Some("<path>"))]);

        let Some(AvailableCommandInput::Unstructured(input)) = &commands[0].input else {
            panic!("expected unstructured input");
        };
        assert_eq!(input.hint, "<path>");
    }

    #[test]
    fn a_workspace_with_no_templates_still_offers_what_basis_answers_itself() {
        // `/compact` acts on the session, not on the workspace, so there is no
        // repository in which it does not apply.
        let commands = available_commands(&[]);

        let names: Vec<&str> = commands.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec![COMPACT]);
    }

    #[test]
    fn order_is_preserved() {
        let commands = from_templates(&[
            template("alpha", None),
            template("beta", None),
            template("gamma", None),
        ]);

        let names: Vec<&str> = commands.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "beta", "gamma"]);
    }

    #[test]
    fn a_template_invocation_is_rendered_with_its_arguments() {
        let templates = [Template {
            body: "Review $ARGUMENTS carefully.".to_string(),
            ..template("review", None)
        }];

        assert_eq!(
            expand(vec![PromptPart::text("/review the diff")], &templates),
            vec![PromptPart::text("Review the diff carefully.")],
            "the same rewrite the shell performs, through the same render"
        );
    }

    #[test]
    fn only_the_opening_text_is_read_and_the_rest_is_kept() {
        let templates = [Template {
            body: "Review $ARGUMENTS.".to_string(),
            ..template("review", None)
        }];
        let image = PromptPart::image("image/png", vec![1, 2, 3]);

        // A screenshot first: the `/review` after it is prose about reviewing.
        assert_eq!(
            expand(
                vec![image.clone(), PromptPart::text("/review this")],
                &templates
            ),
            vec![image.clone(), PromptPart::text("/review this")]
        );
        // Text first: rendered, and what followed it still follows.
        assert_eq!(
            expand(
                vec![PromptPart::text("/review this"), image.clone()],
                &templates
            ),
            vec![PromptPart::text("Review this."), image]
        );
    }

    #[test]
    fn a_name_that_matches_no_template_goes_to_the_model_as_typed() {
        // The shell refuses here and offers stdin as the escape; an editor has
        // no stdin, and the client offered the names to begin with.
        assert_eq!(
            expand(vec![PromptPart::text("/etc/hosts is what I mean")], &[]),
            vec![PromptPart::text("/etc/hosts is what I mean")]
        );
    }

    #[test]
    fn a_namespaced_name_reaches_the_wire_intact() {
        let commands = from_templates(&[template("git:commit", None)]);

        assert_eq!(commands[0].name, "git:commit");
    }

    #[test]
    fn a_template_that_took_a_builtins_name_is_not_offered_alongside_it() {
        // Two commands with one name is a coin flip the client makes. The
        // built-in wins because that loss is the recoverable one: the template
        // author can rename the file.
        let commands = available_commands(&[template("compact", None), template("review", None)]);

        let names: Vec<&str> = commands.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec![COMPACT, "review"]);
        assert_ne!(
            commands[0].description, "does compact",
            "the surviving `compact` must be basis's, not the template's"
        );
    }

    #[test]
    fn a_slash_compact_is_recognized_with_and_without_an_instruction() {
        assert_eq!(
            builtin("/compact"),
            Some(Builtin::Compact { instructions: None })
        );
        assert_eq!(
            builtin("/compact keep the migration plan"),
            Some(Builtin::Compact {
                instructions: Some("keep the migration plan")
            })
        );
        assert_eq!(
            builtin("/compact   "),
            Some(Builtin::Compact { instructions: None }),
            "trailing whitespace is not an instruction"
        );
    }

    #[test]
    fn anything_else_is_a_prompt_for_the_model() {
        // Including a template's own name: that is `expand`'s to answer, once
        // the templates are in hand.
        assert_eq!(builtin("/review the diff"), None);
        assert_eq!(builtin("compact the log output"), None);
        assert_eq!(
            builtin("/compaction is what I want"),
            None,
            "the name is a whole token, not a prefix"
        );
    }
}
