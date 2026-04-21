//! Domain types for the memory store.
//!
//! Defines the fixed core memory ontology and transport types used by the
//! Vault-as-Truth architecture. See:
//! - `docs/design/vault-as-truth.md`
//! - `docs/design/entity-type-and-schema-kind.md`

use serde::{Deserialize, Serialize};
pub use symbiotic_core::MemorySpace;
use thiserror::Error;

// --- Fact Type ---

/// Classification of an extracted fact.
///
/// Maps to the six types from the Distillery's Classify stage:
/// Decision, Finding, Preference, Entity, Episode, Methodology.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum FactType {
    /// User chose X over Y.
    Decision,
    /// Discovered / learned X.
    Finding,
    /// User prefers X.
    Preference,
    /// Person / project / tool reference.
    Entity,
    /// Event that happened.
    Episode,
    /// How user does things.
    Methodology,
}

impl FactType {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Decision => "decision",
            Self::Finding => "finding",
            Self::Preference => "preference",
            Self::Entity => "entity",
            Self::Episode => "episode",
            Self::Methodology => "methodology",
        }
    }
}

impl std::fmt::Display for FactType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for FactType {
    type Err = MemoryStoreError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "decision" => Ok(Self::Decision),
            "finding" => Ok(Self::Finding),
            "preference" => Ok(Self::Preference),
            "entity" => Ok(Self::Entity),
            "episode" => Ok(Self::Episode),
            "methodology" => Ok(Self::Methodology),
            _ => Err(MemoryStoreError::Database(format!(
                "unknown fact type: {s}"
            ))),
        }
    }
}

// --- Entity ---

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entity {
    pub id: String,
    pub entity_type: EntityType,
    pub name: String,
    pub attributes: serde_json::Value,
    pub sensitivity: Sensitivity,
    pub allowed_models: AllowedModels,
    /// Which memory space this entity belongs to.
    /// Defaults to `Knowledge` for backward compatibility.
    #[serde(default = "default_space")]
    pub space: MemorySpace,
    pub status: EntityStatus,
    pub merged_into: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

fn default_space() -> MemorySpace {
    MemorySpace::Knowledge
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EntityType {
    Person,
    Project,
    Org,
    Tool,
    Preference,
    Concept,
    Task,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EntityTypeMetadata {
    pub storage_name: &'static str,
    pub frontmatter_name: &'static str,
    pub singular_label: &'static str,
    pub plural_dir: &'static str,
}

impl EntityType {
    pub const ALL: [Self; 7] = [
        Self::Person,
        Self::Project,
        Self::Org,
        Self::Tool,
        Self::Preference,
        Self::Concept,
        Self::Task,
    ];

    pub const fn metadata(&self) -> EntityTypeMetadata {
        match self {
            Self::Person => EntityTypeMetadata {
                storage_name: "person",
                frontmatter_name: "person",
                singular_label: "person",
                plural_dir: "people",
            },
            Self::Project => EntityTypeMetadata {
                storage_name: "project",
                frontmatter_name: "project",
                singular_label: "project",
                plural_dir: "projects",
            },
            Self::Org => EntityTypeMetadata {
                storage_name: "org",
                frontmatter_name: "organization",
                singular_label: "organization",
                plural_dir: "organizations",
            },
            Self::Tool => EntityTypeMetadata {
                storage_name: "tool",
                frontmatter_name: "tool",
                singular_label: "tool",
                plural_dir: "tools",
            },
            Self::Preference => EntityTypeMetadata {
                storage_name: "preference",
                frontmatter_name: "preference",
                singular_label: "preference",
                plural_dir: "preferences",
            },
            Self::Concept => EntityTypeMetadata {
                storage_name: "concept",
                frontmatter_name: "concept",
                singular_label: "concept",
                plural_dir: "concepts",
            },
            Self::Task => EntityTypeMetadata {
                storage_name: "task",
                frontmatter_name: "task",
                singular_label: "task",
                plural_dir: "tasks",
            },
        }
    }

    pub fn as_str(&self) -> &'static str {
        self.metadata().storage_name
    }

    pub fn frontmatter_str(&self) -> &'static str {
        self.metadata().frontmatter_name
    }

    pub fn singular_label(&self) -> &'static str {
        self.metadata().singular_label
    }

    pub fn plural_dir(&self) -> &'static str {
        self.metadata().plural_dir
    }
}

