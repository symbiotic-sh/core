//! Symbiotic Memory Store — Neural Graph persistence layer.
//!
//! Provides SQLite-backed storage for entities, relationships, memories,
//! and evidence with FTS5 full-text search and entity deduplication.
//!
//! See `docs/design/vault-as-truth.md` for the canonical storage model.

pub mod cost;
pub mod dedup;
pub mod entity_dedup;
pub mod extraction;
pub mod graph_store;
pub mod grounding;
pub mod quality_pipeline;
pub mod recall_probes;
pub mod self_improvement;
pub mod sqlite;
mod sqlite_schema;
mod sqlite_search;
pub mod staleness;
pub mod store;
pub mod tool_memory;
pub mod types;
pub mod vault_git;
pub mod vault_history;
pub mod vault_indexer;
pub mod vault_layout;
pub mod vault_linter;
pub mod vault_migrate;
pub mod vault_parser;
pub mod vault_watcher;
pub mod vault_writer;
pub mod wikilink;

pub use extraction::{ExtractionLlmClient, MemoryExtractor, OllamaExtractor};
pub use graph_store::SqliteGraphStore;
pub use grounding::GroundingChecker;
pub use sqlite::{is_database_unencrypted, migrate_to_encrypted, CipherError, SqliteMemoryStore};
pub use store::MemoryStore;
pub use types::*;
