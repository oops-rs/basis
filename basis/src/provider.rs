//! Choosing a provider and finding its credential.
//!
//! Which model answers is configuration, not a basis opinion — but *finding* the
//! credential is glue every embedder would otherwise write, so basis does it
//! once, by the environment-variable names the ecosystem already uses.
//!
//! # Nothing here repeats what it read
//!
//! The value this module goes looking for is a credential, so
//! [`ProviderChoice`]'s `Debug` redacts it. That is not hypothetical tidiness:
//! a resolution test that failed with the wrong variables exported printed a
//! live key into a terminal, because `expect` formats the `Ok` it did not
//! want. The same rule as [`WorkspaceBuilder`](crate::WorkspaceBuilder)'s own
//! `Debug`.
//!
//! # The environment is a parameter
//!
//! Resolution consults the environment in three places — the base URL, the
//! compatible-endpoint key, and auto-detection — which is enough to make every
//! test of it a test of the shell that started it. So the lookup is passed in,
//! exactly as `crate::mcp` passes one to `${VAR}` expansion, and the rules
//! below can be pinned without mutating the process's own environment.

use mentra::BuiltinProvider;
use thiserror::Error;

/// A hosted provider basis can select automatically, paired with the environment
/// variable holding its key.
///
/// Order is the auto-detection preference when several keys are present.
/// Local providers (Ollama, LM Studio) are deliberately absent: they have no
/// key to detect, so selecting one is always an explicit choice — and, named,
/// they resolve without one.
const CANDIDATES: &[(BuiltinProvider, &str)] = &[
    (BuiltinProvider::Anthropic, "ANTHROPIC_API_KEY"),
    (BuiltinProvider::OpenAI, "OPENAI_API_KEY"),
    (BuiltinProvider::Gemini, "GEMINI_API_KEY"),
    (BuiltinProvider::OpenRouter, "OPENROUTER_API_KEY"),
];

/// Environment variables naming a custom OpenAI-compatible endpoint, in
/// preference order. `OPENAI_BASE_URL` is honored because gateways and proxies
/// already tell their users to set it.
const BASE_URL_VARS: &[&str] = &["BASIS_BASE_URL", "OPENAI_BASE_URL"];

/// Environment variables holding the key for a custom endpoint.
const COMPATIBLE_KEY_VARS: &[&str] = &["BASIS_API_KEY", "OPENAI_API_KEY"];

/// A provider together with the key it will authenticate with.
#[derive(Clone)]
pub struct ProviderChoice {
    pub provider: BuiltinProvider,
    /// `None` when the endpoint asked for none: a local preset, or a base URL
    /// with no key anywhere. The request then carries no `Authorization`
    /// header at all, and a server that wanted one answers 401 in its own
    /// words — which is the honest failure, where refusing up front would
    /// have made every Ollama and llama.cpp user invent a key to paste.
    pub api_key: Option<String>,
    /// The variable the key came from, or `None` when it was passed directly.
    pub source_var: Option<&'static str>,
    /// Set when the model lives behind an OpenAI-compatible endpoint rather
    /// than the provider's own service. Already normalized by
    /// [`normalize_base_url`].
    pub base_url: Option<String>,
}

/// Hand-written so a resolved credential cannot reach a log — or a panicking
/// test's output — through a `{:?}`. This is the struct an `expect` on a
/// resolution prints, and the field is a key basis has just read out of the
/// environment, in plain text. Everything else is printed as it is, including
/// `source_var`: naming the variable a key came from is how a caller debugs
/// which one won, and it says nothing about the value.
impl std::fmt::Debug for ProviderChoice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let credential = self
            .api_key
            .as_ref()
            .map(|_| self.source_var.unwrap_or("direct"));
        f.debug_struct("ProviderChoice")
            .field("provider", &self.provider)
            .field("api_key", &crate::redaction::redacted_env(credential))
            .field("source_var", &self.source_var)
            .field("base_url", &self.base_url)
            .finish()
    }
}

impl ProviderChoice {
    /// Whether this choice points at a custom OpenAI-compatible endpoint.
    pub fn is_compatible_endpoint(&self) -> bool {
        self.base_url.is_some()
    }
}