impl std::fmt::Display for EntityType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for EntityType {
    type Err = MemoryStoreError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "person" => Ok(Self::Person),
            "project" => Ok(Self::Project),
            "org" => Ok(Self::Org),
            "tool" => Ok(Self::Tool),
            "preference" => Ok(Self::Preference),
            "concept" => Ok(Self::Concept),
            "task" => Ok(Self::Task),
            _ => Err(MemoryStoreError::Database(format!(
                "unknown entity type: {s}"
            ))),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum Sensitivity {
    Shareable,
    Restricted,
    Private,
}

impl Sensitivity {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Shareable => "shareable",
            Self::Restricted => "restricted",
            Self::Private => "private",
        }
    }
}

impl std::str::FromStr for Sensitivity {
    type Err = MemoryStoreError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "shareable" => Ok(Self::Shareable),
            "restricted" => Ok(Self::Restricted),
            "private" => Ok(Self::Private),
            _ => Err(MemoryStoreError::Database(format!(
                "unknown sensitivity: {s}"
            ))),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AllowedModels {
    LocalOnly,
    Hybrid,
    Any,
}

impl AllowedModels {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::LocalOnly => "local_only",
            Self::Hybrid => "hybrid",
            Self::Any => "any",
        }
    }
}

impl std::str::FromStr for AllowedModels {
    type Err = MemoryStoreError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "local_only" => Ok(Self::LocalOnly),
            "hybrid" => Ok(Self::Hybrid),
            "any" => Ok(Self::Any),
            _ => Err(MemoryStoreError::Database(format!(
                "unknown allowed_models: {s}"
            ))),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EntityStatus {
    Active,
    Merged,
    Archived,
}

impl EntityStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Merged => "merged",
            Self::Archived => "archived",
        }
    }
}

impl std::str::FromStr for EntityStatus {
    type Err = MemoryStoreError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "active" => Ok(Self::Active),
            "merged" => Ok(Self::Merged),
            "archived" => Ok(Self::Archived),
            _ => Err(MemoryStoreError::Database(format!(
                "unknown entity status: {s}"
            ))),
        }
    }
}

// --- Relationship ---

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Relationship {
    pub id: String,
    pub from_entity: String,
    pub to_entity: String,
    pub relation_type: String,
    pub strength: f64,
    pub valid_from: String,
    pub valid_to: Option<String>,
    pub sensitivity: Sensitivity,
    pub allowed_models: AllowedModels,
    pub status: RelationshipStatus,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RelationshipStatus {
    Active,
    Superseded,
    Expired,
}

impl RelationshipStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Superseded => "superseded",
            Self::Expired => "expired",
        }
    }
}

impl std::str::FromStr for RelationshipStatus {
    type Err = MemoryStoreError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "active" => Ok(Self::Active),
            "superseded" => Ok(Self::Superseded),
            "expired" => Ok(Self::Expired),
            _ => Err(MemoryStoreError::Database(format!(
                "unknown relationship status: {s}"
            ))),
        }
    }
}

// --- Memory ---

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Memory {
    pub id: String,
    pub entity_id: String,
    pub fact: String,
    pub confidence: f64,
    pub disposition: FactDisposition,
    pub sensitivity: Sensitivity,
    pub valid_from: String,
    pub valid_to: Option<String>,
    pub status: MemoryStatus,
    pub superseded_by: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    /// Typed classification of this fact (Phase 2: Typed Fact Classification).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fact_type: Option<FactType>,
    /// Who authored this fact: "user", "agent", "distillery", etc.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authored_by: Option<String>,
    /// ID of the memory this supersedes (forward link in superseding chain).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supersedes: Option<String>,
    /// IDs of memories this fact depends on (for cascading staleness).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub depends_on: Vec<String>,
    /// Optional FSRS state for adaptive recall decay.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fsrs: Option<FsrsState>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FsrsState {
    pub stability: f64,
    pub difficulty: f64,
    pub last_access: u64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FactDisposition {
    AutoStored,
    ReviewFlagged,
    UserConfirmed,
}

impl FactDisposition {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::AutoStored => "auto_stored",
            Self::ReviewFlagged => "review_flagged",
            Self::UserConfirmed => "user_confirmed",
        }
    }
}

