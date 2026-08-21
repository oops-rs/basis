//! The session settings a client may change: which model answers, and how hard
//! it thinks.
//!
//! ACP 2.0 standardised this as `session/set_config_option`, with the available
//! options advertised on the `session/new`, `session/load` and `session/resume`
//! responses and every change echoed back as a `ConfigOptionUpdate`. It is the
//! protocol's answer to the six RPC commands pi spends on "change model" and
//! "change thinking level" — one method, one enumerable list, and a category
//! that tells a client which control to draw.
//!
//! # Why the ids live here and not in the core
//!
//! The same argument [`mode`](crate::mode) makes. basis has
//! [`Effort`] — five levels and an unset — and mentra has
//! [`Session::set_model`](mentra::Session::set_model), and neither is
//! *enumerable* the way a picker needs: ACP wants an id, a label, and a
//! description per value, which is a protocol binding and belongs with the
//! protocol.
//!
//! # What the model list is, and is not
//!
//! One entry: the model the session is on. That is a limitation with a reason
//! rather than a stub. mentra's `Runtime::list_models` asks the provider every
//! time it is called and caches nothing, so advertising a real catalogue would
//! put a network round trip on every `session/new` — on the dispatch loop,
//! where `session/new` answers (ADR-0007).
//!
//! The list is advice, not an allowlist, and [`change`] enforces nothing:
//! a client that sends an id basis never listed gets it, because mentra does
//! not check either and a self-hosted endpoint's model names are exactly the
//! ones basis has never heard of. An id the provider rejects fails on the next
//! turn, where the provider can say why.

use agent_client_protocol::schema::v1::{
    SessionConfigId, SessionConfigOption, SessionConfigOptionCategory, SessionConfigOptionValue,
    SessionConfigSelectOption, SessionConfigValueId,
};

use basis::Effort;

/// Option ids on the wire. basis chooses them, a client echoes them back, and
/// [`change`] reads them — a contract with ourselves, so it lives in one place.
pub(crate) const MODEL: &str = "model";
pub(crate) const EFFORT: &str = "effort";

/// The value id for "leave the effort alone".
///
/// Not "the provider's default", though that is usually what it is: a session
/// opened by an operator who asked for an effort is already at that level, and
/// mentra offers no way to read the level back off a session. So this value
/// means *unset by this session*, and its description says so.
const AS_OPENED: &str = "default";

const LOW: &str = "low";
const MEDIUM: &str = "medium";
const HIGH: &str = "high";
const XHIGH: &str = "xhigh";
const MAX: &str = "max";

/// What a client asked to change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    /// The five levels, or `None` to stop asking for one.
    Effort(Option<Effort>),
    /// Whatever the client sent, unchecked — see the module docs.
    Model(String),
}

/// Why a `session/set_config_option` was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigError {
    /// An option id basis never advertised.
    UnknownOption(String),
    /// A value id for an option basis does advertise, but not one of its
    /// values.
    UnknownValue { option: &'static str, value: String },
    /// A boolean where a select was advertised. ACP types the value by the
    /// shape rather than by the option, so this is reachable.
    WrongShape(&'static str),
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownOption(id) => write!(f, "unknown configuration option: {id}"),
            Self::UnknownValue { option, value } => {
                write!(f, "{option} has no value {value}")
            }
            Self::WrongShape(option) => write!(f, "{option} takes one of its listed values"),
        }
    }
}

/// Reads a `session/set_config_option` into the change it asks for.
///
/// Every refusal names what was wrong, because the alternative — an option that
/// answers `Ok` and does nothing — is the failure a picker cannot show.
pub fn change(
    config_id: &SessionConfigId,
    value: &SessionConfigOptionValue,
) -> Result<Change, ConfigError> {
    match &*config_id.0 {
        MODEL => Ok(Change::Model(selected(value, MODEL)?.to_string())),
        EFFORT => Ok(Change::Effort(effort_for(selected(value, EFFORT)?)?)),
        unknown => Err(ConfigError::UnknownOption(unknown.to_string())),
    }
}

/// The value id a client picked, for an option that offers a list of them.
fn selected<'a>(
    value: &'a SessionConfigOptionValue,
    option: &'static str,
) -> Result<&'a str, ConfigError> {
    value
        .as_value_id()
        .map(|id| &*id.0)
        .ok_or(ConfigError::WrongShape(option))
}

