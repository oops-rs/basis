//! Asking a person at a terminal.
//!
//! The TTY half of [`lan_core::approval`], which is why it lives in the binary
//! rather than the core: a library has no terminal to ask at (ADR-0011).

use std::io::{BufRead, IsTerminal, Write};

use lan_core::{ApprovalDecision, ApprovalRequest, Approver};

/// Asks on the terminal and reads a single-key answer.
///
/// Prompts go to stderr and answers come from stdin, so a run whose stdout is
/// being piped still asks the person rather than writing the question into
/// the pipe.
///
/// With nothing to ask — stdin is not a terminal — the answer is always
/// [`ApprovalDecision::Deny`]. That is the right fallback for an unattended
/// run: it fails safe, visibly, instead of quietly granting whatever was asked
/// for, and it is what `lan run --approve prompt` documents.
#[derive(Debug, Default)]
pub struct TerminalApprover;

impl TerminalApprover {
    pub fn new() -> Self {
        Self
    }
}

/// Asks on the terminal and reads the answer. Blocking by nature.
fn ask(request: &ApprovalRequest) -> std::io::Result<ApprovalDecision> {
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

#[async_trait::async_trait]
impl Approver for TerminalApprover {
    async fn approve(&mut self, request: &ApprovalRequest) -> ApprovalDecision {
        if !std::io::stdin().is_terminal() {
            return ApprovalDecision::Deny;
        }

        let request = request.clone();

        // Reading stdin blocks for as long as the person takes, so it belongs
        // on a blocking thread rather than a runtime worker.
        tokio::task::spawn_blocking(move || ask(&request).unwrap_or(ApprovalDecision::Deny))
            .await
            .unwrap_or(ApprovalDecision::Deny)
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

    #[tokio::test]
    async fn with_no_terminal_the_request_is_denied() {
        // The suite runs without a terminal on stdin, so this exercises the
        // real path rather than a simulated one.
        let mut approver = TerminalApprover::new();
        assert_eq!(
            approver.approve(&request(json!({}))).await,
            ApprovalDecision::Deny,
            "an unattended run must fail safe rather than grant what it cannot ask about"
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
