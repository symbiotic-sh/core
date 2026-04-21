//! Migration tool: export SQLite entities/memories/relationships to Markdown entity files.
//!
//! Enables switching from SQLite-as-truth to Markdown-as-truth by generating
//! `.md` files from existing database content. Supports round-trip verification.
//!
//! See `docs/design/vault-as-truth.md` § Migration Path.

use std::path::Path;

use crate::store::MemoryStore;
use crate::vault_layout::canonical_entity_relative_path;
use crate::vault_parser;
use crate::{Entity, Memory, MemoryStatus, Relationship, SqliteMemoryStore};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Statistics from a migration operation.
#[derive(Debug, Clone, Default)]
pub struct MigrationStats {
    pub entities_exported: usize,
    pub memories_exported: usize,
    pub relationships_exported: usize,
    pub files_written: usize,
    pub files_skipped: usize,
}

/// Errors during migration.
#[derive(Debug, thiserror::Error)]
pub enum MigrationError {
    #[error("store error: {0}")]
    Store(String),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("round-trip verification failed for {entity_id}: {detail}")]
    VerificationFailed { entity_id: String, detail: String },
}

/// Controls migration behavior.
#[derive(Debug, Clone)]
pub struct MigrationOptions {
    /// If true, skip files that already exist in the vault.
    pub skip_existing: bool,
    /// If true, verify round-trip fidelity after writing each file.
    pub verify: bool,
}

