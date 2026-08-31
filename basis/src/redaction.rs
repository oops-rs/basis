//! Shared debug redaction for environment-shaped secrets.

use std::collections::BTreeMap;

/// Keeps variable names while replacing every value with one fixed marker.
pub(crate) fn redacted_env<K>(names: impl IntoIterator<Item = K>) -> BTreeMap<K, &'static str>
where
    K: Ord,
{
    names.into_iter().map(|name| (name, "<redacted>")).collect()
}
