//! `/name` at the front of a prompt is a template invocation.
//!
//! basis has discovered `.basis/templates/**/*.md` since P2 and surfaced them
//! in exactly one place: over ACP, as the `AvailableCommand`s a client offers
//! (`basis-acp::available_commands`). The person who wrote
//! `.basis/templates/git/commit.md` could not type it at a shell — the
//! convention every peer CLI spells `/command` reached the editor and stopped
//! there.
//!
//! It is a rewrite, like [`shorthand`](crate::shorthand), and it sits beside
//! it for the same reason: what `spawn` receives is a prompt either way, and
//! the rendered text is what the task records, so `basis watch` and
//! `basis list` show what was actually asked rather than the shorthand for it.
//!
//! # The rule
//!
//! [`basis::templates::invocation`] reads the first token and nothing else,
//! and it lives there rather than here because `basis-acp` reads the same
//! convention for its own built-ins — a rule two crates spelled separately is
//! two rules.
//!
//! What this file adds is the shell's answer to a name that fits the shape and
//! matches nothing: refuse, rather than send. A typo'd `/comit` handed to the
//! model as prose is a run that answers the wrong question and bills for it;
//! and the escape is one character (`basis spawn -` reads a literal prompt
//! from stdin), which is cheaper than guessing.

use std::path::Path;

use basis::{Template, TemplatesConfig, templates::invocation};

use crate::{cli::RunArgs, local::ClientError};

/// The run this invocation asked for, with any `/name` resolved to the prompt
/// it stands for.
///
/// `-` is left alone: it means the prompt arrives on stdin, and stdin is the
/// documented way to send a prompt that begins with a literal `/`.
pub(crate) fn expand(args: RunArgs) -> Result<RunArgs, ClientError> {
    if args.prompt == "-" {
        return Ok(args);
    }
    let workspace = workspace_of(args.workspace.as_deref())?;
    match resolve(&args.prompt, &workspace)? {
        Some(prompt) => Ok(RunArgs { prompt, ..args }),
        None => Ok(args),
    }
}

/// The prompt a `/name …` line stands for, or `None` when the line is prose.
///
/// Discovery is [`basis::templates::load`], the same function the workspace
/// builder hands ACP its command list from, so a name a client offers and a
/// name a shell accepts are the same set — including the layering that lets a
/// workspace template shadow a personal one.
pub(crate) fn resolve(prompt: &str, workspace: &Path) -> Result<Option<String>, ClientError> {
    let Some((name, arguments)) = invocation(prompt) else {
        return Ok(None);
    };
    let templates = basis::templates::load(workspace, &TemplatesConfig::default())
        .map_err(|error| ClientError::new(format!("load templates: {error}")))?;

    match templates.iter().find(|template| template.name == name) {
        Some(template) => Ok(Some(template.render(arguments))),
        None => Err(unknown(name, &templates)),
    }
}

/// A name that fits the shape and names nothing.
///
/// The available names are on the error because a typo is the usual cause and
/// the fix is usually visible in the list; the stdin escape is on it because
/// the other cause is a prompt that genuinely begins with `/`, and that person
/// needs a way through, not a list.
fn unknown(name: &str, templates: &[Template]) -> ClientError {
    let available = if templates.is_empty() {
        "this workspace defines none — a `.basis/templates/<name>.md` file declares one".to_string()
    } else {
        let names: Vec<&str> = templates
            .iter()
            .map(|template| template.name.as_str())
            .collect();
        format!("available: {}", names.join(", "))
    };
    ClientError::usage(format!(
        "no template named `{name}` — {available}. A prompt that begins with a literal `/` goes in on stdin"
    ))
    .pointing_at("basis spawn -")
}

