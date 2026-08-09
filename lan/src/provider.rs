//! Choosing a provider and finding its credential.
//!
//! Which model answers is configuration, not a lan opinion — but *finding* the
//! credential is glue every embedder would otherwise write, so lan does it
//! once, by the environment-variable names the ecosystem already uses.

use mentra::BuiltinProvider;
use thiserror::Error;

/// A hosted provider lan can select automatically, paired with the environment
/// variable holding its key.
///
/// Order is the auto-detection preference when several keys are present.
/// Local providers (Ollama, LM Studio) are deliberately absent: they have no
/// key to detect, so selecting one is always an explicit choice.
const CANDIDATES: &[(BuiltinProvider, &str)] = &[
    (BuiltinProvider::Anthropic, "ANTHROPIC_API_KEY"),
    (BuiltinProvider::OpenAI, "OPENAI_API_KEY"),
    (BuiltinProvider::Gemini, "GEMINI_API_KEY"),
    (BuiltinProvider::OpenRouter, "OPENROUTER_API_KEY"),
];

/// Environment variables naming a custom OpenAI-compatible endpoint, in
/// preference order. `OPENAI_BASE_URL` is honored because gateways and proxies
/// already tell their users to set it.
const BASE_URL_VARS: &[&str] = &["LAN_BASE_URL", "OPENAI_BASE_URL"];

/// Environment variables holding the key for a custom endpoint.
const COMPATIBLE_KEY_VARS: &[&str] = &["LAN_API_KEY", "OPENAI_API_KEY"];

/// A provider together with the key it will authenticate with.
#[derive(Debug, Clone)]
pub struct ProviderChoice {
    pub provider: BuiltinProvider,
    pub api_key: String,
    /// The variable the key came from, or `None` when it was passed directly.
    pub source_var: Option<&'static str>,
    /// Set when the model lives behind an OpenAI-compatible endpoint rather
    /// than the provider's own service. Already normalized by
    /// [`normalize_base_url`].
    pub base_url: Option<String>,
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

    #[error("{0} has no API-key environment variable; it is a local provider")]
    NotKeyed(BuiltinProvider),

    #[error(
        "a base URL was given but no key; set one of: {}",
        COMPATIBLE_KEY_VARS.join(", ")
    )]
    NoCompatibleCredential,

    #[error("base URL must be an absolute http(s) URL, got '{0}'")]
    InvalidBaseUrl(String),
}

/// Trims a base URL to what mentra's Responses transport expects.
///
/// The transport appends `v1/responses` and `v1/models` itself, but every
/// gateway publishes its URL *with* `/v1` on the end, because that is the form
/// the OpenAI SDKs take. Pasting the published URL would otherwise produce
/// `/v1/v1/responses` and a puzzling 404, so strip a trailing `/v1` here
/// rather than making each user discover the difference.
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

/// Resolves how lan will reach a model.
///
/// A base URL — passed in, or found in the environment — wins over provider
/// auto-detection: pointing at a specific endpoint is always deliberate, so it
/// should not be silently overridden by whichever key happens to be exported.
pub fn resolve(
    requested: Option<BuiltinProvider>,
    base_url: Option<&str>,
) -> Result<ProviderChoice, ProviderError> {
    if let Some(raw) = base_url.map(str::to_string).or_else(env_base_url) {
        return resolve_compatible(&raw, requested);
    }

    match requested {
        Some(provider) => {
            let var = key_var(provider).ok_or(ProviderError::NotKeyed(provider))?;
            let api_key =
                read_key(var).ok_or(ProviderError::MissingCredential { provider, var })?;
            Ok(ProviderChoice {
                provider,
                api_key,
                source_var: Some(var),
                base_url: None,
            })
        }
        None => CANDIDATES
            .iter()
            .find_map(|(provider, var)| {
                read_key(var).map(|api_key| ProviderChoice {
                    provider: *provider,
                    api_key,
                    source_var: Some(var),
                    base_url: None,
                })
            })
            .ok_or(ProviderError::NoCredential),
    }
}

/// A custom endpoint speaks the OpenAI Responses wire format, so it is
/// registered under the OpenAI provider id unless the caller named another.
fn resolve_compatible(
    raw: &str,
    requested: Option<BuiltinProvider>,
) -> Result<ProviderChoice, ProviderError> {
    let base_url = normalize_base_url(raw)?;
    let (api_key, source_var) = COMPATIBLE_KEY_VARS
        .iter()
        .find_map(|var| read_key(var).map(|key| (key, Some(*var))))
        .ok_or(ProviderError::NoCompatibleCredential)?;

    Ok(ProviderChoice {
        provider: requested.unwrap_or(BuiltinProvider::OpenAI),
        api_key,
        source_var,
        base_url: Some(base_url),
    })
}

fn env_base_url() -> Option<String> {
    BASE_URL_VARS.iter().find_map(|var| {
        std::env::var(var)
            .ok()
            .filter(|value| !value.trim().is_empty())
    })
}

/// Treats a variable set to whitespace as absent — an empty key produces a
/// confusing authentication failure much later.
fn read_key(var: &str) -> Option<String> {
    std::env::var(var)
        .ok()
        .filter(|value| !value.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn selecting_a_local_provider_by_key_is_rejected() {
        let error = resolve(Some(BuiltinProvider::Ollama), None).expect_err("rejected");

        assert!(matches!(error, ProviderError::NotKeyed(_)));
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
            api_key: "k".to_string(),
            source_var: None,
            base_url: Some("http://localhost:1/".to_string()),
        };

        assert!(choice.is_compatible_endpoint());
    }
}
