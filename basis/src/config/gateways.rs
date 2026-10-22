//! A gateway ring said in data: the global file's `gateways` key and the
//! `BASIS_GATEWAYS` variable, one schema for both.
//!
//! Two spellings, because a ring is stated by two kinds of host. One reads
//! `.basis/config.json` through [`Config`](super::Config); the other — `dial
//! serve`, a container whose whole configuration is its environment — has only
//! variables, and `BASIS_BASE_URL` already lives there. Both are the same JSON
//! and go through [`parse`], so the file and the variable cannot come to mean
//! different things.
//!
//! ```json
//! [
//!   { "base_url": "http://10.0.0.1:2455/v1", "api_key_file": "/run/secrets/local" },
//!   { "base_url": "http://relay.example/v1", "api_key_env": "RELAY_KEY",
//!     "strip_model_prefix": "openai/" }
//! ]
//! ```
//!
//! or, to tune the ring too, `{ "members": [ … ], "failure_threshold": 3,
//! "cooldown_secs": 120 }` — `"sticky": true` in place of `cooldown_secs` for
//! a ring that never drifts back.
//!
//! A member names where its key *lives* — `api_key_env` or `api_key_file` —
//! never the key, for the reason the file has no `api_key`: a credential
//! belongs to the environment the process runs in, and a file or a variable
//! that describes the ring is printed, diffed and pasted far more often than
//! a secret should be. The key is read at build
//! ([`GatewayMember::with_api_key_env`]).

use std::time::Duration;

use serde::Deserialize;

use crate::{
    provider,
    runtime::{GatewayMember, GatewayRingPolicy},
};

/// A ring as a config file or `BASIS_GATEWAYS` stated it: the members in
/// preference order, and the policy if one was said.
///
/// `policy` is `None` when nothing about it was said, so a host that stated
/// its own with
/// [`RuntimeBuilder::with_gateway_ring_policy`](crate::RuntimeBuilder::with_gateway_ring_policy)
/// keeps it — a file only fills what nobody answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GatewayRingConfig {
    pub members: Vec<GatewayMember>,
    pub policy: Option<GatewayRingPolicy>,
}

/// The two shapes: a bare list when the defaults are fine, an object when
/// they are not.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub(crate) enum GatewaysValue {
    Members(Vec<MemberValue>),
    Ring(RingValue),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RingValue {
    members: Vec<MemberValue>,
    failure_threshold: Option<u32>,
    cooldown_secs: Option<u64>,
    #[serde(default)]
    sticky: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MemberValue {
    base_url: String,
    api_key_env: Option<String>,
    api_key_file: Option<String>,
    strip_model_prefix: Option<String>,
}

/// Turns the parsed JSON into a ring, refusing what cannot be meant.
///
/// `expand` resolves `${VAR}` for the file and is the identity for the
/// variable, whose value the shell has already expanded. A refusal is a
/// sentence about the value — the caller names where it came from.
pub(crate) fn parse(
    value: GatewaysValue,
    expand: &dyn Fn(&str) -> Result<String, String>,
) -> Result<GatewayRingConfig, String> {
    let (members, policy) = match value {
        GatewaysValue::Members(members) => (members, None),
        GatewaysValue::Ring(ring) => {
            let policy = policy(&ring)?;
            (ring.members, policy)
        }
    };
    if members.is_empty() {
        return Err("lists no gateways; remove it to reach the model the usual way".to_string());
    }
    let members = members
        .into_iter()
        .enumerate()
        .map(|(index, member)| {
            self::member(member, expand).map_err(|reason| format!("gateway #{index} {reason}"))
        })
        .collect::<Result<_, _>>()?;
    Ok(GatewayRingConfig { members, policy })
}

fn policy(ring: &RingValue) -> Result<Option<GatewayRingPolicy>, String> {
    if ring.failure_threshold.is_none() && ring.cooldown_secs.is_none() && !ring.sticky {
        return Ok(None);
    }
    // `GatewayRingPolicy` reads 0 as 1; a file that says 0 meant something
    // else, and says so here rather than rotating on the first failure.
    if ring.failure_threshold == Some(0) {
        return Err("sets `failure_threshold` to 0; the smallest that means anything is 1".into());
    }
    let cooldown = match (ring.sticky, ring.cooldown_secs) {
        (true, Some(_)) => {
            return Err(
                "sets both `sticky` and `cooldown_secs`; a sticky ring never \
                        returns, so it has no cooldown to wait"
                    .into(),
            );
        }
        (true, None) => None,
        (false, Some(secs)) => Some(Duration::from_secs(secs)),
        (false, None) => Some(GatewayRingPolicy::DEFAULT_COOLDOWN),
    };
    Ok(Some(GatewayRingPolicy {
        failure_threshold: ring
            .failure_threshold
            .unwrap_or(GatewayRingPolicy::DEFAULT_FAILURE_THRESHOLD),
        cooldown,
    }))
}

fn member(
    value: MemberValue,
    expand: &dyn Fn(&str) -> Result<String, String>,
) -> Result<GatewayMember, String> {
    let base_url = expand(&value.base_url)?;
    // Normalized here so a typo names the member; the builder normalizes
    // again, which is idempotent.
    provider::normalize_base_url(&base_url).map_err(|error| error.to_string())?;
    let mut member = GatewayMember::new(base_url);

    member = match (value.api_key_env, value.api_key_file) {
        (Some(_), Some(_)) => {
            return Err("names both `api_key_env` and `api_key_file`; a member has one key".into());
        }
        (Some(var), None) => {
            let var = var.trim().to_string();
            if var.is_empty() {
                return Err("has an empty `api_key_env`".into());
            }
            member.with_api_key_env(var)
        }
        (None, Some(path)) => {
            let path = expand(&path)?;
            if path.trim().is_empty() {
                return Err("has an empty `api_key_file`".into());
            }
            member.with_api_key_file(path.trim())
        }
        (None, None) => member,
    };

    if let Some(prefix) = value.strip_model_prefix {
        if prefix.is_empty() {
            return Err("has an empty `strip_model_prefix`".into());
        }
        member = member.with_model_prefix_stripped(prefix);
    }
    Ok(member)
}

/// `BASIS_GATEWAYS`, read: the same JSON, with no `${VAR}` expansion — the
/// shell that set the variable has already expanded what it meant to.
///
/// A malformed value is described by category and position, as a malformed
/// file is, rather than by serde's own message, which quotes what it choked
/// on.
pub(crate) fn parse_var(raw: &str) -> Result<GatewayRingConfig, String> {
    let value: GatewaysValue = serde_json::from_str(raw).map_err(|error| {
        let problem = match error.classify() {
            serde_json::error::Category::Data => {
                "is not a list of gateways or a ring of them (an unknown key, or a value of the wrong type)"
            }
            _ => "is not valid JSON",
        };
        format!("{problem} at column {}", error.column())
    })?;
    parse(value, &|raw| Ok(raw.to_string()))
}