#[derive(Debug, Error)]
pub enum ProviderError {
    #[error(
        "no provider credential found; set one of: {}",
        CANDIDATES.iter().map(|(_, var)| *var).collect::<Vec<_>>().join(", ")
    )]
    NoCredential,

    #[error("{provider} selected but {var} is not set")]
    MissingCredential {
        provider: BuiltinProvider,
        var: &'static str,
    },

    #[error(
        "unknown provider '{0}'; expected one of: anthropic, openai, gemini, openrouter, ollama, lmstudio"
    )]
    Unknown(String),

    #[error("base URL must be an absolute http(s) URL, got '{0}'")]
    InvalidBaseUrl(String),

    #[error("an API key was supplied with no provider and no base URL to attribute it to")]
    UnattributedCredential,

    /// [`RuntimeBuilder::with_provider_instance`](crate::RuntimeBuilder::with_provider_instance)
    /// beside a knob this module's resolution reads. The instance would win,
    /// but a silent priority is a knob that silently stopped meaning anything
    /// — so the pair is refused the way an unattributed credential is, naming
    /// the knob to drop.
    #[error(
        "a provider instance was supplied beside `{knob}`; the instance already answers what \
         {knob} decides, so state one or the other"
    )]
    AmbiguousProviderSource { knob: &'static str },
}

/// Trims a base URL to what mentra's transports expect.
///
/// They append `v1/chat/completions`, `v1/responses` and `v1/models`
/// themselves, but every gateway publishes its URL *with* `/v1` on the end,
/// because that is the form the OpenAI SDKs take. Pasting the published URL
/// would otherwise produce `/v1/v1/…` and a puzzling 404, so strip a trailing
/// `/v1` here rather than making each user discover the difference.
///
/// One rule for both wires, which is what keeps "paste the URL as published"
/// true whichever one [`RuntimeBuilder::with_wire`](crate::RuntimeBuilder::with_wire)
/// selects: they differ in the path they append, never in the base they append
/// it to.
pub fn normalize_base_url(raw: &str) -> Result<String, ProviderError> {
    let trimmed = raw.trim();
    let rest = trimmed
        .strip_prefix("http://")
        .or_else(|| trimmed.strip_prefix("https://"))
        .ok_or_else(|| ProviderError::InvalidBaseUrl(raw.to_string()))?;

    // A scheme with no authority ("https://") would otherwise survive to
    // produce a nonsense request URL.
    let host = rest.split('/').next().unwrap_or_default();
    if host.is_empty() {
        return Err(ProviderError::InvalidBaseUrl(raw.to_string()));
    }

    let without_slash = trimmed.trim_end_matches('/');
    let without_version = without_slash
        .strip_suffix("/v1")
        .unwrap_or(without_slash)
        .trim_end_matches('/');

    if without_version.is_empty() {
        return Err(ProviderError::InvalidBaseUrl(raw.to_string()));
    }

    // A trailing slash is what `url_for_path` expects to join against.
    Ok(format!("{without_version}/"))
}

/// Parses a provider name as written on a command line or in config.
pub fn parse(name: &str) -> Result<BuiltinProvider, ProviderError> {
    match name.trim().to_ascii_lowercase().as_str() {
        "anthropic" => Ok(BuiltinProvider::Anthropic),
        "openai" => Ok(BuiltinProvider::OpenAI),
        "gemini" => Ok(BuiltinProvider::Gemini),
        "openrouter" => Ok(BuiltinProvider::OpenRouter),
        "ollama" => Ok(BuiltinProvider::Ollama),
        "lmstudio" | "lm-studio" => Ok(BuiltinProvider::LmStudio),
        other => Err(ProviderError::Unknown(other.to_string())),
    }
}

/// The environment variable holding `provider`'s key, if it has one.
pub fn key_var(provider: BuiltinProvider) -> Option<&'static str> {
    CANDIDATES
        .iter()
        .find(|(candidate, _)| *candidate == provider)
        .map(|(_, var)| *var)
}

/// Resolves how basis will reach a model, with the credential read from the
/// environment.
///
/// A base URL — passed in, or found in the environment — wins over provider
/// auto-detection: pointing at a specific endpoint is always deliberate, so it
/// should not be silently overridden by whichever key happens to be exported.
pub fn resolve(
    requested: Option<BuiltinProvider>,
    base_url: Option<&str>,
) -> Result<ProviderChoice, ProviderError> {
    resolve_with(requested, base_url, None)
}

/// Resolves how basis will reach a model, with the credential supplied rather
/// than looked up.
///
/// `api_key` of `None` is [`resolve`] — the environment answers. A host that
/// holds its key somewhere basis cannot read, a vault or a token it just
/// exchanged, passes it here instead of exporting a variable for basis to find
/// again ([`RuntimeBuilder::with_api_key`](crate::RuntimeBuilder::with_api_key)).
///
/// A supplied key still has to say *where it is for*: with neither a provider
/// nor a base URL, basis would be choosing a service to send someone's credential
/// to, so that combination is refused.
pub fn resolve_with(
    requested: Option<BuiltinProvider>,
    base_url: Option<&str>,
    api_key: Option<&str>,
) -> Result<ProviderChoice, ProviderError> {
    resolve_against(&|var| std::env::var(var).ok(), requested, base_url, api_key)
}

