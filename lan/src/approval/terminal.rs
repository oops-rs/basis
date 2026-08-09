//! Asking a person at a terminal.

use std::io::{BufRead, IsTerminal, Write};

use super::{ApprovalDecision, ApprovalRequest, Approver};

/// Asks on the terminal and reads a single-key answer.
///
/// Prompts go to stderr and answers come from stdin, so a run whose stdout is
/// being piped still asks the person rather than writing the question into
/// the pipe.
#[derive(Debug, Default)]
pub struct TerminalApprover {
    /// What to answer when there is nobody to ask.
    fallback: ApprovalDecision,
}

impl TerminalApprover {
    /// An approver that denies when stdin is not a terminal.
    ///
    /// Denying is the right fallback for an unattended run: it fails safe,
    /// visibly, instead of quietly granting whatever was asked for.
    pub fn new() -> Self {
        Self {
            fallback: ApprovalDecision::Deny,
        }
    }

    /// Overrides what happens with no terminal attached.
    pub fn with_fallback(self, fallback: ApprovalDecision) -> Self {
        Self { fallback }
    }

    fn ask(&self, request: &ApprovalRequest) -> std::io::Result<ApprovalDecision> {
        let mut stderr = std::io::stderr();
        writeln!(stderr)?;
        writeln!(stderr, "  lan: {} wants to run", request.tool_name)?;
        writeln!(stderr, "       {}", request.description)?;
        if let Some(summary) = summarize_input(&request.input) {
            writeln!(stderr, "       {summary}")?;
        }
        write!(stderr, "       allow? [y]es / [n]o / [a]lways / neve[r]: ")?;
        stderr.flush()?;

        let mut answer = String::new();
        std::io::stdin().lock().read_line(&mut answer)?;

        Ok(match answer.trim().to_ascii_lowercase().as_str() {
            "y" | "yes" => ApprovalDecision::Allow,
            "a" | "always" => ApprovalDecision::AllowForSession,
            "r" | "never" => ApprovalDecision::DenyForSession,
            // Anything else, including a bare newline, is a refusal: the
            // safe answer should be the easy one to give.
            _ => ApprovalDecision::Deny,
        })
    }
}

impl Approver for TerminalApprover {
    fn approve(&mut self, request: &ApprovalRequest) -> ApprovalDecision {
        if !std::io::stdin().is_terminal() {
            return self.fallback;
        }

        self.ask(request).unwrap_or(self.fallback)
    }
}

/// One line describing what the tool was asked to do.
///
/// A shell command is the thing a person actually needs to see; for anything
/// else the top-level keys are enough to tell one call from another without
/// pasting a wall of JSON into the prompt.
fn summarize_input(input: &serde_json::Value) -> Option<String> {
    if let Some(command) = input.get("command").and_then(|value| value.as_str()) {
        return Some(truncate(command, 160));
    }

    let object = input.as_object()?;
    if object.is_empty() {
        return None;
    }

    let keys: Vec<&str> = object.keys().map(String::as_str).collect();
    Some(truncate(&keys.join(", "), 160))
}

fn truncate(text: &str, limit: usize) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= limit {
        return trimmed.to_string();
    }
    let kept: String = trimmed.chars().take(limit).collect();
    format!("{kept}…")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn request(input: serde_json::Value) -> ApprovalRequest {
        ApprovalRequest {
            request_id: "r1".to_string(),
            tool_call_id: "tc-1".to_string(),
            tool_name: "shell".to_string(),
            description: "wants to run a command".to_string(),
            input,
        }
    }

    #[test]
    fn with_no_terminal_the_fallback_answers() {
        // The suite runs without a terminal on stdin, so this exercises the
        // real path rather than a simulated one.
        let mut approver = TerminalApprover::new();
        assert_eq!(
            approver.approve(&request(json!({}))),
            ApprovalDecision::Deny
        );

        let mut permissive = TerminalApprover::new().with_fallback(ApprovalDecision::Allow);
        assert_eq!(
            permissive.approve(&request(json!({}))),
            ApprovalDecision::Allow
        );
    }

    #[test]
    fn a_shell_command_is_shown_verbatim() {
        let summary = summarize_input(&json!({"command": "rm -rf build", "cwd": "/repo"}))
            .expect("a command is worth showing");

        assert_eq!(summary, "rm -rf build");
    }

    #[test]
    fn other_inputs_collapse_to_their_keys() {
        let summary = summarize_input(&json!({"operations": [], "workingDirectory": "/repo"}))
            .expect("keys are enough to tell calls apart");

        assert!(summary.contains("operations"));
        assert!(summary.contains("workingDirectory"));
    }

    #[test]
    fn an_empty_input_says_nothing() {
        assert_eq!(summarize_input(&json!({})), None);
        assert_eq!(summarize_input(&json!("not an object")), None);
    }

    #[test]
    fn a_long_command_is_cut_rather_than_flooding_the_prompt() {
        let long = "echo ".to_string() + &"x".repeat(500);
        let summary = summarize_input(&json!({ "command": long })).expect("a summary");

        assert!(summary.chars().count() <= 161, "160 plus the ellipsis");
        assert!(summary.ends_with('…'));
    }
}