fn effort_for(id: &str) -> Result<Option<Effort>, ConfigError> {
    match id {
        AS_OPENED => Ok(None),
        LOW => Ok(Some(Effort::Low)),
        MEDIUM => Ok(Some(Effort::Medium)),
        HIGH => Ok(Some(Effort::High)),
        XHIGH => Ok(Some(Effort::XHigh)),
        MAX => Ok(Some(Effort::Max)),
        unknown => Err(ConfigError::UnknownValue {
            option: EFFORT,
            value: unknown.to_string(),
        }),
    }
}

fn effort_id(effort: Option<Effort>) -> &'static str {
    match effort {
        None => AS_OPENED,
        Some(Effort::Low) => LOW,
        Some(Effort::Medium) => MEDIUM,
        Some(Effort::High) => HIGH,
        Some(Effort::XHigh) => XHIGH,
        Some(Effort::Max) => MAX,
        // `Effort` is `#[non_exhaustive]`, so a level added upstream reaches
        // here with no id of its own. ACP's select expects `currentValue` to
        // be one of the listed values, and an id that is in neither the list
        // nor the client's vocabulary leaves the picker with nothing selected
        // at all. Falling back to the one value that is always listed is
        // wrong about the level and right about the shape; the fix is a new
        // id here, and the test below is what notices it is missing.
        _ => AS_OPENED,
    }
}

/// The options a session reports, with the values it is currently on.
///
/// Sent whole, every time. ACP's `SetSessionConfigOptionResponse` and
/// `ConfigOptionUpdate` both carry "the full set of configuration options and
/// their current values", so there is one shape to build and a client that
/// missed an update is corrected by the next one.
pub fn options(model: &str, effort: Option<Effort>) -> Vec<SessionConfigOption> {
    vec![
        SessionConfigOption::select(
            MODEL,
            "Model",
            SessionConfigValueId::new(model),
            vec![SessionConfigSelectOption::new(
                SessionConfigValueId::new(model),
                model,
            )],
        )
        .category(SessionConfigOptionCategory::Model)
        .description(
            "The model this conversation's next turn runs on. \
             Any id this provider serves is accepted, listed or not.",
        ),
        SessionConfigOption::select(
            EFFORT,
            "Reasoning effort",
            effort_id(effort),
            effort_values(),
        )
        .category(SessionConfigOptionCategory::ThoughtLevel)
        .description(
            "How hard the model thinks before answering. \
             A level this provider or model does not offer fails the turn \
             rather than being quietly lowered.",
        ),
    ]
}

