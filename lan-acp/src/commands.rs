//! Templates as ACP commands.
//!
//! ACP already has the slot: a client that receives `AvailableCommandsUpdate`
//! offers the names in its own UI, and sends back whatever the person typed
//! after one. Nothing here decides *when* to send that update — the ACP server
//! owns the session lifecycle — so this module is only the mapping, kept beside
//! the templates it maps rather than inside the protocol layer, for the same
//! reason `Event` exists: one shape, translated at each edge.

use agent_client_protocol::schema::v1::{
    AvailableCommand, AvailableCommandInput, UnstructuredCommandInput,
};

use lan_core::Template;

/// Maps discovered templates to the commands an ACP client can offer.
///
/// Order is preserved, so templates that arrived name-ordered stay that way.
///
/// The `input` field is set only for a template that declared an
/// `argument-hint`. ACP describes the hint as text "to display when the input
/// hasn't been provided yet", which is the author's words shown verbatim;
/// leaving it unset is how lan says the author declared none, rather than
/// inventing a placeholder they never wrote.
///
/// Every template accepts arguments regardless — see
/// [`Template::render`](lan_core::Template::render). The hint governs what a
/// client displays, not what lan will substitute.
pub fn available_commands(templates: &[Template]) -> Vec<AvailableCommand> {
    templates.iter().map(available_command).collect()
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
    use lan_core::ContextScope;

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

    #[test]
    fn a_template_becomes_a_command_with_its_name_and_description() {
        let commands = available_commands(&[template("review", None)]);

        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].name, "review");
        assert_eq!(commands[0].description, "does review");
    }

    #[test]
    fn a_template_without_a_hint_declares_no_input() {
        let commands = available_commands(&[template("review", None)]);

        assert_eq!(commands[0].input, None);
    }

    #[test]
    fn a_hint_becomes_unstructured_input() {
        let commands = available_commands(&[template("review", Some("<path>"))]);

        let Some(AvailableCommandInput::Unstructured(input)) = &commands[0].input else {
            panic!("expected unstructured input");
        };
        assert_eq!(input.hint, "<path>");
    }

    #[test]
    fn nothing_discovered_means_no_commands() {
        assert!(available_commands(&[]).is_empty());
    }

    #[test]
    fn order_is_preserved() {
        let commands = available_commands(&[
            template("alpha", None),
            template("beta", None),
            template("gamma", None),
        ]);

        let names: Vec<&str> = commands.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "beta", "gamma"]);
    }

    #[test]
    fn a_namespaced_name_reaches_the_wire_intact() {
        let commands = available_commands(&[template("git:commit", None)]);

        assert_eq!(commands[0].name, "git:commit");
    }
}
