//! Which agents `@<name>` may reach (ADR-0037 §3).
//!
//! One name, two kinds of agent it can be: a remote agent a resolver knows,
//! or a teammate on this runtime. `auto` asks the resolvers and falls through
//! to a teammate; `remote` refuses a name no resolver knows, so a typo is a
//! refusal rather than a new teammate; `local` never asks, so a teammate can
//! share a name with a remote agent without reaching it. A plain task with
//! no `@` ignores the field.
//!
//! The field is typed beside the mode rather than read out of the string, so
//! ADR-0016's rule holds: the string is parsed once, at the edge, and nothing
//! downstream re-reads it.

use std::collections::BTreeSet;

/// Which agents `@<name>` may reach.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum Reach {
    /// A remote agent by that name when a resolver knows one, otherwise a
    /// teammate here. The default.
    #[default]
    Auto,
    /// A remote agent only; a name no resolver knows is refused.
    Remote,
    /// A teammate here only; no resolver is asked.
    Local,
}

impl Reach {
    /// The wire spelling: what the model writes, what the preview carries,
    /// and what a remembered rule globs against.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Remote => "remote",
            Self::Local => "local",
        }
    }

    /// Every value, in the order the model is taught them.
    pub const ALL: [Self; 3] = [Self::Auto, Self::Remote, Self::Local];

    pub(crate) fn parse(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|reach| reach.as_str() == text)
    }
}

/// What a host allows `reach` to be, and what it means when the model says
/// nothing (ADR-0037 §3).
///
/// The default allows every value and reads an absent field as
/// [`Reach::Auto`]. A host that serves only remote agents fixes it to
/// [`Reach::Remote`], which is the behavior before ADR-0037: an unknown name
/// is a refusal. Runtime-scoped, like the child policy and for the same
/// reason: `spawn` is one tool however many copies of it there are.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReachPolicy {
    default: Reach,
    allowed: BTreeSet<Reach>,
}

impl Default for ReachPolicy {
    fn default() -> Self {
        Self {
            default: Reach::Auto,
            allowed: Reach::ALL.into_iter().collect(),
        }
    }
}

impl ReachPolicy {
    /// Allows only `reach`, and reads an absent field as it.
    pub fn fixed(reach: Reach) -> Self {
        Self {
            default: reach,
            allowed: BTreeSet::from([reach]),
        }
    }

    /// Reads an absent field as `reach`, which is allowed whatever else is.
    #[must_use]
    pub fn with_default(mut self, reach: Reach) -> Self {
        self.default = reach;
        self.allowed.insert(reach);
        self
    }

    /// The value an absent field means.
    pub fn default_reach(&self) -> Reach {
        self.default
    }

    /// Whether the model may say `reach`.
    pub fn allows(&self, reach: Reach) -> bool {
        self.allowed.contains(&reach)
    }

    /// Every allowed value, in [`Reach::ALL`]'s order.
    pub fn allowed(&self) -> impl Iterator<Item = Reach> + '_ {
        Reach::ALL
            .into_iter()
            .filter(|reach| self.allowed.contains(reach))
    }

    /// Whether the model has any choice: more than one value allowed.
    pub(crate) fn offers_a_choice(&self) -> bool {
        self.allowed.len() > 1
    }

    /// What `said` means under this policy, or why it is refused.
    pub(crate) fn decide(&self, said: Option<Reach>) -> Result<Reach, String> {
        let reach = said.unwrap_or(self.default);
        if self.allows(reach) {
            return Ok(reach);
        }
        let allowed: Vec<String> = self
            .allowed()
            .map(|reach| format!("`{}`", reach.as_str()))
            .collect();
        Err(format!(
            "`reach: {}` is not allowed here; {}",
            reach.as_str(),
            if allowed.len() == 1 {
                format!(
                    "every `@<name>` reaches {} agents",
                    allowed[0].trim_matches('`')
                )
            } else {
                format!("`reach` may be {}", allowed.join(" or "))
            }
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_spellings_are_the_ones_rules_are_written_against() {
        assert_eq!(Reach::Auto.as_str(), "auto");
        assert_eq!(Reach::Remote.as_str(), "remote");
        assert_eq!(Reach::Local.as_str(), "local");
        for reach in Reach::ALL {
            assert_eq!(Reach::parse(reach.as_str()), Some(reach));
        }
        assert_eq!(Reach::parse("anywhere"), None);
    }

    #[test]
    fn the_default_policy_allows_everything_and_reads_silence_as_auto() {
        let policy = ReachPolicy::default();
        assert_eq!(policy.decide(None), Ok(Reach::Auto));
        for reach in Reach::ALL {
            assert_eq!(policy.decide(Some(reach)), Ok(reach));
        }
        assert!(policy.offers_a_choice());
    }

    #[test]
    fn a_fixed_policy_refuses_the_rest_and_names_what_every_name_reaches() {
        let policy = ReachPolicy::fixed(Reach::Remote);
        assert_eq!(policy.decide(None), Ok(Reach::Remote));
        let refusal = policy.decide(Some(Reach::Local)).expect_err("not allowed");
        assert!(
            refusal.contains("`reach: local` is not allowed"),
            "{refusal}"
        );
        assert!(refusal.contains("reaches remote agents"), "{refusal}");
        assert!(!policy.offers_a_choice());
    }

    #[test]
    fn a_default_is_always_allowed() {
        let policy = ReachPolicy::fixed(Reach::Remote).with_default(Reach::Auto);
        assert_eq!(policy.decide(None), Ok(Reach::Auto));
        assert_eq!(
            policy.allowed().collect::<Vec<_>>(),
            [Reach::Auto, Reach::Remote]
        );
        let refusal = policy.decide(Some(Reach::Local)).expect_err("not allowed");
        assert!(refusal.contains("`auto` or `remote`"), "{refusal}");
    }
}