fn effort_values() -> Vec<SessionConfigSelectOption> {
    vec![
        SessionConfigSelectOption::new(AS_OPENED, "Default").description(
            "Leave the effort as the session was opened with — the provider's own, \
             unless the operator asked for one.",
        ),
        SessionConfigSelectOption::new(LOW, "Low")
            .description("Answer quickly. For gathering, not for deciding."),
        SessionConfigSelectOption::new(MEDIUM, "Medium").description("The usual trade."),
        SessionConfigSelectOption::new(HIGH, "High")
            .description("Think before answering. Slower, and worth it on a hard change."),
        SessionConfigSelectOption::new(XHIGH, "Extra high")
            .description("More than high, where the provider offers a level above it."),
        SessionConfigSelectOption::new(MAX, "Max")
            .description("Everything the model has. The slowest and the most expensive."),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::v1::{SessionConfigKind, SessionConfigSelectOptions};

    fn select(options: &[SessionConfigOption], id: &str) -> SessionConfigSelectOptions {
        let option = options
            .iter()
            .find(|option| &*option.id.0 == id)
            .unwrap_or_else(|| panic!("{id} is advertised"));

        match &option.kind {
            SessionConfigKind::Select(select) => select.options.clone(),
            other => panic!("{id} should be a select, got {other:?}"),
        }
    }

    fn current(options: &[SessionConfigOption], id: &str) -> String {
        let option = options
            .iter()
            .find(|option| &*option.id.0 == id)
            .unwrap_or_else(|| panic!("{id} is advertised"));

        match &option.kind {
            SessionConfigKind::Select(select) => select.current_value.0.to_string(),
            other => panic!("{id} should be a select, got {other:?}"),
        }
    }

    #[test]
    fn a_session_advertises_its_model_and_its_effort() {
        let options = options("gpt-5", Some(Effort::High));

        assert_eq!(options.len(), 2);
        assert_eq!(current(&options, MODEL), "gpt-5");
        assert_eq!(current(&options, EFFORT), HIGH);
        assert_eq!(
            options
                .iter()
                .map(|option| option.category.clone())
                .collect::<Vec<_>>(),
            vec![
                Some(SessionConfigOptionCategory::Model),
                Some(SessionConfigOptionCategory::ThoughtLevel)
            ],
            "the category is what tells a client which control to draw"
        );
    }

    #[test]
    fn every_offered_value_maps_back_to_one_lan_can_read() {
        // A value basis sends but cannot read back would be a picker that
        // silently does nothing.
        let SessionConfigSelectOptions::Ungrouped(values) = select(&options("m", None), EFFORT)
        else {
            panic!("effort is an ungrouped list");
        };

        for value in values {
            let read = effort_for(&value.value.0).unwrap_or_else(|error| {
                panic!("offered {} but cannot read it back: {error}", value.value.0)
            });
            assert_eq!(
                effort_id(read),
                &*value.value.0,
                "and it must round-trip to the same id"
            );
        }
    }

    #[test]
    fn the_model_list_names_the_model_the_session_is_on() {
        // One entry, because listing a provider's catalogue is a network round
        // trip mentra does not cache — see the module docs. The current model
        // is the one thing basis can list without asking anyone.
        let SessionConfigSelectOptions::Ungrouped(values) =
            select(&options("local-llama", None), MODEL)
        else {
            panic!("the model list is ungrouped");
        };

        assert_eq!(values.len(), 1);
        assert_eq!(&*values[0].value.0, "local-llama");
    }

    #[test]
    fn a_model_lan_never_listed_is_still_accepted() {
        // The list is advice. mentra does not check the id either, and the
        // models basis has never heard of are exactly the ones a self-hosted
        // endpoint serves.
        assert_eq!(
            change(
                &SessionConfigId::new(MODEL),
                &SessionConfigOptionValue::value_id("some-model-nobody-listed")
            ),
            Ok(Change::Model("some-model-nobody-listed".to_string()))
        );
    }

    #[test]
    fn an_effort_lan_never_offered_is_refused() {
        // Unlike a model: the five levels are basis's own enum, so a sixth is
        // not a value the provider might know — it is a value nothing can act
        // on, and accepting it would leave the picker showing a level the
        // session is not at.
        assert_eq!(
            change(
                &SessionConfigId::new(EFFORT),
                &SessionConfigOptionValue::value_id("ludicrous")
            ),
            Err(ConfigError::UnknownValue {
                option: EFFORT,
                value: "ludicrous".to_string()
            })
        );
    }

    #[test]
    fn an_unknown_option_is_refused_by_name() {
        let error = change(
            &SessionConfigId::new("temperature"),
            &SessionConfigOptionValue::value_id("0.7"),
        )
        .expect_err("basis advertises no such option");

        assert_eq!(error, ConfigError::UnknownOption("temperature".to_string()));
        assert!(
            error.to_string().contains("temperature"),
            "the message must name what was refused: {error}"
        );
    }

    #[test]
    fn a_boolean_is_not_a_choice_from_a_list() {
        // ACP types the value by its shape rather than by the option it is for,
        // so a client can send one. Answering `Ok` to it would report a change
        // that did not happen.
        assert_eq!(
            change(
                &SessionConfigId::new(EFFORT),
                &SessionConfigOptionValue::boolean(true)
            ),
            Err(ConfigError::WrongShape(EFFORT))
        );
    }

    #[test]
    fn clearing_the_effort_is_a_value_and_not_an_absence() {
        // "Back to how it opened" has to be sendable: ACP's select has no
        // "unset", so basis gives that state an id of its own.
        assert_eq!(
            change(
                &SessionConfigId::new(EFFORT),
                &SessionConfigOptionValue::value_id(AS_OPENED)
            ),
            Ok(Change::Effort(None))
        );
        assert_eq!(current(&options("m", None), EFFORT), AS_OPENED);
    }
}
