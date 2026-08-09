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

/// A provider together with the key it will authenticate with.
#[derive(Debug, Clone)]
pub struct ProviderChoice {
    pub provider: BuiltinProvider,
    pub api_key: String,
    /// The variable the key came from, for diagnostics.
    pub source_var: &'static str,
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

/// Resolves the provider to use: the requested one, or the first candidate
/// with a key in the environment.
pub fn resolve(requested: Option<BuiltinProvider>) -> Result<ProviderChoice, ProviderError> {
    match requested {
        Some(provider) => {
            let var = key_var(provider).ok_or(ProviderError::NotKeyed(provider))?;
            let api_key =
                read_key(var).ok_or(ProviderError::MissingCredential { provider, var })?;
            Ok(ProviderChoice {
                provider,
                api_key,
                source_var: var,
            })
        }
        None => CANDIDATES
            .iter()
            .find_map(|(provider, var)| {
                read_key(var).map(|api_key| ProviderChoice {
                    provider: *provider,
                    api_key,
                    source_var: var,
                })
            })
            .ok_or(ProviderError::NoCredential),
    }
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
        let error = resolve(Some(BuiltinProvider::Ollama)).expect_err("rejected");

        assert!(matches!(error, ProviderError::NotKeyed(_)));
    }
}