/// The same, against an explicit environment, so the rules are testable
/// without mutating the process's own.
///
/// Private, and meant to stay that way: a host whose credential lives
/// somewhere basis cannot read passes it to
/// [`RuntimeBuilder::with_api_key`](crate::RuntimeBuilder::with_api_key),
/// and a second, wider way to supply one would be a second thing to keep
/// honest.
fn resolve_against(
    lookup: &dyn Fn(&str) -> Option<String>,
    requested: Option<BuiltinProvider>,
    base_url: Option<&str>,
    api_key: Option<&str>,
) -> Result<ProviderChoice, ProviderError> {
    if let Some(raw) = base_url
        .map(str::to_string)
        .or_else(|| env_base_url(lookup))
    {
        return resolve_compatible(lookup, &raw, requested, api_key);
    }

    match (requested, api_key) {
        (Some(provider), Some(api_key)) => Ok(ProviderChoice {
            provider,
            api_key: Some(api_key.to_string()),
            source_var: None,
            base_url: None,
        }),
        (None, Some(_)) => Err(ProviderError::UnattributedCredential),
        // A local preset has no variable to read, and nothing to read from
        // it: mentra reaches Ollama and LM Studio at their fixed local
        // addresses and ignores whatever key it is handed.
        (Some(provider), None) => match key_var(provider) {
            None => Ok(ProviderChoice {
                provider,
                api_key: None,
                source_var: None,
                base_url: None,
            }),
            Some(var) => {
                let api_key =
                    read(lookup, var).ok_or(ProviderError::MissingCredential { provider, var })?;
                Ok(ProviderChoice {
                    provider,
                    api_key: Some(api_key),
                    source_var: Some(var),
                    base_url: None,
                })
            }
        },
        (None, None) => CANDIDATES
            .iter()
            .find_map(|(provider, var)| {
                read(lookup, var).map(|api_key| ProviderChoice {
                    provider: *provider,
                    api_key: Some(api_key),
                    source_var: Some(var),
                    base_url: None,
                })
            })
            .ok_or(ProviderError::NoCredential),
    }
}

/// A custom endpoint is filed under the OpenAI provider id unless the caller
/// named another — which is only a name for the credential and the model
/// lookup, not a claim about the endpoint's vendor.
///
/// Which *wire* it is spoken to in is decided later, at
/// [`RuntimeBuilder::with_wire`](crate::RuntimeBuilder::with_wire), and
/// defaults to `chat/completions`. Resolution has nothing to say about it: it
/// answers where and with what, never how.
fn resolve_compatible(
    lookup: &dyn Fn(&str) -> Option<String>,
    raw: &str,
    requested: Option<BuiltinProvider>,
    api_key: Option<&str>,
) -> Result<ProviderChoice, ProviderError> {
    let base_url = normalize_base_url(raw)?;
    // No key anywhere is an answer, not an error: the servers a base URL
    // usually names — Ollama, LM Studio, vLLM, llama.cpp on a workstation —
    // take none, and the one that does take one says so with a 401.
    let (api_key, source_var) = match api_key {
        Some(api_key) => (Some(api_key.to_string()), None),
        None => COMPATIBLE_KEY_VARS
            .iter()
            .find_map(|var| read(lookup, var).map(|key| (Some(key), Some(*var))))
            .unwrap_or((None, None)),
    };

    Ok(ProviderChoice {
        provider: requested.unwrap_or(BuiltinProvider::OpenAI),
        api_key,
        source_var,
        base_url: Some(base_url),
    })
}

fn env_base_url(lookup: &dyn Fn(&str) -> Option<String>) -> Option<String> {
    BASE_URL_VARS.iter().find_map(|var| read(lookup, var))
}

