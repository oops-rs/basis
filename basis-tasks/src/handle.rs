//! The opaque `<workspace-key>/<uuid>` grammar a task is named by.
//!
//! Opaque deliberately: the two halves are an FNV-1a digest of the canonical
//! workspace path and a v4 UUID, and nothing outside [`data_dir`](super::data_dir)
//! should parse them apart. A handle is a capability — knowing one is what
//! lets a caller act on the task it names — not a path to be built by hand.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::{Error, data_dir::valid_task_handle};

/// A durable task's handle: `<16 lowercase hex>/<32 lowercase hex>`.
///
/// Never becomes a filesystem path outside the data directory root — every
/// place one is turned into a path validates the grammar first
/// ([`DataDir::agent_dir`](crate::DataDir::agent_dir)) — and is stable for the
/// task's whole life: `spawn` mints it once, and every other verb takes it
/// back unchanged.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TaskHandle(String);

impl TaskHandle {
    /// Parses a handle, refusing anything that does not fit the grammar.
    ///
    /// The refusal is deliberately generic — "not a task handle" rather than
    /// a byte-by-byte diagnosis — because the grammar is opaque by design:
    /// there is nothing more specific a caller should learn about *why* a
    /// string is not one.
    pub fn parse(handle: impl Into<String>) -> Result<Self, Error> {
        let handle = handle.into();
        if valid_task_handle(&handle).is_some() {
            Ok(Self(handle))
        } else {
            Err(Error::new(format!("`{handle}` is not a task handle")))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The workspace-key half: which workspace's `agents/` directory this
    /// task lives under.
    pub(crate) fn key(&self) -> &str {
        valid_task_handle(&self.0)
            .expect("a TaskHandle is only ever constructed from a valid grammar")
            .0
    }
}

impl fmt::Display for TaskHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::str::FromStr for TaskHandle {
    type Err = Error;

    fn from_str(handle: &str) -> Result<Self, Self::Err> {
        Self::parse(handle)
    }
}

impl AsRef<str> for TaskHandle {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl Serialize for TaskHandle {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for TaskHandle {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let handle = String::deserialize(deserializer)?;
        Self::parse(handle).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_well_formed_handle_round_trips() {
        let text = format!("0123456789abcdef/{:032x}", 1);
        let handle = TaskHandle::parse(text.clone()).expect("well-formed");
        assert_eq!(handle.as_str(), text);
        assert_eq!(handle.to_string(), text);
        assert_eq!(handle.key(), "0123456789abcdef");
    }

    #[test]
    fn a_malformed_handle_is_refused_generically() {
        for bad in ["not-a-handle", "0123456789abcdef", "../../etc/passwd"] {
            let error = TaskHandle::parse(bad).expect_err("refused");
            assert!(error.to_string().contains("not a task handle"), "{error}");
        }
    }

    #[test]
    fn json_round_trips_as_a_bare_string() {
        let text = format!("0123456789abcdef/{:032x}", 2);
        let handle = TaskHandle::parse(text.clone()).unwrap();
        let json = serde_json::to_string(&handle).unwrap();
        assert_eq!(json, format!("\"{text}\""));
        let back: TaskHandle = serde_json::from_str(&json).unwrap();
        assert_eq!(back, handle);
    }
}
