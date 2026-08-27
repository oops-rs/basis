//! The host-owned part of Mentra's runtime policy for tool-result delivery.
//!
//! Basis derives command, filesystem, and process authority elsewhere. A host
//! that only needs to protect large structured results should not have to
//! replace that whole policy, so this type carries exactly the three result
//! settings Basis can overlay without changing any other runtime behavior.

use mentra::RuntimePolicy;

/// Limits applied to a completed tool result before the next provider request.
///
/// `max_bytes` counts the serialized result body. `max_physical_lines` counts
/// newline-delimited physical lines, independently of display wrapping.
/// `spill_full_output` decides whether Mentra may preserve a truncated body in
/// the runtime's configured artifact store. A store that disallows artifacts
/// still prevents spilling even when this value is `true`.
///
/// This deliberately does not implement [`Default`]. An omitted
/// [`RuntimeBuilder::with_tool_result_policy`](super::RuntimeBuilder::with_tool_result_policy)
/// call leaves Mentra's current defaults intact; restating those values here
/// would create a second default that could drift.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolResultPolicy {
    max_bytes: usize,
    max_physical_lines: usize,
    spill_full_output: bool,
}

impl ToolResultPolicy {
    /// Creates an explicit tool-result policy.
    #[must_use]
    pub const fn new(max_bytes: usize, max_physical_lines: usize, spill_full_output: bool) -> Self {
        Self {
            max_bytes,
            max_physical_lines,
            spill_full_output,
        }
    }

    /// Preserves complete result bodies in memory and never spills them.
    ///
    /// Both limits map to `usize::MAX`; the no-spill posture is explicit even
    /// though those limits should make spill unreachable for representable
    /// in-memory results.
    #[must_use]
    pub const fn unlimited() -> Self {
        Self::new(usize::MAX, usize::MAX, false)
    }

    /// Maximum serialized bytes retained in a provider-visible result.
    #[must_use]
    pub const fn max_bytes(self) -> usize {
        self.max_bytes
    }

    /// Maximum physical lines retained in a provider-visible result.
    #[must_use]
    pub const fn max_physical_lines(self) -> usize {
        self.max_physical_lines
    }

    /// Whether a truncated full result may be written to the artifact store.
    #[must_use]
    pub const fn spills_full_output(self) -> bool {
        self.spill_full_output
    }

    pub(super) fn apply_to(self, policy: RuntimePolicy) -> RuntimePolicy {
        policy
            .with_max_tool_result_bytes(self.max_bytes)
            .with_max_tool_result_lines(self.max_physical_lines)
            .spill_full_tool_output(self.spill_full_output)
    }
}