impl Default for MigrationOptions {
    fn default() -> Self {
        Self {
            skip_existing: true,
            verify: true,
        }
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Export all entities from SQLite to Markdown files in the vault.
pub async fn export_to_vault(
    store: &SqliteMemoryStore,
    vault_root: &Path,
    options: &MigrationOptions,
) -> Result<MigrationStats, MigrationError> {
    let mut stats = MigrationStats::default();

    // Get all entities from all spaces
    for space in crate::MemorySpace::all() {
        let entities = store
            .find_entities_by_space(*space, 10_000)
            .await
            .map_err(|e| MigrationError::Store(e.to_string()))?;

        for entity in &entities {
            let rel_path =
                canonical_entity_relative_path(&entity.id, entity.entity_type, entity.space);
            let file_path = vault_root.join(&rel_path);
            if let Some(dir) = file_path.parent() {
                std::fs::create_dir_all(dir)?;
            }

            if options.skip_existing && file_path.exists() {
                stats.files_skipped += 1;
                continue;
            }

            // Get memories and relationships for this entity
            let memories = store
                .get_memories(&entity.id, None)
                .await
                .map_err(|e| MigrationError::Store(e.to_string()))?;

            let relationships = store
                .get_relationships(&entity.id)
                .await
                .map_err(|e| MigrationError::Store(e.to_string()))?;

            // Only include outgoing relationships (from_entity == entity.id)
            let outgoing: Vec<&Relationship> = relationships
                .iter()
                .filter(|r| r.from_entity == entity.id)
                .collect();

            // Generate Markdown content
            let content = generate_entity_markdown(entity, &memories, &outgoing);

            std::fs::write(&file_path, &content)?;
            stats.files_written += 1;
            stats.entities_exported += 1;
            stats.memories_exported += memories.len();
            stats.relationships_exported += outgoing.len();

            // Verify round-trip
            if options.verify {
                verify_roundtrip(entity, &memories, &outgoing, &content)?;
            }
        }
    }

    Ok(stats)
}

// ---------------------------------------------------------------------------
// Markdown generation
// ---------------------------------------------------------------------------

/// Generate a complete entity Markdown file from structured data.
pub fn generate_entity_markdown(
    entity: &Entity,
    memories: &[Memory],
    relationships: &[&Relationship],
) -> String {
    let mut content = String::new();

    // Frontmatter
    content.push_str("---\n");
    content.push_str(&format!("id: {}\n", entity.id));
    content.push_str(&format!("type: {}\n", entity.entity_type.frontmatter_str()));

    // Aliases
    if let Some(aliases) = entity.attributes.get("aliases") {
        if let Some(arr) = aliases.as_array() {
            let alias_strs: Vec<&str> = arr.iter().filter_map(|v| v.as_str()).collect();
            if !alias_strs.is_empty() {
                content.push_str(&format!("aliases: [{}]\n", alias_strs.join(", ")));
            }
        }
    }

    if let Some(schema) = entity.attributes.get("schema").and_then(|v| v.as_str()) {
        content.push_str(&format!("schema: {}\n", schema));
    }

    content.push_str(&format!("space: {}\n", entity.space.as_str()));
    content.push_str(&format!("sensitivity: {}\n", entity.sensitivity.as_str()));
    content.push_str(&format!("created: {}\n", entity.created_at));
    content.push_str(&format!("updated: {}\n", entity.updated_at));
    content.push_str("---\n\n");

    // Heading
    content.push_str(&format!("# {}\n\n", entity.name));

    // ## Facts
    content.push_str("## Facts\n");
    let active: Vec<&Memory> = memories
        .iter()
        .filter(|m| m.status == MemoryStatus::Active)
        .collect();
    for mem in &active {
        content.push_str(&format_memory_line(mem));
        content.push('\n');
    }
    content.push('\n');

    // ## Relationships
    content.push_str("## Relationships\n");
    for rel in relationships {
        content.push_str(&format_relationship_line(rel));
        content.push('\n');
    }
    content.push('\n');

    // ## History
    content.push_str("## History\n");
    let archived: Vec<&Memory> = memories
        .iter()
        .filter(|m| m.status == MemoryStatus::Archived)
        .collect();
    if !archived.is_empty() {
        content.push_str("### Archived Facts\n");
        for mem in &archived {
            content.push_str(&format_archived_memory_line(mem));
            content.push('\n');
        }
    }

    content
}

/// Format an active memory as a fact line.
fn format_memory_line(mem: &Memory) -> String {
    let mut line = format!("- {}", mem.fact);

    // Add source metadata
    if let Some(author) = &mem.authored_by {
        let source = if author == "user" { "manual" } else { author };
        line.push_str(&format!(" [source: {}]", source));
    }

    if let Some(ft) = &mem.fact_type {
        line.push_str(&format!(" [type: {}]", ft));
    }

    if (mem.confidence - 0.8).abs() > f64::EPSILON {
        line.push_str(&format!(" [confidence: {}]", mem.confidence));
    }

    line
}

fn format_archived_memory_line(mem: &Memory) -> String {
    let mut line = format!("- ~~{}~~", mem.fact);

    if let Some(author) = &mem.authored_by {
        let source = if author == "user" { "manual" } else { author };
        line.push_str(&format!(" [source: {}]", source));
    }

    if let Some(ft) = &mem.fact_type {
        line.push_str(&format!(" [type: {}]", ft));
    }

    if (mem.confidence - 0.8).abs() > f64::EPSILON {
        line.push_str(&format!(" [confidence: {}]", mem.confidence));
    }

    let archived_at = mem.valid_to.as_deref().unwrap_or(&mem.updated_at);
    line.push_str(&format!(" [archived: {}]", archived_at));

    line
}

/// Format a relationship as a Markdown line.
fn format_relationship_line(rel: &Relationship) -> String {
    let mut line = format!("- {}: [[{}]]", rel.relation_type, rel.to_entity);

    if !rel.valid_from.is_empty() {
        line.push_str(&format!(" [since: {}]", rel.valid_from));
    }

    line
}

// ---------------------------------------------------------------------------
// Round-trip verification
// ---------------------------------------------------------------------------

/// Verify that parsing the generated Markdown reproduces the original data.
fn verify_roundtrip(
    entity: &Entity,
    memories: &[Memory],
    relationships: &[&Relationship],
    content: &str,
) -> Result<(), MigrationError> {
    let parsed = vault_parser::parse_entity_file(content).map_err(|e| {
        MigrationError::VerificationFailed {
            entity_id: entity.id.clone(),
            detail: format!("parse failed: {}", e),
        }
    })?;

    // Verify entity
    if parsed.entity.id != entity.id {
        return Err(MigrationError::VerificationFailed {
            entity_id: entity.id.clone(),
            detail: format!(
                "entity ID mismatch: got '{}' expected '{}'",
                parsed.entity.id, entity.id
            ),
        });
    }

    if parsed.entity.entity_type != entity.entity_type {
        return Err(MigrationError::VerificationFailed {
            entity_id: entity.id.clone(),
            detail: format!(
                "entity type mismatch: got '{:?}' expected '{:?}'",
                parsed.entity.entity_type, entity.entity_type
            ),
        });
    }

    // Verify memory count (active + archived)
    let active_count = memories
        .iter()
        .filter(|m| m.status == MemoryStatus::Active)
        .count();
    let archived_count = memories
        .iter()
        .filter(|m| m.status == MemoryStatus::Archived)
        .count();
    let expected_total = active_count + archived_count;

    if parsed.memories.len() != expected_total {
        return Err(MigrationError::VerificationFailed {
            entity_id: entity.id.clone(),
            detail: format!(
                "memory count mismatch: got {} expected {}",
                parsed.memories.len(),
                expected_total
            ),
        });
    }

    // Verify relationship count
    if parsed.relationships.len() != relationships.len() {
        return Err(MigrationError::VerificationFailed {
            entity_id: entity.id.clone(),
            detail: format!(
                "relationship count mismatch: got {} expected {}",
                parsed.relationships.len(),
                relationships.len()
            ),
        });
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        AllowedModels, EntityStatus, EntityType, Evidence, FactDisposition, FactType, MemorySpace,
        Sensitivity,
    };

    fn test_entity() -> Entity {
        Entity {
            id: "kubernetes".to_string(),
            entity_type: EntityType::Concept,
            name: "Kubernetes".to_string(),
            attributes: serde_json::json!({"aliases": ["k8s", "kube"]}),
            sensitivity: Sensitivity::Shareable,
            allowed_models: AllowedModels::Any,
            space: MemorySpace::Knowledge,
            status: EntityStatus::Active,
            merged_into: None,
            created_at: "2026-01-15T00:00:00Z".to_string(),
            updated_at: "2026-03-23T14:30:00Z".to_string(),
        }
    }

    fn test_memories() -> Vec<Memory> {
        vec![
            Memory {
                id: "mem-1".to_string(),
                entity_id: "kubernetes".to_string(),
                fact: "Preferred orchestration platform".to_string(),
                confidence: 0.9,
                disposition: FactDisposition::UserConfirmed,
                sensitivity: Sensitivity::Shareable,
                valid_from: "2026-01-15T00:00:00Z".to_string(),
                valid_to: None,
                status: MemoryStatus::Active,
                superseded_by: None,
                created_at: "2026-01-15T00:00:00Z".to_string(),
                updated_at: "2026-01-15T00:00:00Z".to_string(),
                fact_type: Some(FactType::Decision),
                authored_by: Some("agent".to_string()),
                supersedes: None,
                depends_on: Vec::new(),
                fsrs: None,
            },
            Memory {
                id: "mem-2".to_string(),
                entity_id: "kubernetes".to_string(),
                fact: "Scales well for our use case".to_string(),
                confidence: 0.8,
                disposition: FactDisposition::UserConfirmed,
                sensitivity: Sensitivity::Shareable,
                valid_from: "2026-01-15T00:00:00Z".to_string(),
                valid_to: Some("2026-03-23".to_string()),
                status: MemoryStatus::Archived,
                superseded_by: None,
                created_at: "2026-01-15T00:00:00Z".to_string(),
                updated_at: "2026-03-23T00:00:00Z".to_string(),
                fact_type: None,
                authored_by: Some("agent".to_string()),
                supersedes: None,
                depends_on: Vec::new(),
                fsrs: None,
            },
        ]
    }

    fn test_relationships() -> Vec<Relationship> {
        vec![Relationship {
            id: "rel-1".to_string(),
            from_entity: "kubernetes".to_string(),
            to_entity: "docker".to_string(),
            relation_type: "used_with".to_string(),
            strength: 1.0,
            valid_from: "2026-01-15".to_string(),
            valid_to: None,
            sensitivity: Sensitivity::Shareable,
            allowed_models: AllowedModels::Any,
            status: crate::RelationshipStatus::Active,
            created_at: "2026-01-15T00:00:00Z".to_string(),
            updated_at: "2026-01-15T00:00:00Z".to_string(),
        }]
    }

    #[test]
    fn generate_markdown_includes_active_sections() {
        let entity = test_entity();
        let memories = test_memories();
        let rels = test_relationships();
        let rel_refs: Vec<&Relationship> = rels.iter().collect();

        let content = generate_entity_markdown(&entity, &memories, &rel_refs);

        assert!(content.contains("id: kubernetes"));
        assert!(content.contains("type: concept"));
        assert!(content.contains("aliases: [k8s, kube]"));
        assert!(content.contains("space: knowledge"));
        assert!(content.contains("sensitivity: shareable"));
        assert!(content.contains("# Kubernetes"));
        assert!(content.contains("## Facts"));
        assert!(content.contains("## History"));
        assert!(content.contains("## Relationships"));
    }

    #[test]
    fn generate_markdown_active_facts() {
        let entity = test_entity();
        let memories = test_memories();
        let content = generate_entity_markdown(&entity, &memories, &[]);

        // Active fact should be in ## Facts
        assert!(content.contains("- Preferred orchestration platform"));
        assert!(content.contains("[type: decision]"));
        assert!(content.contains("[confidence: 0.9]"));
        assert!(content.contains("## History"));
    }

    #[test]
    fn generate_markdown_archived_facts_rendered() {
        let entity = test_entity();
        let memories = test_memories();
        let content = generate_entity_markdown(&entity, &memories, &[]);

        assert!(content.contains("## History"));
        assert!(content.contains("### Archived Facts"));
        assert!(content.contains("~~Scales well for our use case~~"));
        assert!(content.contains("[archived: 2026-03-23]"));
    }

    #[test]
    fn generate_markdown_relationships() {
        let entity = test_entity();
        let rels = test_relationships();
        let rel_refs: Vec<&Relationship> = rels.iter().collect();

        let content = generate_entity_markdown(&entity, &[], &rel_refs);

        assert!(content.contains("- used_with: [[docker]]"));
        assert!(content.contains("[since: 2026-01-15]"));
    }

    #[test]
    fn generate_markdown_preserves_optional_schema_metadata() {
        let mut entity = test_entity();
        entity.attributes = serde_json::json!({
            "aliases": ["k8s", "kube"],
            "schema": "infrastructure_platform"
        });

        let content = generate_entity_markdown(&entity, &[], &[]);
        assert!(content.contains("schema: infrastructure_platform"));
    }

    #[test]
    fn default_confidence_omitted() {
        // Confidence 0.8 is the default — should not be written
        let entity = test_entity();
        let mut memories = test_memories();
        memories.retain(|m| m.status == MemoryStatus::Active);
        memories[0].confidence = 0.8;
        memories[0].fact_type = None;
        memories[0].authored_by = None;

        let content = generate_entity_markdown(&entity, &memories, &[]);
        assert!(!content.contains("[confidence:"));
    }

    #[test]
    fn roundtrip_verification_passes() {
        let entity = test_entity();
        let memories: Vec<Memory> = test_memories()
            .into_iter()
            .filter(|m| m.status == MemoryStatus::Active)
            .collect();
        let rels = test_relationships();
        let rel_refs: Vec<&Relationship> = rels.iter().collect();

        let content = generate_entity_markdown(&entity, &memories, &rel_refs);

        // This should not error for active-only data
        verify_roundtrip(&entity, &memories, &rel_refs, &content).unwrap();
    }

    #[test]
    fn roundtrip_verification_catches_mismatch() {
        let entity = test_entity();
        let memories = test_memories();

        // Generate content, then tamper with it
        let content = generate_entity_markdown(&entity, &memories, &[]);
        let tampered = content.replace("id: kubernetes", "id: wrong");

        let result = verify_roundtrip(&entity, &memories, &[], &tampered);
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn export_writes_files() {
        let store = SqliteMemoryStore::open_in_memory().unwrap();
        store.initialize().await.unwrap();

        // Create an entity with memories
        let entity = test_entity();
        store.create_entity(&entity).await.unwrap();

        let mem = &test_memories()[0]; // active memory
        let evidence = Evidence {
            id: "ev-1".to_string(),
            memory_id: Some(mem.id.clone()),
            relationship_id: None,
            entity_id: Some(entity.id.clone()),
            article_id: None,
            source_url: None,
            evidence_quote: Some("test".to_string()),
            observed_at: "2026-01-15T00:00:00Z".to_string(),
            created_at: "2026-01-15T00:00:00Z".to_string(),
        };
        store.create_memory(mem, &[evidence]).await.unwrap();

        // Export
        let vault = tempfile::TempDir::new().unwrap();
        let options = MigrationOptions {
            skip_existing: false,
            verify: true,
        };
        let stats = export_to_vault(&store, vault.path(), &options)
            .await
            .unwrap();

        assert_eq!(stats.entities_exported, 1);
        assert_eq!(stats.files_written, 1);
        assert_eq!(stats.memories_exported, 1);

        // Verify file exists and is parseable
        let file_path = vault
            .path()
            .join("ledger/concepts/kubernetes/kubernetes.md");
        assert!(file_path.exists());

        let content = std::fs::read_to_string(&file_path).unwrap();
        let parsed = vault_parser::parse_entity_file(&content).unwrap();
        assert_eq!(parsed.entity.id, "kubernetes");
        assert_eq!(parsed.memories.len(), 1);
    }

    #[tokio::test]
    async fn export_skips_existing_files() {
        let store = SqliteMemoryStore::open_in_memory().unwrap();
        store.initialize().await.unwrap();

        let entity = test_entity();
        store.create_entity(&entity).await.unwrap();

        let vault = tempfile::TempDir::new().unwrap();

        // Pre-create the file
        let dir = vault.path().join("ledger/concepts/kubernetes");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("kubernetes.md"), "existing content").unwrap();

        let options = MigrationOptions {
            skip_existing: true,
            verify: false,
        };
        let stats = export_to_vault(&store, vault.path(), &options)
            .await
            .unwrap();

        assert_eq!(stats.files_skipped, 1);
        assert_eq!(stats.files_written, 0);

        // Original content preserved
        let content = std::fs::read_to_string(dir.join("kubernetes.md")).unwrap();
        assert_eq!(content, "existing content");
    }
}
