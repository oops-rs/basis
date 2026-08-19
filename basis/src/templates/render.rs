//! Substituting arguments into a template body.
//!
//! One left-to-right pass over the body. Substituted text is appended to the
//! output and never rescanned, so an argument that happens to contain `$1` is
//! delivered as the person typed it rather than expanded again — the property
//! that keeps this from being an injection surface for the argument string.

/// The placeholder for the whole argument string, without its `$`.
const ARGUMENTS: &str = "ARGUMENTS";

/// Renders `body` with `args` substituted in. See [`Template::render`] for the
/// rules this implements.
///
/// [`Template::render`]: super::Template::render
pub fn render(body: &str, args: &str) -> String {
    let args = args.trim();
    let positionals: Vec<&str> = args.split_whitespace().collect();

    let (rendered, referenced) = substitute(body, args, &positionals);
    if referenced || args.is_empty() {
        return rendered;
    }

    append(&rendered, args)
}

/// Walks the body once, returning the result and whether the body asked for
/// arguments at all. `$$` does not count as asking: it is an escape, and a body
/// whose only `$` is escaped has said nothing about arguments.
fn substitute(body: &str, args: &str, positionals: &[&str]) -> (String, bool) {
    let mut out = String::with_capacity(body.len());
    let mut referenced = false;
    let mut rest = body;

    while let Some(at) = rest.find('$') {
        out.push_str(&rest[..at]);
        let tail = &rest[at + 1..];

        if let Some(after) = tail.strip_prefix('$') {
            out.push('$');
            rest = after;
        } else if let Some(after) = tail.strip_prefix(ARGUMENTS) {
            out.push_str(args);
            referenced = true;
            rest = after;
        } else if let Some((index, after)) = positional(tail) {
            // A position nobody supplied renders empty. These are prompts: an
            // absent optional argument should leave a gap, not fail a run.
            out.push_str(positionals.get(index - 1).copied().unwrap_or(""));
            referenced = true;
            rest = after;
        } else {
            // A `$` in front of anything else is prose. `$5` is the cost of
            // that decision — write `$$5` to keep it.
            out.push('$');
            rest = tail;
        }
    }

    out.push_str(rest);
    (out, referenced)
}

/// Reads a positional index from the digits after a `$`.
///
/// The whole digit run is the index, so `$10` is the tenth argument rather than
/// the first followed by a zero. A template has no `${…}` form to say the other
/// thing, and "argument ten" is the only reading an author plausibly meant.
/// `$0` names nothing — arguments are numbered from one — so it stays literal.
fn positional(tail: &str) -> Option<(usize, &str)> {
    let digits = tail.len() - tail.trim_start_matches(|c: char| c.is_ascii_digit()).len();
    if digits == 0 {
        return None;
    }

    // A run too long to be a number is prose, not an argument.
    let index: usize = tail[..digits].parse().ok()?;
    if index == 0 {
        return None;
    }

    Some((index, &tail[digits..]))
}

/// Adds arguments a body never referenced.
///
/// Dropping them would mean a person typed something the model never saw. A
/// blank line separates them so the prompt does not run into them mid-sentence.
fn append(rendered: &str, args: &str) -> String {
    let body = rendered.trim_end();
    if body.is_empty() {
        return args.to_string();
    }

    format!("{body}\n\n{args}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arguments_replaces_the_whole_string() {
        assert_eq!(
            render("Review $ARGUMENTS please.", "the auth module"),
            "Review the auth module please."
        );
    }

    #[test]
    fn arguments_is_trimmed_before_substitution() {
        assert_eq!(render("[$ARGUMENTS]", "  spaced  "), "[spaced]");
    }

    #[test]
    fn positionals_split_on_whitespace() {
        assert_eq!(render("$1 then $2", "first second"), "first then second");
    }

    #[test]
    fn repeated_whitespace_does_not_create_empty_positionals() {
        assert_eq!(render("$1|$2", "a \t\n  b"), "a|b");
    }

    #[test]
    fn a_positional_may_be_used_more_than_once() {
        assert_eq!(render("$1 and $1", "twice"), "twice and twice");
    }

    #[test]
    fn a_position_nobody_supplied_renders_empty() {
        assert_eq!(render("[$1][$2][$3]", "only"), "[only][][]");
    }

    #[test]
    fn a_two_digit_position_is_the_tenth_argument_not_the_first() {
        let args = "a b c d e f g h i j";

        assert_eq!(render("$10", args), "j");
    }

    #[test]
    fn a_dollar_zero_is_literal() {
        assert_eq!(render("cost $0 today", "x"), "cost $0 today\n\nx");
    }

    #[test]
    fn a_doubled_dollar_is_one_literal_dollar() {
        assert_eq!(render("costs $$5", "x"), "costs $5\n\nx");
    }

    #[test]
    fn an_escaped_dollar_does_not_count_as_referencing_arguments() {
        // `$$1` is a literal `$1`, so the body still never asked for anything
        // and the arguments must still arrive.
        assert_eq!(render("literal $$1", "given"), "literal $1\n\ngiven");
    }

    #[test]
    fn a_dollar_before_a_letter_is_prose() {
        assert_eq!(render("$foo stays", ""), "$foo stays");
    }

    #[test]
    fn a_trailing_dollar_is_prose() {
        assert_eq!(render("ends with $", ""), "ends with $");
    }

    #[test]
    fn a_digit_run_too_long_to_be_a_number_is_prose() {
        let body = "$99999999999999999999999999";

        assert_eq!(render(body, ""), body);
    }

    #[test]
    fn a_body_with_no_placeholders_still_receives_the_arguments() {
        assert_eq!(
            render("Summarize the diff.\n", "since main"),
            "Summarize the diff.\n\nsince main"
        );
    }

    #[test]
    fn a_body_that_referenced_anything_gets_nothing_extra() {
        // `$1` used, `$2` ignored: the author took control, so basis does not
        // second-guess by appending the leftovers.
        assert_eq!(render("Fix $1.", "auth login"), "Fix auth.");
    }

    #[test]
    fn an_empty_argument_string_leaves_the_body_alone() {
        assert_eq!(render("Just do it.\n", "   "), "Just do it.\n");
    }

    #[test]
    fn an_empty_body_renders_as_the_arguments_alone() {
        assert_eq!(render("\n\n", "do this"), "do this");
    }

    #[test]
    fn placeholders_render_empty_when_nothing_was_supplied() {
        assert_eq!(render("[$ARGUMENTS][$1]", ""), "[][]");
    }

    #[test]
    fn substituted_text_is_not_rescanned() {
        // The argument contains what looks like a placeholder; it must arrive
        // as typed rather than expand a second time.
        assert_eq!(
            render("$ARGUMENTS", "$1 and $ARGUMENTS"),
            "$1 and $ARGUMENTS"
        );
    }

    #[test]
    fn arguments_may_abut_other_text() {
        assert_eq!(render("$ARGUMENTSand", "x"), "xand");
    }

    #[test]
    fn multibyte_text_around_a_placeholder_survives() {
        assert_eq!(render("修改 $1 文件", "配置"), "修改 配置 文件");
    }
}
