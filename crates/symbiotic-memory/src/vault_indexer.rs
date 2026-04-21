//! Vault Indexer — scans Markdown entity files and populates the SQLite search index.
//!
//! The indexer is the bridge between the Vault (Markdown source of truth) and
//! SQLite (derived search index). It can perform:
//! - **Full rebuild**: delete all indexed data, re-scan all files
//! - **Incremental update**: re-index only files whose content hash changed
//!
//! See `docs/design/vault-as-truth.md` for the architecture.

use std::path::Path;
use std::sync::Arc;

use hex::encode as hex_encode;
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;

use crate::sqlite::SqliteMemoryStore;
use crate::vault_layout::collect_canonical_markdown_files;
use crate::vault_parser::{self, ParsedEntityFile, VaultParseError};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Statistics returned after an indexing operation.
#[derive(Debug, Clone, Default)]
pub struct IndexStats {
    pub files_scanned: usize,
    pub files_indexed: usize,
    pub files_unchanged: usize,
    pub files_failed: usize,
    pub entities_upserted: usize,
    pub memories_upserted: usize,
    pub relationships_upserted: usize,
    pub files_removed: usize,
}

/// Errors during vault indexing.
#[derive(Debug, thiserror::Error)]
pub enum VaultIndexError {
    #[error("database error: {0}")]
    Database(String),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("parse error in {path}: {source}")]
    Parse {
        path: String,
        source: VaultParseError,
    },
}

impl From<rusqlite::Error> for VaultIndexError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Database(e.to_string())
    }
}

// ---------------------------------------------------------------------------
// Schema
// ---------------------------------------------------------------------------

const VAULT_INDEX_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS vault_file_index (
    file_path TEXT PRIMARY KEY,
    content_hash TEXT NOT NULL,
    last_indexed INTEGER NOT NULL
);
"#;

// ---------------------------------------------------------------------------
// VaultIndexer
// ---------------------------------------------------------------------------

/// Scans vault Markdown files and populates the SQLite search index.
pub struct VaultIndexer {
    conn: Arc<Mutex<rusqlite::Connection>>,
}

impl VaultIndexer {
    /// Create a new indexer sharing the store's database connection.
    pub fn new(store: &SqliteMemoryStore) -> Self {
        Self {
            conn: Arc::clone(&store.conn),
        }
    }

    /// Create the vault_file_index table if it doesn't exist.
    pub async fn initialize(&self) -> Result<(), VaultIndexError> {
        let conn = self.conn.lock().await;
        conn.execute_batch(VAULT_INDEX_SCHEMA)?;
        Ok(())
    }

    /// Full rebuild: clear all vault-sourced data, then re-index everything.
    ///
    /// `vault_root` should point to the `knowledge-base/` directory.
    /// Scans canonical Markdown under `ledger/`, `identity/`, and `operations/`.
    pub async fn rebuild(&self, vault_root: &Path) -> Result<IndexStats, VaultIndexError> {
        // Clear vault file index
        {
            let conn = self.conn.lock().await;
            conn.execute("DELETE FROM vault_file_index", [])?;
            // Clear all entities, memories, relationships (they are derived)
            conn.execute("DELETE FROM evidence", [])?;
            conn.execute("DELETE FROM memories", [])?;
            conn.execute("DELETE FROM relationships", [])?;
            conn.execute("DELETE FROM links", [])?;
            conn.execute("DELETE FROM entities", [])?;
        }

        self.index_all(vault_root).await
    }

    /// Incremental update: only re-index files whose content hash changed.
    ///
    /// Also removes index entries for files that no longer exist.
    pub async fn update(&self, vault_root: &Path) -> Result<IndexStats, VaultIndexError> {
        self.index_all(vault_root).await
    }