impl std::str::FromStr for FactDisposition {
    type Err = MemoryStoreError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "auto_stored" => Ok(Self::AutoStored),
            "review_flagged" => Ok(Self::ReviewFlagged),
            "user_confirmed" => Ok(Self::UserConfirmed),
            _ => Err(MemoryStoreError::Database(format!(
                "unknown fact disposition: {s}"
            ))),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MemoryStatus {
    Active,
    Superseded,
    Expired,
    Archived,
}

impl MemoryStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Superseded => "superseded",
            Self::Expired => "expired",
            Self::Archived => "archived",
        }
    }
}

impl std::str::FromStr for MemoryStatus {
    type Err = MemoryStoreError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "active" => Ok(Self::Active),
            "superseded" => Ok(Self::Superseded),
            "expired" => Ok(Self::Expired),
            "archived" => Ok(Self::Archived),
            _ => Err(MemoryStoreError::Database(format!(
                "unknown memory status: {s}"
            ))),
        }
    }
}

// --- Evidence ---

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Evidence {
    pub id: String,
    pub memory_id: Option<String>,
    pub relationship_id: Option<String>,
    pub entity_id: Option<String>,
    pub article_id: Option<String>,
    pub source_url: Option<String>,
    pub evidence_quote: Option<String>,
    pub observed_at: String,
    pub created_at: String,
}

// --- Memory Space ---
//
// `MemorySpace` is defined in `symbiotic-core` and re-exported via
// `use symbiotic_core::MemorySpace` at the top of this file.

// --- Recall result ---

/// A recall result combining entity, memory, and evidence for context delivery.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryRecallResult {
    pub entity: Entity,
    pub memories: Vec<Memory>,
    pub evidence: Vec<Evidence>,
    pub relevance_score: f64,
}

// --- Deduplication config ---

pub struct DeduplicationConfig {
    /// Maximum edit distance for fuzzy name matching.
    pub max_edit_distance: usize,
    /// Whether to auto-merge exact matches or require review.
    pub auto_merge_exact: bool,
}

impl Default for DeduplicationConfig {
    fn default() -> Self {
        Self {
            max_edit_distance: 2,
            auto_merge_exact: true,
        }
    }
}

// --- Error ---

#[derive(Debug, Error)]
pub enum MemoryStoreError {
    #[error("entity not found: {0}")]
    EntityNotFound(String),
    #[error("memory not found: {0}")]
    MemoryNotFound(String),
    #[error("relationship not found: {0}")]
    RelationshipNotFound(String),
    #[error("evidence required: memory must have at least one evidence link")]
    EvidenceRequired,
    #[error("duplicate entity: {name} (type: {entity_type}) already exists as {existing_id}")]
    DuplicateEntity {
        name: String,
        entity_type: EntityType,
        existing_id: String,
    },
    #[error("database error: {0}")]
    Database(String),
    #[error("encryption error: {0}")]
    Encryption(String),
    #[error("invalid sensitivity transition: {from:?} -> {to:?}")]
    InvalidSensitivityChange { from: Sensitivity, to: Sensitivity },
}

impl From<rusqlite::Error> for MemoryStoreError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Database(e.to_string())
    }
}

// --- Wikilink types ---

/// A parsed wikilink reference found in note body text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WikilinkRef {
    /// The raw target text inside [[...]].
    pub target: String,
    /// Normalized target for matching (lowercase, kebab-case).
    pub target_normalized: String,
    /// Optional display text after | separator.
    pub display_text: Option<String>,
    /// Surrounding context snippet.
    pub context: String,
}

/// A stored link between two entities.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntityLink {
    pub id: String,
    pub source_entity_id: String,
    pub target_entity_id: String,
    pub link_text: String,
    pub context: String,
    pub created_at: String,
}

// --- Extraction types ---

/// A single fact extracted from source text by the LLM.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractedFact {
    pub entity_type: EntityType,
    pub entity_name: String,
    pub fact: String,
    pub confidence: f64,
    pub evidence_quote: String,
    pub temporal_hint: Option<String>,
    /// Classified fact type (Phase 2: Typed Fact Classification).
    /// Populated during extraction or by a subsequent classification step.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fact_type: Option<FactType>,
}

/// Complete extraction result from a single input.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractionResult {
    pub facts: Vec<ExtractedFact>,
    /// Approximate input token count (if available from the LLM response).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    /// Approximate output token count (if available from the LLM response).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
}

/// Input to the extraction process.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractionInput {
    /// The text to extract facts from.
    pub text: String,
    /// Source document ID for evidence linking.
    pub source_id: String,
    /// Source URL if available.
    pub source_url: Option<String>,
}

