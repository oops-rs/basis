//! Rewriting the command line before clap sees it.
//!
//! ADR-0017's shorthand — `lan "fix the failing test"` is exactly
//! `lan spawn "fix the failing test"` — is a statement about one token, and clap
//! has no way to express "this positional is a subcommand unless it isn't". So
//! it happens out here on the raw argv, and [`cli`](crate::cli) then parses
//! whatever comes out, none the wiser.
//!
//! The two word lists are the whole rule, which is why they live beside the
//! function that consults them rather than beside the grammar they describe.
//! They have to be changed together: add a subcommand to
//! [`Command`](crate::cli::Command) without adding its word to
//! [`SUBCOMMANDS`], and the new subcommand silently becomes a prompt.

use std::ffi::{OsStr, OsString};

/// The lifecycle commands, adapters, and clap's own help command. Anything
/// else is a prompt.
const SUBCOMMANDS: [&str; 11] = [
    "spawn",
    "run",
    "send",
    "wait",
    "cancel",
    "watch",
    "inbox",
    "fingerprint",
    "serve",
    "__daemon",
    "help",
];

/// Flags the top level answers itself. Every other flag belongs to `run`,
/// which is what lets `lan --json "hi"` mean what it looks like.
const TOP_LEVEL_FLAGS: [&str; 4] = ["-h", "--help", "-V", "--version"];

/// Rewrites `lan "<prompt>"` as `lan spawn "<prompt>"`.
///
/// Deciding on the first argument alone is what makes the rule total: a flag's
/// *value* can look like anything (`lan --model gpt-5 "hi"`), and scanning
/// further would have to know every flag's arity to avoid mistaking `gpt-5`
/// for the prompt.
pub(crate) fn normalize(argv: impl IntoIterator<Item = OsString>) -> Vec<OsString> {
    let mut argv: Vec<OsString> = argv.into_iter().collect();

    // Bare `lan` is left alone so main can return usage without inventing a
    // long-lived server mode.
    let Some(first) = argv.get(1) else {
        return argv;
    };

    if starts_a_run(first) {
        argv.insert(1, OsString::from("spawn"));
    }

    argv
}

/// Whether the first argument opens a run, rather than naming a subcommand or
/// asking the top level for help.
///
/// `--` lands here as a run too, which is what makes it the escape: `lan -- run`
/// becomes `lan spawn -- run`, and the word arrives as a prompt.
fn starts_a_run(first: &OsStr) -> bool {
    match first.to_str() {
        // Not UTF-8, so it is neither a reserved word nor a flag lan defines.
        // `spawn` takes it as a prompt and clap reports the encoding.
        None => true,
        Some(word) => !SUBCOMMANDS.contains(&word) && !TOP_LEVEL_FLAGS.contains(&word),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::PathBuf;

    use clap::Parser;

    use crate::{
        cli::{Cli, Command, RunArgs},
        exit::EXIT_USAGE,
    };

    /// The command line as lan sees it after the shorthand is resolved.
    fn normalized(argv: &[&str]) -> Vec<String> {
        normalize(argv.iter().map(OsString::from))
            .into_iter()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect()
    }

    /// The `spawn` the shorthand produced, so a test can check what it carries.
    fn shorthand(argv: &[&str]) -> RunArgs {
        let parsed = Cli::try_parse_from(normalize(argv.iter().map(OsString::from)))
            .expect("the shorthand should parse");

        let Some(Command::Spawn(args)) = parsed.command else {
            panic!("a prompt should have become a spawn: {argv:?}");
        };
        args
    }

    #[test]
    fn a_positional_that_is_not_a_subcommand_is_a_prompt() {
        assert_eq!(
            shorthand(&["lan", "fix the failing test"]).prompt,
            "fix the failing test"
        );
    }

    #[test]
    fn the_shorthand_is_exactly_the_spawn_subcommand() {
        assert_eq!(
            normalized(&["lan", "fix the failing test"]),
            ["lan", "spawn", "fix the failing test"]
        );
    }

    #[test]
    fn flags_pass_through_the_shorthand() {
        // The interesting half is `--model gpt-5`: a flag's value looks like a
        // positional, so anything that scanned past the first argument would
        // take "gpt-5" for the prompt.
        let args = shorthand(&["lan", "--json", "--model", "gpt-5", "hi"]);

        assert!(args.json);
        assert_eq!(args.model.as_deref(), Some("gpt-5"));
        assert_eq!(args.prompt, "hi");
    }

    #[test]
    fn a_prompt_that_starts_with_a_dash_is_still_a_prompt() {
        assert_eq!(shorthand(&["lan", "-"]).prompt, "-");
        assert_eq!(
            shorthand(&["lan", "-C", "/repo", "hi"]).workspace,
            Some(PathBuf::from("/repo"))
        );
    }

    #[test]
    fn a_prompt_that_names_a_subcommand_needs_the_escape() {
        // Without `--`, `lan spawn` is the subcommand with its prompt missing —
        // clap says so rather than lan guessing which was meant.
        let ambiguous = Cli::try_parse_from(normalize(["lan", "spawn"].iter().map(OsString::from)))
            .expect_err("a bare subcommand name is not a prompt");
        assert_eq!(ambiguous.exit_code(), i32::from(EXIT_USAGE));

        assert_eq!(shorthand(&["lan", "--", "run"]).prompt, "run");
        assert_eq!(shorthand(&["lan", "--", "serve"]).prompt, "serve");
    }

    #[test]
    fn lifecycle_verbs_are_never_rewritten_as_prompts() {
        for verb in ["send", "wait", "cancel", "watch", "inbox"] {
            assert_eq!(
                normalized(&["lan", verb, "task"])[1],
                verb,
                "{verb} must reach clap as a subcommand"
            );
        }
    }

    #[test]
    fn a_prompt_that_merely_begins_with_a_subcommand_name_needs_nothing() {
        assert_eq!(
            shorthand(&["lan", "run the tests and summarize"]).prompt,
            "run the tests and summarize"
        );
    }

    #[test]
    fn bare_lan_is_left_alone_for_usage_to_handle() {
        assert_eq!(normalized(&["lan"]), ["lan"]);

        let parsed = Cli::try_parse_from(["lan"]).expect("parses");
        assert!(
            parsed.command.is_none(),
            "no subcommand is handled by main as usage, never as a server"
        );
    }

    #[test]
    fn every_subcommand_still_reaches_itself() {
        for subcommand in SUBCOMMANDS {
            assert_eq!(
                normalized(&["lan", subcommand]),
                ["lan", subcommand],
                "{subcommand} must not be rewritten as a prompt"
            );
        }
    }

    #[test]
    fn the_top_level_still_answers_for_help_and_version() {
        for flag in TOP_LEVEL_FLAGS {
            assert_eq!(normalized(&["lan", flag]), ["lan", flag]);
        }
    }
}