    /// Core indexing logic used by both `rebuild` and `update`.
    async fn index_all(&self, vault_root: &Path) -> Result<IndexStats, VaultIndexError> {
        let mut stats = IndexStats::default();

        // Collect all canonical `.md` files from the active vault layout.
        let md_files = collect_canonical_markdown_files(vault_root)?;

        // Track which file paths we've seen (for orphan detection)
        let mut seen_paths = std::collections::HashSet::new();

        for (file_path, rel_path) in &md_files {
            stats.files_scanned += 1;
            seen_paths.insert(rel_path.clone());

            // Read file content
            let content = std::fs::read_to_string(file_path)?;
            let content_hash = compute_hash(&content);

            // Check if already indexed with same hash
            let needs_index = {
                let conn = self.conn.lock().await;
                let existing: Option<String> = conn
                    .query_row(
                        "SELECT content_hash FROM vault_file_index WHERE file_path = ?1",
                        [rel_path],
                        |row| row.get(0),
                    )
                    .ok();
                existing.as_deref() != Some(&content_hash)
            };

            if !needs_index {
                stats.files_unchanged += 1;
                continue;
            }

            // Parse the file
            let parsed = match vault_parser::parse_entity_file(&content) {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!("Failed to parse vault file {}: {}", rel_path, e);
                    stats.files_failed += 1;
                    continue;
                }
            };

            // Index the parsed data
            let file_stats = self
                .index_parsed_file(&parsed, rel_path, &content_hash)
                .await?;
            stats.files_indexed += 1;
            stats.entities_upserted += file_stats.entities_upserted;
            stats.memories_upserted += file_stats.memories_upserted;
            stats.relationships_upserted += file_stats.relationships_upserted;
        }

        // Remove entries for files that no longer exist
        let removed = self.remove_orphans(&seen_paths).await?;
        stats.files_removed = removed;