/// Errors that can occur during extraction.
#[derive(Debug, Error)]
pub enum ExtractionError {
    #[error("ollama unavailable: {0}")]
    OllamaUnavailable(String),
    #[error("extraction produced invalid JSON: {0}")]
    InvalidOutput(String),
    #[error("extraction timed out after {0}ms")]
    Timeout(u64),
    #[error("input too large: {tokens} tokens exceeds max {max}")]
    InputTooLarge { tokens: usize, max: usize },
}

/// Result of validating a fact against its source text.
#[derive(Debug, Clone)]
pub struct GroundingResult {
    pub fact: ExtractedFact,
    pub grounded: bool,
    /// Similarity between evidence_quote and nearest span in source text.
    pub match_score: f64,
}

/// Configurable confidence thresholds for fact disposition routing.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct ConfidenceThresholds {
    /// Above this: auto-store (if grounded).
    pub auto_accept: f64,
    /// Between drop and auto_accept: store with review flag.
    pub review: f64,
    /// Below this: drop or ask user.
    pub drop: f64,
}

impl Default for ConfidenceThresholds {
    fn default() -> Self {
        Self {
            auto_accept: 0.7,
            review: 0.4,
            drop: 0.4,
        }
    }
}

/// Routing decision for an extracted fact before it reaches the store.
///
/// Separate from `FactDisposition` which represents the stored state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtractionDisposition {
    /// Automatically stored (high confidence + grounded).
    AutoStore,
    /// Stored but flagged for user review.
    ReviewFlag,
    /// Dropped due to low confidence or failed grounding.
    Drop,
    /// Awaiting user confirmation (very low confidence).
    PendingConfirmation,
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- FactType serialization/deserialization ---

    #[test]
    fn fact_type_serializes_to_snake_case() {
        let json = serde_json::to_string(&FactType::Decision).unwrap();
        assert_eq!(json, "\"decision\"");

        let json = serde_json::to_string(&FactType::Finding).unwrap();
        assert_eq!(json, "\"finding\"");

        let json = serde_json::to_string(&FactType::Preference).unwrap();
        assert_eq!(json, "\"preference\"");

        let json = serde_json::to_string(&FactType::Entity).unwrap();
        assert_eq!(json, "\"entity\"");

        let json = serde_json::to_string(&FactType::Episode).unwrap();
        assert_eq!(json, "\"episode\"");

        let json = serde_json::to_string(&FactType::Methodology).unwrap();
        assert_eq!(json, "\"methodology\"");
    }

    #[test]
    fn fact_type_deserializes_from_snake_case() {
        let dt: FactType = serde_json::from_str("\"decision\"").unwrap();
        assert_eq!(dt, FactType::Decision);

        let dt: FactType = serde_json::from_str("\"finding\"").unwrap();
        assert_eq!(dt, FactType::Finding);

        let dt: FactType = serde_json::from_str("\"preference\"").unwrap();
        assert_eq!(dt, FactType::Preference);

        let dt: FactType = serde_json::from_str("\"entity\"").unwrap();
        assert_eq!(dt, FactType::Entity);

        let dt: FactType = serde_json::from_str("\"episode\"").unwrap();
        assert_eq!(dt, FactType::Episode);

        let dt: FactType = serde_json::from_str("\"methodology\"").unwrap();
        assert_eq!(dt, FactType::Methodology);
    }

    #[test]
    fn fact_type_roundtrip() {
        let variants = vec![
            FactType::Decision,
            FactType::Finding,
            FactType::Preference,
            FactType::Entity,
            FactType::Episode,
            FactType::Methodology,
        ];
        for v in variants {
            let json = serde_json::to_string(&v).unwrap();
            let back: FactType = serde_json::from_str(&json).unwrap();
            assert_eq!(v, back);
        }
    }

    #[test]
    fn fact_type_from_str() {
        assert_eq!("decision".parse::<FactType>().unwrap(), FactType::Decision);
        assert_eq!("finding".parse::<FactType>().unwrap(), FactType::Finding);
        assert_eq!(
            "preference".parse::<FactType>().unwrap(),
            FactType::Preference
        );
        assert_eq!("entity".parse::<FactType>().unwrap(), FactType::Entity);
        assert_eq!("episode".parse::<FactType>().unwrap(), FactType::Episode);
        assert_eq!(
            "methodology".parse::<FactType>().unwrap(),
            FactType::Methodology
        );
        assert!("unknown".parse::<FactType>().is_err());
    }

    #[test]
    fn fact_type_display() {
        assert_eq!(FactType::Decision.to_string(), "decision");
        assert_eq!(FactType::Finding.to_string(), "finding");
        assert_eq!(FactType::Methodology.to_string(), "methodology");
    }

    #[test]
    fn entity_type_metadata_distinguishes_storage_from_frontmatter() {
        assert_eq!(EntityType::Person.as_str(), "person");
        assert_eq!(EntityType::Person.frontmatter_str(), "person");
        assert_eq!(EntityType::Person.plural_dir(), "people");

        assert_eq!(EntityType::Org.as_str(), "org");
        assert_eq!(EntityType::Org.frontmatter_str(), "organization");
        assert_eq!(EntityType::Org.singular_label(), "organization");
        assert_eq!(EntityType::Org.plural_dir(), "organizations");

        assert_eq!(EntityType::Tool.as_str(), "tool");
        assert_eq!(EntityType::Tool.frontmatter_str(), "tool");
        assert_eq!(EntityType::Tool.plural_dir(), "tools");

        assert_eq!(EntityType::ALL.len(), 7);
        assert_eq!(EntityType::ALL[0], EntityType::Person);
        assert_eq!(EntityType::ALL[6], EntityType::Task);
    }

    // --- Memory with new fields serialization ---

    #[test]
    fn memory_new_fields_optional_in_json() {
        // Deserialize a Memory JSON without the new fields — should default to None/empty
        let json = serde_json::json!({
            "id": "mem-1",
            "entity_id": "ent-1",
            "fact": "Test fact",
            "confidence": 0.9,
            "disposition": "auto_stored",
            "sensitivity": "private",
            "valid_from": "2026-01-01T00:00:00Z",
            "valid_to": null,
            "status": "active",
            "superseded_by": null,
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:00:00Z"
        });
        let mem: Memory = serde_json::from_value(json).unwrap();
        assert!(mem.fact_type.is_none());
        assert!(mem.authored_by.is_none());
        assert!(mem.supersedes.is_none());
        assert!(mem.depends_on.is_empty());
    }

    #[test]
    fn memory_new_fields_roundtrip() {
        let mem = Memory {
            id: "mem-1".to_string(),
            entity_id: "ent-1".to_string(),
            fact: "Test fact".to_string(),
            confidence: 0.9,
            disposition: FactDisposition::AutoStored,
            sensitivity: Sensitivity::Private,
            valid_from: "2026-01-01T00:00:00Z".to_string(),
            valid_to: None,
            status: MemoryStatus::Active,
            superseded_by: None,
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
            fact_type: Some(FactType::Decision),
            authored_by: Some("user".to_string()),
            supersedes: Some("mem-0".to_string()),
            depends_on: vec!["dep-1".to_string(), "dep-2".to_string()],
            fsrs: None,
        };

        let json = serde_json::to_string(&mem).unwrap();
        let back: Memory = serde_json::from_str(&json).unwrap();

        assert_eq!(back.fact_type, Some(FactType::Decision));
        assert_eq!(back.authored_by, Some("user".to_string()));
        assert_eq!(back.supersedes, Some("mem-0".to_string()));
        assert_eq!(
            back.depends_on,
            vec!["dep-1".to_string(), "dep-2".to_string()]
        );
    }

    // --- ExtractedFact with fact_type ---

    #[test]
    fn extracted_fact_with_fact_type_roundtrip() {
        let fact = ExtractedFact {
            entity_type: EntityType::Person,
            entity_name: "Alice".to_string(),
            fact: "Works at Acme".to_string(),
            confidence: 0.9,
            evidence_quote: "Alice works at Acme".to_string(),
            temporal_hint: None,
            fact_type: Some(FactType::Finding),
        };

        let json = serde_json::to_string(&fact).unwrap();
        let back: ExtractedFact = serde_json::from_str(&json).unwrap();
        assert_eq!(back.fact_type, Some(FactType::Finding));
    }

    #[test]
    fn extracted_fact_without_fact_type_defaults_to_none() {
        let json = serde_json::json!({
            "entity_type": "person",
            "entity_name": "Alice",
            "fact": "Works at Acme",
            "confidence": 0.9,
            "evidence_quote": "Alice works at Acme",
            "temporal_hint": null
        });
        let fact: ExtractedFact = serde_json::from_value(json).unwrap();
        assert!(fact.fact_type.is_none());
    }
}