/// Treats a variable set to whitespace as absent — an empty key produces a
/// confusing authentication failure much later, and an empty base URL a
/// request to nowhere.
fn read(lookup: &dyn Fn(&str) -> Option<String>, var: &str) -> Option<String> {
    lookup(var).filter(|value| !value.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An environment fixed by the test rather than by the shell that started
    /// it. Every resolution test goes through one of these, because the
    /// variables this module reads are exactly the ones a person working on basis
    /// is likely to have exported.
    fn exporting(vars: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let vars: Vec<(String, String)> = vars
            .iter()
            .map(|(var, value)| (var.to_string(), value.to_string()))
            .collect();

        move |name| {
            vars.iter()
                .find(|(var, _)| var == name)
                .map(|(_, value)| value.clone())
        }
    }

    fn nothing_exported() -> impl Fn(&str) -> Option<String> {
        exporting(&[])
    }

    #[test]
    fn provider_names_parse_case_insensitively() {
        assert_eq!(parse("OpenAI").expect("parses"), BuiltinProvider::OpenAI);
        assert_eq!(
            parse("  anthropic  ").expect("parses"),
            BuiltinProvider::Anthropic
        );
        assert_eq!(
            parse("lm-studio").expect("parses"),
            BuiltinProvider::LmStudio
        );
    }

    #[test]
    fn an_unknown_provider_names_the_alternatives() {
        let error = parse("hal9000").expect_err("rejected");

        assert!(matches!(error, ProviderError::Unknown(name) if name == "hal9000"));
    }

    #[test]
    fn hosted_providers_have_a_key_variable_and_local_ones_do_not() {
        assert_eq!(
            key_var(BuiltinProvider::Anthropic),
            Some("ANTHROPIC_API_KEY")
        );
        assert_eq!(key_var(BuiltinProvider::Ollama), None);
    }

    #[test]
    fn detection_order_prefers_the_first_candidate() {
        let vars: Vec<&str> = CANDIDATES.iter().map(|(_, var)| *var).collect();

        assert_eq!(vars.first(), Some(&"ANTHROPIC_API_KEY"));
        assert_eq!(
            vars.len(),
            4,
            "local providers must not be auto-detection candidates"
        );
    }

    #[test]
    fn detection_takes_the_first_candidate_the_environment_offers() {
        let choice = resolve_against(
            &exporting(&[
                ("OPENAI_API_KEY", "openai-key"),
                ("ANTHROPIC_API_KEY", "anthropic-key"),
            ]),
            None,
            None,
            None,
        )
        .expect("a key is exported");

        assert_eq!(choice.provider, BuiltinProvider::Anthropic);
        assert_eq!(choice.source_var, Some("ANTHROPIC_API_KEY"));
    }

    #[test]
    fn a_named_provider_reads_its_own_variable_and_says_which() {
        let choice = resolve_against(
            &exporting(&[
                ("ANTHROPIC_API_KEY", "anthropic-key"),
                ("GEMINI_API_KEY", "gemini-key"),
            ]),
            Some(BuiltinProvider::Gemini),
            None,
            None,
        )
        .expect("the named provider's key is exported");

        assert_eq!(choice.api_key.as_deref(), Some("gemini-key"));
        assert_eq!(choice.source_var, Some("GEMINI_API_KEY"));
    }

    #[test]
    fn a_variable_set_to_whitespace_is_treated_as_absent() {
        // Otherwise the run fails at the first request, with an
        // authentication error that names nothing useful.
        let error = resolve_against(
            &exporting(&[("ANTHROPIC_API_KEY", "   ")]),
            None,
            None,
            None,
        )
        .expect_err("rejected");

        assert!(matches!(error, ProviderError::NoCredential));
    }

    #[test]
    fn an_environment_base_url_outranks_provider_detection() {
        // Pointing at an endpoint is always deliberate; whichever key happens
        // to be exported is not.
        let choice = resolve_against(
            &exporting(&[
                ("ANTHROPIC_API_KEY", "anthropic-key"),
                ("BASIS_BASE_URL", "http://127.0.0.1:3455/v1"),
                ("BASIS_API_KEY", "gateway-key"),
            ]),
            None,
            None,
            None,
        )
        .expect("a base URL and a key are enough");

        assert_eq!(choice.base_url.as_deref(), Some("http://127.0.0.1:3455/"));
        assert_eq!(choice.api_key.as_deref(), Some("gateway-key"));
        assert_eq!(choice.source_var, Some("BASIS_API_KEY"));
    }

    #[test]
    fn a_base_url_with_no_key_anywhere_goes_without_one() {
        // The common case for a base URL is a server on this machine that
        // takes no key; refusing would make its operator invent one.
        let choice = resolve_against(
            &exporting(&[("BASIS_BASE_URL", "http://127.0.0.1:11434/v1")]),
            None,
            None,
            None,
        )
        .expect("resolves");

        assert_eq!(choice.api_key, None);
        assert_eq!(choice.source_var, None);
        assert_eq!(choice.base_url.as_deref(), Some("http://127.0.0.1:11434/"));
    }

    #[test]
    fn a_local_provider_needs_no_key() {
        let choice = resolve_against(
            &nothing_exported(),
            Some(BuiltinProvider::Ollama),
            None,
            None,
        )
        .expect("resolves");

        assert_eq!(choice.provider, BuiltinProvider::Ollama);
        assert_eq!(choice.api_key, None);
        assert_eq!(
            choice.base_url, None,
            "the preset's own address, not a custom one"
        );
    }

    #[test]
    fn a_named_provider_with_no_key_names_the_variable_it_wanted() {
        let error = resolve_against(
            &nothing_exported(),
            Some(BuiltinProvider::OpenRouter),
            None,
            None,
        )
        .expect_err("rejected");

        assert!(matches!(
            error,
            ProviderError::MissingCredential {
                var: "OPENROUTER_API_KEY",
                ..
            }
        ));
    }

    #[test]
    fn a_supplied_key_is_used_instead_of_the_environment() {
        // The point of supplying one: a host whose credential lives in a vault
        // wants its own key used even where basis could have found another.
        let choice = resolve_against(
            &exporting(&[("ANTHROPIC_API_KEY", "exported-key")]),
            Some(BuiltinProvider::Anthropic),
            None,
            Some("supplied-key"),
        )
        .expect("a named provider and a key need no lookup");

        assert_eq!(choice.api_key.as_deref(), Some("supplied-key"));
        assert_eq!(choice.provider, BuiltinProvider::Anthropic);
        assert_eq!(
            choice.source_var, None,
            "no variable was read, so none may be named"
        );
    }

    #[test]
    fn a_supplied_key_reaches_a_compatible_endpoint() {
        let choice = resolve_against(
            &nothing_exported(),
            None,
            Some("http://127.0.0.1:3455/v1"),
            Some("supplied-key"),
        )
        .expect("a base URL and a key are enough");

        assert_eq!(choice.api_key.as_deref(), Some("supplied-key"));
        assert_eq!(choice.base_url.as_deref(), Some("http://127.0.0.1:3455/"));
        assert!(choice.is_compatible_endpoint());
    }

    #[test]
    fn a_key_with_nothing_to_attribute_it_to_is_refused() {
        // Guessing here would mean picking a service to send someone's
        // credential to.
        let error = resolve_against(&nothing_exported(), None, None, Some("supplied-key"))
            .expect_err("rejected");

        assert!(matches!(error, ProviderError::UnattributedCredential));
    }

    #[test]
    fn a_resolved_credential_is_not_printed() {
        // How this was found: a resolution test failed with a gateway's
        // variables exported, and `expect` printed the live key it had just
        // read into the terminal.
        let choice = resolve_against(
            &exporting(&[("ANTHROPIC_API_KEY", "sk-secret-value")]),
            None,
            None,
            None,
        )
        .expect("a key is exported");

        let printed = format!("{choice:?}");

        assert!(!printed.contains("sk-secret-value"));
        assert!(printed.contains("redacted"));
        assert!(
            printed.contains("ANTHROPIC_API_KEY"),
            "which variable answered is not the secret, and is how a caller debugs this"
        );
    }

    #[test]
    fn a_published_base_url_keeps_its_host_and_loses_its_version_suffix() {
        // The form every gateway publishes, because it is what the OpenAI
        // SDKs want. mentra's transport adds `v1/...` itself.
        assert_eq!(
            normalize_base_url("http://127.0.0.1:3455/v1").expect("normalizes"),
            "http://127.0.0.1:3455/"
        );
        assert_eq!(
            normalize_base_url("https://gateway.example.com/v1/").expect("normalizes"),
            "https://gateway.example.com/"
        );
    }

    #[test]
    fn a_base_url_without_a_version_suffix_is_left_alone() {
        assert_eq!(
            normalize_base_url("https://gateway.example.com").expect("normalizes"),
            "https://gateway.example.com/"
        );
    }

    #[test]
    fn a_path_prefix_survives_normalization() {
        // A gateway mounted under a path must keep it; only the trailing
        // version segment is ours to remove.
        assert_eq!(
            normalize_base_url("https://example.com/openai/v1").expect("normalizes"),
            "https://example.com/openai/"
        );
    }

    #[test]
    fn a_base_url_must_be_absolute_http() {
        for raw in ["127.0.0.1:3455/v1", "ftp://example.com", "", "https://"] {
            assert!(
                normalize_base_url(raw).is_err(),
                "'{raw}' must be rejected before it reaches the transport"
            );
        }
    }

    #[test]
    fn an_endpoint_is_flagged_as_compatible() {
        let choice = ProviderChoice {
            provider: BuiltinProvider::OpenAI,
            api_key: Some("k".to_string()),
            source_var: None,
            base_url: Some("http://localhost:1/".to_string()),
        };

        assert!(choice.is_compatible_endpoint());
    }
}
