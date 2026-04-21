//! The `MemoryStore` async trait — the primary API for memory operations.

use crate::types::*;

#[async_trait::async_trait]
pub trait MemoryStore: Send + Sync {
    // --- Entity operations ---

    /// Create a new entity. Returns error if duplicate detected (same name + type).
    async fn create_entity(&self, entity: &Entity) -> Result<(), MemoryStoreError>;

    /// Get entity by ID.
    async fn get_entity(&self, id: &str) -> Result<Entity, MemoryStoreError>;

    /// Find entities by name (fuzzy match via FTS5).
    async fn find_entities(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<Entity>, MemoryStoreError>;

    /// Find entities by type.
    async fn find_entities_by_type(
        &self,
        entity_type: EntityType,
        limit: usize,
    ) -> Result<Vec<Entity>, MemoryStoreError>;

    /// Update entity attributes. Bumps `updated_at`.
    async fn update_entity(
        &self,
        id: &str,
        attributes: serde_json::Value,
    ) -> Result<(), MemoryStoreError>;

    /// Merge two entities: mark `source_id` as merged into `target_id`,
    /// reassign all memories and relationships.
    async fn merge_entities(
        &self,
        source_id: &str,
        target_id: &str,
    ) -> Result<(), MemoryStoreError>;

    // --- Memory operations ---

    /// Store a memory fact with evidence. Rejects if no evidence provided.
    async fn create_memory(
        &self,
        memory: &Memory,
        evidence: &[Evidence],
    ) -> Result<(), MemoryStoreError>;

    /// Get memories for an entity, filtered by status.
    async fn get_memories(
        &self,
        entity_id: &str,
        status: Option<MemoryStatus>,
    ) -> Result<Vec<Memory>, MemoryStoreError>;

    /// Search memories by text (FTS5).
    async fn search_memories(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<Memory>, MemoryStoreError>;

    /// Supersede a memory: set old memory status to `superseded`, link to new memory.
    async fn supersede_memory(
        &self,
        old_id: &str,
        new_memory: &Memory,
        evidence: &[Evidence],
    ) -> Result<(), MemoryStoreError>;

    /// Archive a memory (soft delete).
    ///
    /// Sets the memory's status to `Archived`. The memory remains in the
    /// database for historical auditing but is excluded from standard
    /// context retrieval.
    async fn archive_memory(&self, memory_id: &str) -> Result<(), MemoryStoreError>;

    /// Get memories pending review.
    async fn get_review_queue(&self, limit: usize) -> Result<Vec<Memory>, MemoryStoreError>;

    // --- Relationship operations ---

    /// Create a relationship between two entities.
    async fn create_relationship(&self, rel: &Relationship) -> Result<(), MemoryStoreError>;

    /// Get relationships for an entity (both directions).
    async fn get_relationships(
        &self,
        entity_id: &str,
    ) -> Result<Vec<Relationship>, MemoryStoreError>;

    /// Get relationships of a specific type.
    async fn get_relationships_by_type(
        &self,
        relation_type: &str,
        limit: usize,
    ) -> Result<Vec<Relationship>, MemoryStoreError>;

    // --- Evidence operations ---

    /// Get evidence for a memory.
    async fn get_evidence(&self, memory_id: &str) -> Result<Vec<Evidence>, MemoryStoreError>;

    // --- Space-aware operations ---

    /// Find entities belonging to a specific memory space.
    async fn find_entities_by_space(
        &self,
        space: MemorySpace,
        limit: usize,
    ) -> Result<Vec<Entity>, MemoryStoreError>;

    /// Search memories filtered by memory space.
    async fn search_memories_in_space(
        &self,
        query: &str,
        space: MemorySpace,
        limit: usize,
    ) -> Result<Vec<Memory>, MemoryStoreError>;

    // --- Query operations for Recall Gateway ---

    /// Retrieve entities and their active memories matching a query,
    /// filtered by sensitivity level.
    async fn recall(
        &self,
        query: &str,
        sensitivity_max: Sensitivity,
        limit: usize,
    ) -> Result<Vec<MemoryRecallResult>, MemoryStoreError>;

    /// Retrieve entities and memories matching a query, filtered by both
    /// sensitivity and memory space.
    async fn recall_in_space(
        &self,
        query: &str,
        sensitivity_max: Sensitivity,
        space: MemorySpace,
        limit: usize,
    ) -> Result<Vec<MemoryRecallResult>, MemoryStoreError>;

    /// Cross-space recall: search across all spaces and return results
    /// annotated with their source space.
    async fn recall_cross_space(
        &self,
        query: &str,
        sensitivity_max: Sensitivity,
        limit: usize,
    ) -> Result<Vec<MemoryRecallResult>, MemoryStoreError>;

    // --- Link operations (wikilink graph) ---

    /// Store a link between two entities (from wikilink parsing).
    async fn create_link(&self, link: &EntityLink) -> Result<(), MemoryStoreError>;

    /// Get all outgoing links from an entity.
    async fn get_outgoing_links(
        &self,
        entity_id: &str,
    ) -> Result<Vec<EntityLink>, MemoryStoreError>;

    /// Get all backlinks pointing to an entity.
    async fn get_backlinks(&self, entity_id: &str) -> Result<Vec<EntityLink>, MemoryStoreError>;
}