fn workspace_of(requested: Option<&Path>) -> Result<std::path::PathBuf, ClientError> {
    match requested {
        Some(path) => Ok(path.to_path_buf()),
        None => std::env::current_dir()
            .map_err(|error| ClientError::new(format!("no working directory: {error}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::PathBuf;

    /// A workspace with the templates a test needs, written where discovery
    /// looks: `.basis/templates`, namespaced by directory.
    fn workspace(templates: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        for (path, body) in templates {
            let file = dir.path().join(".basis/templates").join(path);
            std::fs::create_dir_all(file.parent().expect("parent")).expect("create dir");
            std::fs::write(file, body).expect("write template");
        }
        dir
    }

    const FIX: &str = "---\ndescription: fix something\n---\nFix $1 in $2.";
    const COMMIT: &str = "---\ndescription: write a commit\n---\nCommit: $ARGUMENTS";

    #[test]
    fn a_slash_name_renders_the_template_it_names_with_the_rest_of_the_line() {
        let dir = workspace(&[("fix.md", FIX)]);

        let prompt = resolve("/fix auth login.rs", dir.path())
            .expect("a known template")
            .expect("a template invocation");

        assert_eq!(prompt, "Fix auth in login.rs.");
    }

    /// Nesting is namespacing: `.basis/templates/git/commit.md` is
    /// `git:commit`, the same name ACP puts on the wire.
    #[test]
    fn a_namespaced_template_is_reached_by_its_namespaced_name() {
        let dir = workspace(&[("git/commit.md", COMMIT)]);

        let prompt = resolve("/git:commit the parser fix", dir.path())
            .expect("a known template")
            .expect("a template invocation");

        assert_eq!(prompt, "Commit: the parser fix");
    }

    #[test]
    fn a_template_invoked_with_nothing_still_renders() {
        let dir = workspace(&[("commit.md", COMMIT)]);

        assert_eq!(
            resolve("/commit", dir.path()).expect("known").expect("one"),
            "Commit: "
        );
    }

    /// The refusal that keeps a typo from becoming a paid-for answer to the
    /// wrong question — and the one that has to hand back a way through.
    #[test]
    fn an_unknown_name_is_refused_with_the_names_that_exist() {
        let dir = workspace(&[("fix.md", FIX), ("git/commit.md", COMMIT)]);

        let error = resolve("/comit the parser fix", dir.path()).expect_err("refused");
        let rendered = format!("{error:?}");

        assert!(rendered.contains("no template named `comit`"), "{rendered}");
        assert!(rendered.contains("fix"), "the list is on it: {rendered}");
        assert!(rendered.contains("git:commit"), "{rendered}");
        assert!(rendered.contains("stdin"), "and the escape: {rendered}");
    }

    #[test]
    fn a_workspace_with_no_templates_says_so_rather_than_listing_nothing() {
        let dir = workspace(&[]);

        let error = resolve("/fix it", dir.path()).expect_err("refused");

        assert!(
            format!("{error:?}").contains(".basis/templates"),
            "{error:?}"
        );
    }

    /// A template name never contains `/`, so a first token that does is a
    /// path. This is the whole reason the rule can be applied to every prompt
    /// without an escape for the common case.
    #[test]
    fn a_path_is_a_path_and_passes_straight_through() {
        let dir = workspace(&[("fix.md", FIX)]);

        for prose in [
            "/usr/bin/x crashes on startup",
            "/ is the root directory",
            "//comment syntax",
            "look at /etc/hosts",
        ] {
            assert_eq!(
                resolve(prose, dir.path()).expect("prose is never refused"),
                None,
                "{prose} is prose"
            );
        }
    }

    /// `-` means the prompt is on stdin, and stdin is the documented way to
    /// send one that begins with a literal `/`. Expanding it would take the
    /// escape away from the only people who need it.
    #[test]
    fn a_stdin_prompt_is_never_read_as_a_template() {
        let dir = workspace(&[("fix.md", FIX)]);
        let args = run_args("-", dir.path());

        assert_eq!(expand(args).expect("untouched").prompt, "-");
    }

    fn run_args(prompt: &str, workspace: &Path) -> RunArgs {
        use clap::Parser;

        let Some(crate::cli::Command::Spawn(args)) = crate::cli::Cli::try_parse_from([
            "basis",
            "spawn",
            prompt,
            "-C",
            &workspace.to_string_lossy(),
        ])
        .expect("parses")
        .command
        else {
            panic!("spawn parses");
        };
        assert_eq!(args.workspace, Some(PathBuf::from(workspace)));
        args
    }
}
