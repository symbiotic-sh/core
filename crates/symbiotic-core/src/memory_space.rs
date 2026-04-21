//! Canonical definition of the three memory spaces from Architecture 2.0.
//!
//! This is the single source of truth for [`MemorySpace`]. All crates
//! (`symbiotic-memory`, `symbiotic-intake`, etc.) import from here.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The three memory spaces from Architecture 2.0.
///
/// Each space represents a distinct category of knowledge and maps to a
/// canonical subdirectory under `kb_root`:
/// - `Knowledge` -> `knowledge/` (Semantic -- facts, entities, claims, external knowledge)
/// - `Identity` -> `identity/` (Identity -- preferences, identity, personal history)
/// - `Operations` -> `operations/` (Procedural / active work -- goals, skills, workflows, reports)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemorySpace {
    /// Semantic memory: atomic notes, facts, entities, wiki-linked concepts.
    /// What the system *knows*.
    #[serde(rename = "knowledge")]
    Knowledge,
    /// Identity memory: agent identity, user preferences, calibrated confidence.
    /// Who the system *is* and whom it serves.
    #[serde(rename = "identity")]
    Identity,
    /// Operations memory: active tasks, friction logs, handoffs, skills.
    /// What the system is pursuing and how it acts.
    #[serde(rename = "operations")]
    Operations,
}

/// Error returned when parsing an unknown memory space string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemorySpaceParseError(pub String);

impl std::fmt::Display for MemorySpaceParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "unknown memory space: {}", self.0)
    }
}

impl std::error::Error for MemorySpaceParseError {}

impl MemorySpace {
    /// Returns the lowercase string representation of this space.
    ///
    /// Identical to [`dir_name`](Self::dir_name) -- provided for ergonomic use
    /// in contexts that don't deal with filesystem paths.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Knowledge => "knowledge",
            Self::Identity => "identity",
            Self::Operations => "operations",
        }
    }

    /// Returns the subdirectory name for this space under `kb_root`.
    pub fn dir_name(&self) -> &'static str {
        self.as_str()
    }

    /// Returns the full path for this space: `kb_root/{dir_name}`.
    pub fn path(&self, kb_root: &Path) -> PathBuf {
        kb_root.join(self.dir_name())
    }

    /// All three memory spaces.
    pub fn all() -> &'static [MemorySpace] {
        &[Self::Knowledge, Self::Identity, Self::Operations]
    }
}

impl std::fmt::Display for MemorySpace {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for MemorySpace {
    type Err = MemorySpaceParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "knowledge" => Ok(Self::Knowledge),
            "identity" => Ok(Self::Identity),
            "operations" => Ok(Self::Operations),
            _ => Err(MemorySpaceParseError(s.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn as_str_values() {
        assert_eq!(MemorySpace::Knowledge.as_str(), "knowledge");
        assert_eq!(MemorySpace::Identity.as_str(), "identity");
        assert_eq!(MemorySpace::Operations.as_str(), "operations");
    }

    #[test]
    fn dir_name_matches_as_str() {
        for space in MemorySpace::all() {
            assert_eq!(space.as_str(), space.dir_name());
        }
    }

    #[test]
    fn path_joins_correctly() {
        let root = Path::new("/kb");
        assert_eq!(
            MemorySpace::Knowledge.path(root),
            PathBuf::from("/kb/knowledge")
        );
        assert_eq!(
            MemorySpace::Identity.path(root),
            PathBuf::from("/kb/identity")
        );
        assert_eq!(
            MemorySpace::Operations.path(root),
            PathBuf::from("/kb/operations")
        );
    }

    #[test]
    fn all_returns_three() {
        assert_eq!(MemorySpace::all().len(), 3);
    }

    #[test]
    fn display_matches_as_str() {
        for space in MemorySpace::all() {
            assert_eq!(format!("{space}"), space.as_str());
        }
    }

    #[test]
    fn from_str_valid() {
        assert_eq!(
            "knowledge".parse::<MemorySpace>().unwrap(),
            MemorySpace::Knowledge
        );
        assert_eq!(
            "identity".parse::<MemorySpace>().unwrap(),
            MemorySpace::Identity
        );
        assert_eq!(
            "operations".parse::<MemorySpace>().unwrap(),
            MemorySpace::Operations
        );
    }

    #[test]
    fn from_str_invalid() {
        let err = "bogus".parse::<MemorySpace>().unwrap_err();
        assert_eq!(err.to_string(), "unknown memory space: bogus");
    }

    #[test]
    fn serde_round_trip() {
        for space in MemorySpace::all() {
            let json = serde_json::to_string(space).expect("serialize");
            let parsed: MemorySpace = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(*space, parsed);
        }
    }

    #[test]
    fn serde_values() {
        assert_eq!(
            serde_json::to_string(&MemorySpace::Knowledge).unwrap(),
            "\"knowledge\""
        );
        assert_eq!(
            serde_json::to_string(&MemorySpace::Identity).unwrap(),
            "\"identity\""
        );
        assert_eq!(
            serde_json::to_string(&MemorySpace::Operations).unwrap(),
            "\"operations\""
        );
    }
}