        Ok(stats)
    }

    /// Index a single parsed entity file into SQLite.
    async fn index_parsed_file(
        &self,
        parsed: &ParsedEntityFile,
        file_path: &str,
        content_hash: &str,
    ) -> Result<IndexStats, VaultIndexError> {
        let mut stats = IndexStats::default();
        let conn = self.conn.lock().await;

        let entity = &parsed.entity;

        // Upsert entity
        conn.execute(
            "INSERT OR REPLACE INTO entities (id, entity_type, name, attributes, sensitivity, allowed_models, space, status, merged_into, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            rusqlite::params![
                entity.id,
                entity.entity_type.as_str(),
                entity.name,
                serde_json::to_string(&entity.attributes).unwrap_or_default(),
                entity.sensitivity.as_str(),
                entity.allowed_models.as_str(),
                entity.space.as_str(),
                entity.status.as_str(),
                entity.merged_into,
                entity.created_at,
                entity.updated_at,
            ],
        )?;
        stats.entities_upserted += 1;

        // Delete existing memories for this entity before re-inserting
        // (ensures facts removed from the .md file are removed from the index)
        conn.execute("DELETE FROM memories WHERE entity_id = ?1", [&entity.id])?;

        // Insert memories
        for memory in &parsed.memories {
            conn.execute(
                "INSERT INTO memories (id, entity_id, fact, confidence, disposition, sensitivity, valid_from, valid_to, status, superseded_by, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                rusqlite::params![
                    memory.id,
                    memory.entity_id,
                    memory.fact,
                    memory.confidence,
                    memory.disposition.as_str(),
                    memory.sensitivity.as_str(),
                    memory.valid_from,
                    memory.valid_to,
                    memory.status.as_str(),
                    memory.superseded_by,
                    memory.created_at,
                    memory.updated_at,
                ],
            )?;
            stats.memories_upserted += 1;
        }

        // Delete existing relationships from this entity before re-inserting
        conn.execute(
            "DELETE FROM relationships WHERE from_entity = ?1",
            [&entity.id],
        )?;

        // Insert relationships
        for rel in &parsed.relationships {
            // Ensure the target entity exists (create a stub if not)
            let target_exists: bool = conn
                .query_row(
                    "SELECT 1 FROM entities WHERE id = ?1",
                    [&rel.to_entity],
                    |_| Ok(true),
                )
                .unwrap_or(false);

            if !target_exists {
                conn.execute(
                    "INSERT INTO entities (id, entity_type, name, attributes, sensitivity, allowed_models, space, status, created_at, updated_at)
                     VALUES (?1, 'concept', ?2, '{}', 'private', 'any', 'knowledge', 'active', ?3, ?4)",
                    rusqlite::params![
                        rel.to_entity,
                        rel.to_entity,
                        rel.created_at,
                        rel.updated_at,
                    ],
                )?;
            }

            conn.execute(
                "INSERT OR REPLACE INTO relationships (id, from_entity, to_entity, relation_type, strength, valid_from, valid_to, sensitivity, allowed_models, status, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                rusqlite::params![
                    rel.id,
                    rel.from_entity,
                    rel.to_entity,
                    rel.relation_type,
                    rel.strength,
                    rel.valid_from,
                    rel.valid_to,
                    rel.sensitivity.as_str(),
                    rel.allowed_models.as_str(),
                    rel.status.as_str(),
                    rel.created_at,
                    rel.updated_at,
                ],
            )?;
            stats.relationships_upserted += 1;
        }

        // Update vault file index
        let now = chrono::Utc::now().timestamp();
        conn.execute(
            "INSERT OR REPLACE INTO vault_file_index (file_path, content_hash, last_indexed)
             VALUES (?1, ?2, ?3)",
            rusqlite::params![file_path, content_hash, now],
        )?;

        Ok(stats)
    }

    /// Remove index entries for files that no longer exist in the vault.
    async fn remove_orphans(
        &self,
        existing_paths: &std::collections::HashSet<String>,
    ) -> Result<usize, VaultIndexError> {
        let conn = self.conn.lock().await;

        // Get all indexed file paths
        let mut stmt = conn.prepare("SELECT file_path FROM vault_file_index")?;
        let indexed_paths: Vec<String> = stmt
            .query_map([], |row| row.get(0))?
            .filter_map(|r| r.ok())
            .collect();

        let mut removed = 0;
        for path in indexed_paths {
            if !existing_paths.contains(&path) {
                // Look up which entity this file contained
                // We need to find the entity ID from the file path
                // Since we don't store entity_id → file_path mapping directly,
                // we delete the vault_file_index entry.
                // The entity data will be cleaned up on next rebuild.
                conn.execute("DELETE FROM vault_file_index WHERE file_path = ?1", [&path])?;
                removed += 1;
            }
        }

        Ok(removed)
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Compute SHA-256 hash of content, returned as hex string.
fn compute_hash(content: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    hex_encode(hasher.finalize())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SqliteMemoryStore;
    use tempfile::TempDir;

    /// Helper: set up an in-memory store + indexer.
    async fn setup() -> (SqliteMemoryStore, VaultIndexer) {
        let store = SqliteMemoryStore::open_in_memory().unwrap();
        store.initialize().await.unwrap();
        let indexer = VaultIndexer::new(&store);
        indexer.initialize().await.unwrap();
        (store, indexer)
    }

    /// Helper: write an entity file to a temp vault.
    fn write_entity(vault: &Path, subdir: &str, filename: &str, content: &str) {
        let dir = vault.join(subdir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(filename), content).unwrap();
    }

    const RUST_ENTITY: &str = r#"---
id: rust
type: tool
space: knowledge
sensitivity: shareable
created: 2026-01-01T00:00:00Z
updated: 2026-01-01T00:00:00Z
---

# Rust

## Facts
- Systems programming language [source: manual] [type: finding] [confidence: 0.95]
- Memory safe without GC [source: article-001] [type: finding]

## Relationships
- used_with: [[cargo]]
"#;

    const CARGO_ENTITY: &str = r#"---
id: cargo
type: tool
space: knowledge
created: 2026-01-01T00:00:00Z
updated: 2026-01-01T00:00:00Z
---

# Cargo

## Facts
- Rust package manager and build tool [source: manual] [type: finding]
"#;

    #[tokio::test]
    async fn rebuild_indexes_all_files() {
        let (_store, indexer) = setup().await;
        let vault = TempDir::new().unwrap();

        write_entity(vault.path(), "ledger/tools/rust", "rust.md", RUST_ENTITY);
        write_entity(vault.path(), "ledger/tools/cargo", "cargo.md", CARGO_ENTITY);

        let stats = indexer.rebuild(vault.path()).await.unwrap();
        assert_eq!(stats.files_scanned, 2);
        assert_eq!(stats.files_indexed, 2);
        assert_eq!(stats.files_unchanged, 0);
        assert_eq!(stats.entities_upserted, 2);
        assert_eq!(stats.memories_upserted, 3); // 2 rust + 1 cargo
        assert_eq!(stats.relationships_upserted, 1);
    }

    #[tokio::test]
    async fn incremental_skips_unchanged() {
        let (_store, indexer) = setup().await;
        let vault = TempDir::new().unwrap();

        write_entity(vault.path(), "ledger/tools/rust", "rust.md", RUST_ENTITY);

        // First index
        let stats1 = indexer.update(vault.path()).await.unwrap();
        assert_eq!(stats1.files_indexed, 1);

        // Second index — same content, should skip
        let stats2 = indexer.update(vault.path()).await.unwrap();
        assert_eq!(stats2.files_unchanged, 1);
        assert_eq!(stats2.files_indexed, 0);
    }

    #[tokio::test]
    async fn incremental_reindexes_changed() {
        let (_store, indexer) = setup().await;
        let vault = TempDir::new().unwrap();

        write_entity(vault.path(), "ledger/tools/rust", "rust.md", RUST_ENTITY);

        // First index
        indexer.update(vault.path()).await.unwrap();

        // Modify the file
        let updated = RUST_ENTITY.replace(
            "Memory safe without GC",
            "Memory safe without garbage collection",
        );
        write_entity(vault.path(), "ledger/tools/rust", "rust.md", &updated);

        // Second index — should re-index
        let stats = indexer.update(vault.path()).await.unwrap();
        assert_eq!(stats.files_indexed, 1);
        assert_eq!(stats.files_unchanged, 0);
    }

    #[tokio::test]
    async fn entities_queryable_after_index() {
        let (store, indexer) = setup().await;
        let vault = TempDir::new().unwrap();

        write_entity(vault.path(), "ledger/tools/rust", "rust.md", RUST_ENTITY);
        indexer.rebuild(vault.path()).await.unwrap();

        // Query the entity via the store
        use crate::store::MemoryStore;
        let entity = store.get_entity("rust").await.unwrap();
        assert_eq!(entity.name, "Rust");
        assert_eq!(entity.entity_type, crate::EntityType::Tool);

        // Query memories
        let memories = store.get_memories("rust", None).await.unwrap();
        assert_eq!(memories.len(), 2);
    }

    #[tokio::test]
    async fn relationships_create_stub_entities() {
        let (store, indexer) = setup().await;
        let vault = TempDir::new().unwrap();

        // Rust references [[cargo]] but cargo.md doesn't exist yet
        write_entity(vault.path(), "ledger/tools/rust", "rust.md", RUST_ENTITY);
        indexer.rebuild(vault.path()).await.unwrap();

        // Cargo should exist as a stub entity
        use crate::store::MemoryStore;
        let cargo = store.get_entity("cargo").await.unwrap();
        assert_eq!(cargo.name, "cargo"); // stub uses ID as name
    }

    #[tokio::test]
    async fn rebuild_clears_old_data() {
        let (store, indexer) = setup().await;
        let vault = TempDir::new().unwrap();

        write_entity(vault.path(), "ledger/tools/rust", "rust.md", RUST_ENTITY);
        write_entity(vault.path(), "ledger/tools/cargo", "cargo.md", CARGO_ENTITY);
        indexer.rebuild(vault.path()).await.unwrap();

        // Remove cargo.md and rebuild
        std::fs::remove_file(vault.path().join("ledger/tools/cargo/cargo.md")).unwrap();
        let stats = indexer.rebuild(vault.path()).await.unwrap();

        // Only rust should be indexed
        assert_eq!(stats.files_indexed, 1);

        use crate::store::MemoryStore;
        let result = store.get_entity("cargo").await;
        // Cargo stub might still exist from rust's relationship reference,
        // but its memories should be gone
        if let Ok(cargo) = result {
            let mems = store.get_memories(&cargo.id, None).await.unwrap();
            assert_eq!(mems.len(), 0);
        }
    }

    #[tokio::test]
    async fn removed_facts_disappear_on_reindex() {
        let (store, indexer) = setup().await;
        let vault = TempDir::new().unwrap();

        write_entity(vault.path(), "ledger/tools/rust", "rust.md", RUST_ENTITY);
        indexer.update(vault.path()).await.unwrap();

        // Remove a fact from the file
        let updated = RUST_ENTITY.replace(
            "- Memory safe without GC [source: article-001] [type: finding]\n",
            "",
        );
        write_entity(vault.path(), "ledger/tools/rust", "rust.md", &updated);
        indexer.update(vault.path()).await.unwrap();

        use crate::store::MemoryStore;
        let memories = store.get_memories("rust", None).await.unwrap();
        assert_eq!(memories.len(), 1); // Only 1 fact remains
    }

    #[tokio::test]
    async fn scans_ledger_identity_and_operations_subdirectories() {
        let (_store, indexer) = setup().await;
        let vault = TempDir::new().unwrap();

        write_entity(vault.path(), "ledger/tools/rust", "rust.md", RUST_ENTITY);

        let identity_entity = r#"---
id: preferences
type: preference
space: identity
created: 2026-01-01T00:00:00Z
updated: 2026-01-01T00:00:00Z
---

# My Preferences

## Facts
- Dark mode preferred [source: manual] [type: preference]
"#;
        write_entity(vault.path(), "identity", "preferences.md", identity_entity);

        let stats = indexer.rebuild(vault.path()).await.unwrap();
        assert_eq!(stats.files_scanned, 2);
        assert_eq!(stats.files_indexed, 2);
        assert_eq!(stats.entities_upserted, 2);
    }

    #[tokio::test]
    async fn orphan_removal_on_update() {
        let (_store, indexer) = setup().await;
        let vault = TempDir::new().unwrap();

        write_entity(vault.path(), "ledger/tools/rust", "rust.md", RUST_ENTITY);
        write_entity(vault.path(), "ledger/tools/cargo", "cargo.md", CARGO_ENTITY);
        indexer.update(vault.path()).await.unwrap();

        // Remove cargo.md
        std::fs::remove_file(vault.path().join("ledger/tools/cargo/cargo.md")).unwrap();

        let stats = indexer.update(vault.path()).await.unwrap();
        assert_eq!(stats.files_removed, 1);
    }

    #[tokio::test]
    async fn non_md_files_ignored() {
        let (_store, indexer) = setup().await;
        let vault = TempDir::new().unwrap();

        write_entity(vault.path(), "ledger/tools/rust", "rust.md", RUST_ENTITY);
        // Write a non-md file
        let dir = vault.path().join("ledger/tools/rust");
        std::fs::write(dir.join("notes.txt"), "just text").unwrap();

        let stats = indexer.update(vault.path()).await.unwrap();
        assert_eq!(stats.files_scanned, 1); // only .md
    }

    #[tokio::test]
    async fn malformed_files_counted_as_failed() {
        let (_store, indexer) = setup().await;
        let vault = TempDir::new().unwrap();

        write_entity(
            vault.path(),
            "ledger/concepts/bad",
            "bad.md",
            "No frontmatter here, just text.",
        );

        let stats = indexer.update(vault.path()).await.unwrap();
        assert_eq!(stats.files_scanned, 1);
        assert_eq!(stats.files_failed, 1);
        assert_eq!(stats.files_indexed, 0);
    }

    #[tokio::test]
    async fn empty_vault_returns_zero_stats() {
        let (_store, indexer) = setup().await;
        let vault = TempDir::new().unwrap();

        let stats = indexer.rebuild(vault.path()).await.unwrap();
        assert_eq!(stats.files_scanned, 0);
        assert_eq!(stats.files_indexed, 0);
    }

    #[tokio::test]
    async fn scans_nested_ledger_layout_and_ignores_briefs() {
        let (_store, indexer) = setup().await;
        let vault = TempDir::new().unwrap();

        write_entity(vault.path(), "ledger/tools/rust", "rust.md", RUST_ENTITY);
        write_entity(
            vault.path(),
            "ledger/tools/rust",
            "rust.brief.md",
            "# Generated brief",
        );

        let stats = indexer.rebuild(vault.path()).await.unwrap();
        assert_eq!(stats.files_scanned, 1);
        assert_eq!(stats.files_indexed, 1);
        assert_eq!(stats.entities_upserted, 1);
    }
}
