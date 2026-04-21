//! `MemoryStore` trait implementation and search/query methods for `SqliteMemoryStore`.

use chrono::Utc;
use symbiotic_core::now_unix;

use crate::dedup;
use crate::sqlite::SqliteMemoryStore;
use crate::sqlite_schema::{
    row_to_entity, row_to_entity_link, row_to_evidence, row_to_memory, row_to_relationship,
};
use crate::types::*;

#[async_trait::async_trait]
impl crate::store::MemoryStore for SqliteMemoryStore {
    // --- Entity operations ---

    async fn create_entity(&self, entity: &Entity) -> Result<(), MemoryStoreError> {
        let conn = self.conn.lock().await;
        let normalized = dedup::normalize_name(&entity.name);

        // Check for duplicate: exact normalized name match with same type
        let mut stmt = conn.prepare(
            "SELECT id, name FROM entities WHERE entity_type = ?1 AND status = 'active'",
        )?;
        let existing: Vec<(String, String)> = stmt
            .query_map([entity.entity_type.as_str()], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .filter_map(|r| r.ok())
            .collect();

        for (existing_id, existing_name) in &existing {
            let existing_normalized = dedup::normalize_name(existing_name);
            if existing_normalized == normalized {
                return Err(MemoryStoreError::DuplicateEntity {
                    name: entity.name.clone(),
                    entity_type: entity.entity_type,
                    existing_id: existing_id.clone(),
                });
            }
            if self.dedup_config().max_edit_distance > 0
                && dedup::edit_distance(&normalized, &existing_normalized)
                    <= self.dedup_config().max_edit_distance
            {
                return Err(MemoryStoreError::DuplicateEntity {
                    name: entity.name.clone(),
                    entity_type: entity.entity_type,
                    existing_id: existing_id.clone(),
                });
            }
        }

        let attrs = serde_json::to_string(&entity.attributes)
            .map_err(|e| MemoryStoreError::Database(e.to_string()))?;

        conn.execute(
            "INSERT INTO entities (id, entity_type, name, attributes, sensitivity, allowed_models, space, status, merged_into, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            rusqlite::params![
                entity.id,
                entity.entity_type.as_str(),
                entity.name,
                attrs,
                entity.sensitivity.as_str(),
                entity.allowed_models.as_str(),
                entity.space.as_str(),
                entity.status.as_str(),
                entity.merged_into,
                entity.created_at,
                entity.updated_at,
            ],
        )?;

        Ok(())
    }

    async fn get_entity(&self, id: &str) -> Result<Entity, MemoryStoreError> {
        let conn = self.conn.lock().await;
        conn.query_row("SELECT * FROM entities WHERE id = ?1", [id], row_to_entity)
            .map_err(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => {
                    MemoryStoreError::EntityNotFound(id.to_string())
                }
                other => MemoryStoreError::from(other),
            })
    }

    async fn find_entities(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<Entity>, MemoryStoreError> {
        let conn = self.conn.lock().await;
        // Use FTS5 MATCH for full-text search
        let fts_query = format!("\"{}\"", query.replace('"', "\"\""));
        let mut stmt = conn.prepare(
            "SELECT e.* FROM entities e
             JOIN entities_fts fts ON e.rowid = fts.rowid
             WHERE entities_fts MATCH ?1
             AND e.status = 'active'
             LIMIT ?2",
        )?;
        let results = stmt
            .query_map(rusqlite::params![fts_query, limit as i64], row_to_entity)?
            .filter_map(|r| r.ok())
            .collect();
        Ok(results)
    }

    async fn find_entities_by_type(
        &self,
        entity_type: EntityType,
        limit: usize,
    ) -> Result<Vec<Entity>, MemoryStoreError> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(
            "SELECT * FROM entities WHERE entity_type = ?1 AND status = 'active' LIMIT ?2",
        )?;
        let results = stmt
            .query_map(
                rusqlite::params![entity_type.as_str(), limit as i64],
                row_to_entity,
            )?
            .filter_map(|r| r.ok())
            .collect();
        Ok(results)
    }

    async fn update_entity(
        &self,
        id: &str,
        attributes: serde_json::Value,
    ) -> Result<(), MemoryStoreError> {
        let conn = self.conn.lock().await;
        let now = Utc::now().to_rfc3339();
        let attrs = serde_json::to_string(&attributes)
            .map_err(|e| MemoryStoreError::Database(e.to_string()))?;

        let rows = conn.execute(
            "UPDATE entities SET attributes = ?1, updated_at = ?2 WHERE id = ?3",
            rusqlite::params![attrs, now, id],
        )?;

        if rows == 0 {
            return Err(MemoryStoreError::EntityNotFound(id.to_string()));
        }
        Ok(())
    }

    async fn merge_entities(
        &self,
        source_id: &str,
        target_id: &str,
    ) -> Result<(), MemoryStoreError> {
        let conn = self.conn.lock().await;
        let now = Utc::now().to_rfc3339();

        // Verify both entities exist
        let source_exists: bool = conn
            .query_row(
                "SELECT COUNT(*) > 0 FROM entities WHERE id = ?1",
                [source_id],
                |row| row.get(0),
            )
            .map_err(MemoryStoreError::from)?;
        if !source_exists {
            return Err(MemoryStoreError::EntityNotFound(source_id.to_string()));
        }

        let target_exists: bool = conn
            .query_row(
                "SELECT COUNT(*) > 0 FROM entities WHERE id = ?1",
                [target_id],
                |row| row.get(0),
            )
            .map_err(MemoryStoreError::from)?;
        if !target_exists {
            return Err(MemoryStoreError::EntityNotFound(target_id.to_string()));
        }

        // Reassign memories from source to target
        conn.execute(
            "UPDATE memories SET entity_id = ?1, updated_at = ?2 WHERE entity_id = ?3",
            rusqlite::params![target_id, now, source_id],
        )?;

        // Reassign relationships: update from_entity
        conn.execute(
            "UPDATE relationships SET from_entity = ?1, updated_at = ?2 WHERE from_entity = ?3",
            rusqlite::params![target_id, now, source_id],
        )?;

        // Reassign relationships: update to_entity
        conn.execute(
            "UPDATE relationships SET to_entity = ?1, updated_at = ?2 WHERE to_entity = ?3",
            rusqlite::params![target_id, now, source_id],
        )?;

        // Reassign evidence entity_id
        conn.execute(
            "UPDATE evidence SET entity_id = ?1 WHERE entity_id = ?2",
            rusqlite::params![target_id, source_id],
        )?;

        // Mark source as merged
        conn.execute(
            "UPDATE entities SET status = 'merged', merged_into = ?1, updated_at = ?2 WHERE id = ?3",
            rusqlite::params![target_id, now, source_id],
        )?;

        Ok(())
    }

    // --- Memory operations ---

    async fn create_memory(
        &self,
        memory: &Memory,
        evidence: &[Evidence],
    ) -> Result<(), MemoryStoreError> {
        if evidence.is_empty() {
            return Err(MemoryStoreError::EvidenceRequired);
        }

        let conn = self.conn.lock().await;

        // Verify entity exists
        let entity_exists: bool = conn
            .query_row(
                "SELECT COUNT(*) > 0 FROM entities WHERE id = ?1",
                [&memory.entity_id],
                |row| row.get(0),
            )
            .map_err(MemoryStoreError::from)?;
        if !entity_exists {
            return Err(MemoryStoreError::EntityNotFound(memory.entity_id.clone()));
        }
        let fsrs = memory
            .fsrs
            .clone()
            .unwrap_or_else(|| default_fsrs_state(memory));

        conn.execute(
            "INSERT INTO memories (id, entity_id, fact, confidence, disposition, sensitivity, valid_from, valid_to, status, superseded_by, fsrs_stability, fsrs_difficulty, fsrs_last_access, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
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
                fsrs.stability,
                fsrs.difficulty,
                fsrs.last_access,
                memory.created_at,
                memory.updated_at,
            ],
        )?;

        // Insert evidence records
        for ev in evidence {
            conn.execute(
                "INSERT INTO evidence (id, memory_id, relationship_id, entity_id, article_id, source_url, evidence_quote, observed_at, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                rusqlite::params![
                    ev.id,
                    ev.memory_id,
                    ev.relationship_id,
                    ev.entity_id,
                    ev.article_id,
                    ev.source_url,
                    ev.evidence_quote,
                    ev.observed_at,
                    ev.created_at,
                ],
            )?;
        }

        Ok(())
    }

    async fn get_memories(
        &self,
        entity_id: &str,
        status: Option<MemoryStatus>,
    ) -> Result<Vec<Memory>, MemoryStoreError> {
        let conn = self.conn.lock().await;
        match status {
            Some(s) => {
                let mut stmt =
                    conn.prepare("SELECT * FROM memories WHERE entity_id = ?1 AND status = ?2")?;
                let results: Vec<Memory> = stmt
                    .query_map(rusqlite::params![entity_id, s.as_str()], row_to_memory)?
                    .filter_map(|r| r.ok())
                    .collect();
                Ok(results)
            }
            None => {
                let mut stmt = conn.prepare("SELECT * FROM memories WHERE entity_id = ?1")?;
                let results: Vec<Memory> = stmt
                    .query_map([entity_id], row_to_memory)?
                    .filter_map(|r| r.ok())
                    .collect();
                Ok(results)
            }
        }
    }

    async fn search_memories(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<Memory>, MemoryStoreError> {
        let conn = self.conn.lock().await;
        let fts_query = format!("\"{}\"", query.replace('"', "\"\""));
        let mut stmt = conn.prepare(
            "SELECT m.* FROM memories m
             JOIN memories_fts fts ON m.rowid = fts.rowid
             WHERE memories_fts MATCH ?1
             AND m.status = 'active'
             LIMIT ?2",
        )?;
        let results = stmt
            .query_map(rusqlite::params![fts_query, limit as i64], row_to_memory)?
            .filter_map(|r| r.ok())
            .collect();
        Ok(results)
    }

    async fn archive_memory(&self, memory_id: &str) -> Result<(), MemoryStoreError> {
        let conn = self.conn.lock().await;
        let now = chrono::Utc::now().to_rfc3339();
        let rows = conn.execute(
            "UPDATE memories SET status = 'archived', updated_at = ?1 WHERE id = ?2 AND status = 'active'",
            rusqlite::params![now, memory_id],
        )?;
        if rows == 0 {
            return Err(MemoryStoreError::MemoryNotFound(memory_id.to_string()));
        }
        Ok(())
    }

    async fn supersede_memory(
        &self,
        old_id: &str,
        new_memory: &Memory,
        evidence: &[Evidence],
    ) -> Result<(), MemoryStoreError> {
        if evidence.is_empty() {
            return Err(MemoryStoreError::EvidenceRequired);
        }

        let conn = self.conn.lock().await;
        let now = Utc::now().to_rfc3339();

        // Mark old memory as superseded
        let rows = conn.execute(
            "UPDATE memories SET status = 'superseded', superseded_by = ?1, updated_at = ?2 WHERE id = ?3",
            rusqlite::params![new_memory.id, now, old_id],
        )?;
        if rows == 0 {
            return Err(MemoryStoreError::MemoryNotFound(old_id.to_string()));
        }
        let fsrs = new_memory
            .fsrs
            .clone()
            .unwrap_or_else(|| default_fsrs_state(new_memory));

        // Insert new memory
        conn.execute(
            "INSERT INTO memories (id, entity_id, fact, confidence, disposition, sensitivity, valid_from, valid_to, status, superseded_by, fsrs_stability, fsrs_difficulty, fsrs_last_access, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
            rusqlite::params![
                new_memory.id,
                new_memory.entity_id,
                new_memory.fact,
                new_memory.confidence,
                new_memory.disposition.as_str(),
                new_memory.sensitivity.as_str(),
                new_memory.valid_from,
                new_memory.valid_to,
                new_memory.status.as_str(),
                new_memory.superseded_by,
                fsrs.stability,
                fsrs.difficulty,
                fsrs.last_access,
                new_memory.created_at,
                new_memory.updated_at,
            ],
        )?;

        // Insert evidence
        for ev in evidence {
            conn.execute(
                "INSERT INTO evidence (id, memory_id, relationship_id, entity_id, article_id, source_url, evidence_quote, observed_at, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                rusqlite::params![
                    ev.id,
                    ev.memory_id,
                    ev.relationship_id,
                    ev.entity_id,
                    ev.article_id,
                    ev.source_url,
                    ev.evidence_quote,
                    ev.observed_at,
                    ev.created_at,
                ],
            )?;
        }

        Ok(())
    }

    async fn get_review_queue(&self, limit: usize) -> Result<Vec<Memory>, MemoryStoreError> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(
            "SELECT * FROM memories WHERE disposition = 'review_flagged' AND status = 'active' LIMIT ?1",
        )?;
        let results = stmt
            .query_map([limit as i64], row_to_memory)?
            .filter_map(|r| r.ok())
            .collect();
        Ok(results)
    }

    // --- Relationship operations ---

    async fn create_relationship(&self, rel: &Relationship) -> Result<(), MemoryStoreError> {
        let conn = self.conn.lock().await;

        // Verify both entities exist
        let from_exists: bool = conn
            .query_row(
                "SELECT COUNT(*) > 0 FROM entities WHERE id = ?1",
                [&rel.from_entity],
                |row| row.get(0),
            )
            .map_err(MemoryStoreError::from)?;
        if !from_exists {
            return Err(MemoryStoreError::EntityNotFound(rel.from_entity.clone()));
        }

        let to_exists: bool = conn
            .query_row(
                "SELECT COUNT(*) > 0 FROM entities WHERE id = ?1",
                [&rel.to_entity],
                |row| row.get(0),
            )
            .map_err(MemoryStoreError::from)?;
        if !to_exists {
            return Err(MemoryStoreError::EntityNotFound(rel.to_entity.clone()));
        }

        conn.execute(
            "INSERT INTO relationships (id, from_entity, to_entity, relation_type, strength, valid_from, valid_to, sensitivity, allowed_models, status, created_at, updated_at)
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

        Ok(())
    }

    async fn get_relationships(
        &self,
        entity_id: &str,
    ) -> Result<Vec<Relationship>, MemoryStoreError> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(
            "SELECT * FROM relationships WHERE (from_entity = ?1 OR to_entity = ?1) AND status = 'active'",
        )?;
        let results = stmt
            .query_map([entity_id], row_to_relationship)?
            .filter_map(|r| r.ok())
            .collect();
        Ok(results)
    }

    async fn get_relationships_by_type(
        &self,
        relation_type: &str,
        limit: usize,
    ) -> Result<Vec<Relationship>, MemoryStoreError> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(
            "SELECT * FROM relationships WHERE relation_type = ?1 AND status = 'active' LIMIT ?2",
        )?;
        let results = stmt
            .query_map(
                rusqlite::params![relation_type, limit as i64],
                row_to_relationship,
            )?
            .filter_map(|r| r.ok())
            .collect();
        Ok(results)
    }

    // --- Evidence operations ---

    async fn get_evidence(&self, memory_id: &str) -> Result<Vec<Evidence>, MemoryStoreError> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare("SELECT * FROM evidence WHERE memory_id = ?1")?;
        let results = stmt
            .query_map([memory_id], row_to_evidence)?
            .filter_map(|r| r.ok())
            .collect();
        Ok(results)
    }

    // --- Space-aware operations ---

    async fn find_entities_by_space(
        &self,
        space: MemorySpace,
        limit: usize,
    ) -> Result<Vec<Entity>, MemoryStoreError> {
        let conn = self.conn.lock().await;
        let mut stmt =
            conn.prepare("SELECT * FROM entities WHERE space = ?1 AND status = 'active' LIMIT ?2")?;
        let results = stmt
            .query_map(
                rusqlite::params![space.as_str(), limit as i64],
                row_to_entity,
            )?
            .filter_map(|r| r.ok())
            .collect();
        Ok(results)
    }

    async fn search_memories_in_space(
        &self,
        query: &str,
        space: MemorySpace,
        limit: usize,
    ) -> Result<Vec<Memory>, MemoryStoreError> {
        let conn = self.conn.lock().await;
        let fts_query = format!("\"{}\"", query.replace('"', "\"\""));
        let mut stmt = conn.prepare(
            "SELECT m.* FROM memories m
             JOIN memories_fts fts ON m.rowid = fts.rowid
             JOIN entities e ON m.entity_id = e.id
             WHERE memories_fts MATCH ?1
             AND m.status = 'active'
             AND e.space = ?2
             LIMIT ?3",
        )?;
        let results = stmt
            .query_map(
                rusqlite::params![fts_query, space.as_str(), limit as i64],
                row_to_memory,
            )?
            .filter_map(|r| r.ok())
            .collect();
        Ok(results)
    }

    // --- Recall ---

    async fn recall(
        &self,
        query: &str,
        sensitivity_max: Sensitivity,
        limit: usize,
    ) -> Result<Vec<MemoryRecallResult>, MemoryStoreError> {
        let conn = self.conn.lock().await;

        // Build list of allowed sensitivity levels
        let allowed: Vec<&str> = match sensitivity_max {
            Sensitivity::Shareable => vec!["shareable"],
            Sensitivity::Restricted => vec!["shareable", "restricted"],
            Sensitivity::Private => vec!["shareable", "restricted", "private"],
        };

        let fts_query = format!("\"{}\"", query.replace('"', "\"\""));

        // Search memories via FTS5, then group by entity
        let sql = format!(
            "SELECT m.* FROM memories m
             JOIN memories_fts fts ON m.rowid = fts.rowid
             WHERE memories_fts MATCH ?1
             AND m.status = 'active'
             AND m.sensitivity IN ({})
             LIMIT ?2",
            allowed
                .iter()
                .map(|s| format!("'{s}'"))
                .collect::<Vec<_>>()
                .join(", ")
        );

        let mut stmt = conn.prepare(&sql)?;
        let memories: Vec<Memory> = stmt
            .query_map(
                rusqlite::params![fts_query, (limit * 5) as i64],
                row_to_memory,
            )?
            .filter_map(|r| r.ok())
            .collect();

        // Group memories by entity_id
        let mut entity_memories: std::collections::HashMap<String, Vec<Memory>> =
            std::collections::HashMap::new();
        for mem in memories {
            entity_memories
                .entry(mem.entity_id.clone())
                .or_default()
                .push(mem);
        }

        let mut results = Vec::new();
        for (entity_id, mems) in entity_memories.into_iter().take(limit) {
            let entity = conn
                .query_row(
                    "SELECT * FROM entities WHERE id = ?1",
                    [&entity_id],
                    row_to_entity,
                )
                .map_err(MemoryStoreError::from)?;

            // Collect evidence for these memories
            let mut all_evidence = Vec::new();
            for mem in &mems {
                let mut ev_stmt = conn.prepare("SELECT * FROM evidence WHERE memory_id = ?1")?;
                let evs: Vec<Evidence> = ev_stmt
                    .query_map([&mem.id], row_to_evidence)?
                    .filter_map(|r| r.ok())
                    .collect();
                all_evidence.extend(evs);
            }

            // Use average confidence as relevance score
            let avg_confidence = mems.iter().map(|m| m.confidence).sum::<f64>() / mems.len() as f64;

            results.push(MemoryRecallResult {
                entity,
                memories: mems,
                evidence: all_evidence,
                relevance_score: avg_confidence,
            });
        }

        // Sort by relevance descending
        results.sort_by(|a, b| {
            b.relevance_score
                .partial_cmp(&a.relevance_score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        Ok(results)
    }

    async fn recall_in_space(
        &self,
        query: &str,
        sensitivity_max: Sensitivity,
        space: MemorySpace,
        limit: usize,
    ) -> Result<Vec<MemoryRecallResult>, MemoryStoreError> {
        let conn = self.conn.lock().await;

        let allowed: Vec<&str> = match sensitivity_max {
            Sensitivity::Shareable => vec!["shareable"],
            Sensitivity::Restricted => vec!["shareable", "restricted"],
            Sensitivity::Private => vec!["shareable", "restricted", "private"],
        };

        let fts_query = format!("\"{}\"", query.replace('"', "\"\""));

        let sql = format!(
            "SELECT m.* FROM memories m
             JOIN memories_fts fts ON m.rowid = fts.rowid
             JOIN entities e ON m.entity_id = e.id
             WHERE memories_fts MATCH ?1
             AND m.status = 'active'
             AND m.sensitivity IN ({})
             AND e.space = ?2
             LIMIT ?3",
            allowed
                .iter()
                .map(|s| format!("'{s}'"))
                .collect::<Vec<_>>()
                .join(", ")
        );

        let mut stmt = conn.prepare(&sql)?;
        let memories: Vec<Memory> = stmt
            .query_map(
                rusqlite::params![fts_query, space.as_str(), (limit * 5) as i64],
                row_to_memory,
            )?
            .filter_map(|r| r.ok())
            .collect();

        let mut entity_memories: std::collections::HashMap<String, Vec<Memory>> =
            std::collections::HashMap::new();
        for mem in memories {
            entity_memories
                .entry(mem.entity_id.clone())
                .or_default()
                .push(mem);
        }

        let mut results = Vec::new();
        for (entity_id, mems) in entity_memories.into_iter().take(limit) {
            let entity = conn
                .query_row(
                    "SELECT * FROM entities WHERE id = ?1",
                    [&entity_id],
                    row_to_entity,
                )
                .map_err(MemoryStoreError::from)?;

            let mut all_evidence = Vec::new();
            for mem in &mems {
                let mut ev_stmt = conn.prepare("SELECT * FROM evidence WHERE memory_id = ?1")?;
                let evs: Vec<Evidence> = ev_stmt
                    .query_map([&mem.id], row_to_evidence)?
                    .filter_map(|r| r.ok())
                    .collect();
                all_evidence.extend(evs);
            }

            let avg_confidence = mems.iter().map(|m| m.confidence).sum::<f64>() / mems.len() as f64;

            results.push(MemoryRecallResult {
                entity,
                memories: mems,
                evidence: all_evidence,
                relevance_score: avg_confidence,
            });
        }

        results.sort_by(|a, b| {
            b.relevance_score
                .partial_cmp(&a.relevance_score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        Ok(results)
    }

    async fn recall_cross_space(
        &self,
        query: &str,
        sensitivity_max: Sensitivity,
        limit: usize,
    ) -> Result<Vec<MemoryRecallResult>, MemoryStoreError> {
        // Cross-space recall searches all spaces, returning results from each.
        // We do a single unified query (no space filter) and let the entity's
        // space field annotate which space each result came from.
        self.recall(query, sensitivity_max, limit).await
    }

    // --- Link operations (wikilink graph) ---

    async fn create_link(&self, link: &EntityLink) -> Result<(), MemoryStoreError> {
        let conn = self.conn.lock().await;

        conn.execute(
            "INSERT OR IGNORE INTO links (id, source_entity_id, target_entity_id, link_text, context, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                link.id,
                link.source_entity_id,
                link.target_entity_id,
                link.link_text,
                link.context,
                link.created_at,
            ],
        )?;

        Ok(())
    }

    async fn get_outgoing_links(
        &self,
        entity_id: &str,
    ) -> Result<Vec<EntityLink>, MemoryStoreError> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare("SELECT * FROM links WHERE source_entity_id = ?1")?;
        let results = stmt
            .query_map([entity_id], row_to_entity_link)?
            .filter_map(|r| r.ok())
            .collect();
        Ok(results)
    }

    async fn get_backlinks(&self, entity_id: &str) -> Result<Vec<EntityLink>, MemoryStoreError> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare("SELECT * FROM links WHERE target_entity_id = ?1")?;
        let results = stmt
            .query_map([entity_id], row_to_entity_link)?
            .filter_map(|r| r.ok())
            .collect();
        Ok(results)
    }
}

pub(crate) fn default_fsrs_state(memory: &Memory) -> FsrsState {
    let last_access = chrono::DateTime::parse_from_rfc3339(&memory.updated_at)
        .ok()
        .and_then(|dt| u64::try_from(dt.timestamp()).ok())
        .unwrap_or_else(now_unix);
    FsrsState {
        stability: 30.0,
        difficulty: (1.0 - memory.confidence).clamp(0.0, 1.0),
        last_access,
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use crate::sqlite::SqliteMemoryStore;
    use crate::store::MemoryStore;
    use crate::types::*;

    fn now() -> String {
        Utc::now().to_rfc3339()
    }

    fn make_entity(name: &str, entity_type: EntityType) -> Entity {
        let ts = now();
        Entity {
            id: uuid::Uuid::new_v4().to_string(),
            entity_type,
            name: name.to_string(),
            attributes: serde_json::json!({}),
            sensitivity: Sensitivity::Private,
            allowed_models: AllowedModels::LocalOnly,
            space: MemorySpace::Knowledge,
            status: EntityStatus::Active,
            merged_into: None,
            created_at: ts.clone(),
            updated_at: ts,
        }
    }

    fn make_entity_in_space(name: &str, entity_type: EntityType, space: MemorySpace) -> Entity {
        let ts = now();
        Entity {
            id: uuid::Uuid::new_v4().to_string(),
            entity_type,
            name: name.to_string(),
            attributes: serde_json::json!({}),
            sensitivity: Sensitivity::Private,
            allowed_models: AllowedModels::LocalOnly,
            space,
            status: EntityStatus::Active,
            merged_into: None,
            created_at: ts.clone(),
            updated_at: ts,
        }
    }

    fn make_memory(entity_id: &str, fact: &str) -> Memory {
        let ts = now();
        Memory {
            id: uuid::Uuid::new_v4().to_string(),
            entity_id: entity_id.to_string(),
            fact: fact.to_string(),
            confidence: 0.9,
            disposition: FactDisposition::AutoStored,
            sensitivity: Sensitivity::Private,
            valid_from: ts.clone(),
            valid_to: None,
            status: MemoryStatus::Active,
            superseded_by: None,
            created_at: ts.clone(),
            updated_at: ts,
            fact_type: None,
            authored_by: None,
            supersedes: None,
            depends_on: Vec::new(),
            fsrs: None,
        }
    }

    fn make_evidence(memory_id: &str) -> Evidence {
        let ts = now();
        Evidence {
            id: uuid::Uuid::new_v4().to_string(),
            memory_id: Some(memory_id.to_string()),
            relationship_id: None,
            entity_id: None,
            article_id: Some("art-001".to_string()),
            source_url: Some("https://example.com".to_string()),
            evidence_quote: Some("supporting text".to_string()),
            observed_at: ts.clone(),
            created_at: ts,
        }
    }

    fn make_relationship(from_id: &str, to_id: &str, rel_type: &str) -> Relationship {
        let ts = now();
        Relationship {
            id: uuid::Uuid::new_v4().to_string(),
            from_entity: from_id.to_string(),
            to_entity: to_id.to_string(),
            relation_type: rel_type.to_string(),
            strength: 0.8,
            valid_from: ts.clone(),
            valid_to: None,
            sensitivity: Sensitivity::Private,
            allowed_models: AllowedModels::LocalOnly,
            status: RelationshipStatus::Active,
            created_at: ts.clone(),
            updated_at: ts,
        }
    }

    async fn setup_store() -> SqliteMemoryStore {
        let store = SqliteMemoryStore::open_in_memory().unwrap();
        store.initialize().await.unwrap();
        store
    }

    // --- Schema tests ---

    #[tokio::test]
    async fn schema_creation_succeeds() {
        let store = SqliteMemoryStore::open_in_memory().unwrap();
        store.initialize().await.unwrap();
    }

    #[tokio::test]
    async fn schema_idempotent() {
        let store = SqliteMemoryStore::open_in_memory().unwrap();
        store.initialize().await.unwrap();
        store.initialize().await.unwrap();
    }

    // --- Entity CRUD ---

    #[tokio::test]
    async fn create_and_get_entity() {
        let store = setup_store().await;
        let entity = make_entity("Alice", EntityType::Person);
        store.create_entity(&entity).await.unwrap();
        let fetched = store.get_entity(&entity.id).await.unwrap();
        assert_eq!(fetched.id, entity.id);
        assert_eq!(fetched.name, "Alice");
        assert_eq!(fetched.entity_type, EntityType::Person);
    }

    #[tokio::test]
    async fn get_entity_not_found() {
        let store = setup_store().await;
        let result = store.get_entity("nonexistent").await;
        assert!(matches!(result, Err(MemoryStoreError::EntityNotFound(_))));
    }

    #[tokio::test]
    async fn find_entities_by_type() {
        let store = setup_store().await;
        let alice = make_entity("Alice", EntityType::Person);
        let rust = make_entity("Rust", EntityType::Tool);
        store.create_entity(&alice).await.unwrap();
        store.create_entity(&rust).await.unwrap();

        let people = store
            .find_entities_by_type(EntityType::Person, 10)
            .await
            .unwrap();
        assert_eq!(people.len(), 1);
        assert_eq!(people[0].name, "Alice");

        let tools = store
            .find_entities_by_type(EntityType::Tool, 10)
            .await
            .unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "Rust");
    }

    #[tokio::test]
    async fn update_entity_attributes() {
        let store = setup_store().await;
        let entity = make_entity("Alice", EntityType::Person);
        store.create_entity(&entity).await.unwrap();

        let new_attrs = serde_json::json!({"role": "developer"});
        store
            .update_entity(&entity.id, new_attrs.clone())
            .await
            .unwrap();

        let fetched = store.get_entity(&entity.id).await.unwrap();
        assert_eq!(fetched.attributes, new_attrs);
    }

    #[tokio::test]
    async fn update_nonexistent_entity_fails() {
        let store = setup_store().await;
        let result = store
            .update_entity("nonexistent", serde_json::json!({}))
            .await;
        assert!(matches!(result, Err(MemoryStoreError::EntityNotFound(_))));
    }

    // --- FTS5 entity search ---

    #[tokio::test]
    async fn fts5_find_entities_by_name() {
        let store = setup_store().await;
        let alice = make_entity("Alice Smith", EntityType::Person);
        let bob = make_entity("Bob Jones", EntityType::Person);
        store.create_entity(&alice).await.unwrap();
        store.create_entity(&bob).await.unwrap();

        let results = store.find_entities("Alice", 10).await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "Alice Smith");
    }

    // --- Entity deduplication ---

    #[tokio::test]
    async fn duplicate_entity_exact_name_rejected() {
        let store = setup_store().await;
        let alice1 = make_entity("Alice", EntityType::Person);
        store.create_entity(&alice1).await.unwrap();

        let alice2 = make_entity("Alice", EntityType::Person);
        let result = store.create_entity(&alice2).await;
        assert!(matches!(
            result,
            Err(MemoryStoreError::DuplicateEntity { .. })
        ));
    }

    #[tokio::test]
    async fn duplicate_entity_normalized_name_rejected() {
        let store = setup_store().await;
        let alice1 = make_entity("Alice", EntityType::Person);
        store.create_entity(&alice1).await.unwrap();

        let alice2 = make_entity("  alice  ", EntityType::Person);
        let result = store.create_entity(&alice2).await;
        assert!(matches!(
            result,
            Err(MemoryStoreError::DuplicateEntity { .. })
        ));
    }

    #[tokio::test]
    async fn duplicate_entity_fuzzy_match_rejected() {
        let store = setup_store().await;
        let alice = make_entity("Alice", EntityType::Person);
        store.create_entity(&alice).await.unwrap();

        // "Alise" is within edit distance 2 of "alice"
        let alise = make_entity("Alise", EntityType::Person);
        let result = store.create_entity(&alise).await;
        assert!(matches!(
            result,
            Err(MemoryStoreError::DuplicateEntity { .. })
        ));
    }

    #[tokio::test]
    async fn different_types_same_name_allowed() {
        let store = setup_store().await;
        let rust_tool = make_entity("Rust", EntityType::Tool);
        let rust_concept = make_entity("Rust", EntityType::Concept);
        store.create_entity(&rust_tool).await.unwrap();
        store.create_entity(&rust_concept).await.unwrap();
    }

    // --- Entity merge ---

    #[tokio::test]
    async fn merge_entities_reassigns_memories_and_relationships() {
        let store = setup_store().await;

        let alice = make_entity("Alice", EntityType::Person);
        let bob = make_entity("Bob", EntityType::Person);
        let project = make_entity("Project X", EntityType::Project);
        store.create_entity(&alice).await.unwrap();
        store.create_entity(&bob).await.unwrap();
        store.create_entity(&project).await.unwrap();

        let mem = make_memory(&alice.id, "Alice works at Acme");
        let ev = make_evidence(&mem.id);
        store.create_memory(&mem, &[ev]).await.unwrap();

        let rel = make_relationship(&alice.id, &project.id, "works_on");
        store.create_relationship(&rel).await.unwrap();

        store.merge_entities(&alice.id, &bob.id).await.unwrap();

        let merged = store.get_entity(&alice.id).await.unwrap();
        assert_eq!(merged.status, EntityStatus::Merged);
        assert_eq!(merged.merged_into, Some(bob.id.clone()));

        let bob_memories = store.get_memories(&bob.id, None).await.unwrap();
        assert_eq!(bob_memories.len(), 1);
        assert_eq!(bob_memories[0].fact, "Alice works at Acme");

        let bob_rels = store.get_relationships(&bob.id).await.unwrap();
        assert_eq!(bob_rels.len(), 1);
        assert_eq!(bob_rels[0].from_entity, bob.id);
    }

    #[tokio::test]
    async fn merge_nonexistent_source_fails() {
        let store = setup_store().await;
        let bob = make_entity("Bob", EntityType::Person);
        store.create_entity(&bob).await.unwrap();

        let result = store.merge_entities("nonexistent", &bob.id).await;
        assert!(matches!(result, Err(MemoryStoreError::EntityNotFound(_))));
    }

    // --- Memory CRUD ---

    #[tokio::test]
    async fn create_memory_with_evidence() {
        let store = setup_store().await;
        let entity = make_entity("Alice", EntityType::Person);
        store.create_entity(&entity).await.unwrap();

        let mem = make_memory(&entity.id, "Alice prefers Rust");
        let ev = make_evidence(&mem.id);
        store.create_memory(&mem, &[ev]).await.unwrap();

        let memories = store.get_memories(&entity.id, None).await.unwrap();
        assert_eq!(memories.len(), 1);
        assert_eq!(memories[0].fact, "Alice prefers Rust");
    }

    #[tokio::test]
    async fn create_memory_without_evidence_fails() {
        let store = setup_store().await;
        let entity = make_entity("Alice", EntityType::Person);
        store.create_entity(&entity).await.unwrap();

        let mem = make_memory(&entity.id, "Alice prefers Rust");
        let result = store.create_memory(&mem, &[]).await;
        assert!(matches!(result, Err(MemoryStoreError::EvidenceRequired)));
    }

    #[tokio::test]
    async fn create_memory_for_nonexistent_entity_fails() {
        let store = setup_store().await;
        let mem = make_memory("nonexistent", "some fact");
        let ev = make_evidence(&mem.id);
        let result = store.create_memory(&mem, &[ev]).await;
        assert!(matches!(result, Err(MemoryStoreError::EntityNotFound(_))));
    }

    #[tokio::test]
    async fn get_memories_filtered_by_status() {
        let store = setup_store().await;
        let entity = make_entity("Alice", EntityType::Person);
        store.create_entity(&entity).await.unwrap();

        let mem1 = make_memory(&entity.id, "Fact one");
        let ev1 = make_evidence(&mem1.id);
        store.create_memory(&mem1, &[ev1]).await.unwrap();

        let mem2 = make_memory(&entity.id, "Fact two (updated)");
        let ev2 = make_evidence(&mem2.id);
        store
            .supersede_memory(&mem1.id, &mem2, &[ev2])
            .await
            .unwrap();

        let active = store
            .get_memories(&entity.id, Some(MemoryStatus::Active))
            .await
            .unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].fact, "Fact two (updated)");

        let superseded = store
            .get_memories(&entity.id, Some(MemoryStatus::Superseded))
            .await
            .unwrap();
        assert_eq!(superseded.len(), 1);
        assert_eq!(superseded[0].fact, "Fact one");
    }

    // --- FTS5 memory search ---

    #[tokio::test]
    async fn fts5_search_memories() {
        let store = setup_store().await;
        let entity = make_entity("Alice", EntityType::Person);
        store.create_entity(&entity).await.unwrap();

        let mem1 = make_memory(&entity.id, "Alice uses Rust for backend development");
        let ev1 = make_evidence(&mem1.id);
        store.create_memory(&mem1, &[ev1]).await.unwrap();

        let mem2 = make_memory(&entity.id, "Alice enjoys hiking on weekends");
        let ev2 = make_evidence(&mem2.id);
        store.create_memory(&mem2, &[ev2]).await.unwrap();

        let results = store.search_memories("Rust", 10).await.unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].fact.contains("Rust"));
    }

    // --- Supersede memory ---

    #[tokio::test]
    async fn supersede_memory_links_old_to_new() {
        let store = setup_store().await;
        let entity = make_entity("Alice", EntityType::Person);
        store.create_entity(&entity).await.unwrap();

        let old_mem = make_memory(&entity.id, "Alice uses Python");
        let old_ev = make_evidence(&old_mem.id);
        store.create_memory(&old_mem, &[old_ev]).await.unwrap();

        let new_mem = make_memory(&entity.id, "Alice switched to Rust");
        let new_ev = make_evidence(&new_mem.id);
        store
            .supersede_memory(&old_mem.id, &new_mem, &[new_ev])
            .await
            .unwrap();

        let old_fetched = store
            .get_memories(&entity.id, Some(MemoryStatus::Superseded))
            .await
            .unwrap();
        assert_eq!(old_fetched.len(), 1);
        assert_eq!(old_fetched[0].superseded_by, Some(new_mem.id.clone()));

        let new_fetched = store
            .get_memories(&entity.id, Some(MemoryStatus::Active))
            .await
            .unwrap();
        assert_eq!(new_fetched.len(), 1);
        assert_eq!(new_fetched[0].fact, "Alice switched to Rust");
    }

    #[tokio::test]
    async fn supersede_nonexistent_memory_fails() {
        let store = setup_store().await;
        let entity = make_entity("Alice", EntityType::Person);
        store.create_entity(&entity).await.unwrap();

        let new_mem = make_memory(&entity.id, "new fact");
        let ev = make_evidence(&new_mem.id);
        let result = store.supersede_memory("nonexistent", &new_mem, &[ev]).await;
        assert!(matches!(result, Err(MemoryStoreError::MemoryNotFound(_))));
    }

    // --- Review queue ---

    #[tokio::test]
    async fn review_queue_returns_only_flagged() {
        let store = setup_store().await;
        let entity = make_entity("Alice", EntityType::Person);
        store.create_entity(&entity).await.unwrap();

        let mem1 = make_memory(&entity.id, "auto fact");
        let ev1 = make_evidence(&mem1.id);
        store.create_memory(&mem1, &[ev1]).await.unwrap();

        let ts = now();
        let mem2 = Memory {
            id: uuid::Uuid::new_v4().to_string(),
            entity_id: entity.id.clone(),
            fact: "needs review".to_string(),
            confidence: 0.5,
            disposition: FactDisposition::ReviewFlagged,
            sensitivity: Sensitivity::Private,
            valid_from: ts.clone(),
            valid_to: None,
            status: MemoryStatus::Active,
            superseded_by: None,
            created_at: ts.clone(),
            updated_at: ts,
            fact_type: None,
            authored_by: None,
            supersedes: None,
            depends_on: Vec::new(),
            fsrs: None,
        };
        let ev2 = make_evidence(&mem2.id);
        store.create_memory(&mem2, &[ev2]).await.unwrap();

        let queue = store.get_review_queue(10).await.unwrap();
        assert_eq!(queue.len(), 1);
        assert_eq!(queue[0].fact, "needs review");
    }

    // --- Relationship operations ---

    #[tokio::test]
    async fn create_and_get_relationships() {
        let store = setup_store().await;
        let alice = make_entity("Alice", EntityType::Person);
        let bob = make_entity("Bob", EntityType::Person);
        store.create_entity(&alice).await.unwrap();
        store.create_entity(&bob).await.unwrap();

        let rel = make_relationship(&alice.id, &bob.id, "works_with");
        store.create_relationship(&rel).await.unwrap();

        let alice_rels = store.get_relationships(&alice.id).await.unwrap();
        assert_eq!(alice_rels.len(), 1);

        let bob_rels = store.get_relationships(&bob.id).await.unwrap();
        assert_eq!(bob_rels.len(), 1);
    }

    #[tokio::test]
    async fn create_relationship_nonexistent_entity_fails() {
        let store = setup_store().await;
        let alice = make_entity("Alice", EntityType::Person);
        store.create_entity(&alice).await.unwrap();

        let rel = make_relationship(&alice.id, "nonexistent", "works_with");
        let result = store.create_relationship(&rel).await;
        assert!(matches!(result, Err(MemoryStoreError::EntityNotFound(_))));
    }

    #[tokio::test]
    async fn get_relationships_by_type() {
        let store = setup_store().await;
        let alice = make_entity("Alice", EntityType::Person);
        let bob = make_entity("Bob", EntityType::Person);
        let project = make_entity("Project X", EntityType::Project);
        store.create_entity(&alice).await.unwrap();
        store.create_entity(&bob).await.unwrap();
        store.create_entity(&project).await.unwrap();

        let works_with = make_relationship(&alice.id, &bob.id, "works_with");
        let owns = make_relationship(&alice.id, &project.id, "owns");
        store.create_relationship(&works_with).await.unwrap();
        store.create_relationship(&owns).await.unwrap();

        let result = store.get_relationships_by_type("owns", 10).await.unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].relation_type, "owns");
    }

    // --- Evidence ---

    #[tokio::test]
    async fn get_evidence_for_memory() {
        let store = setup_store().await;
        let entity = make_entity("Alice", EntityType::Person);
        store.create_entity(&entity).await.unwrap();

        let mem = make_memory(&entity.id, "Alice likes Rust");
        let ev1 = make_evidence(&mem.id);
        let ts = now();
        let ev2 = Evidence {
            id: uuid::Uuid::new_v4().to_string(),
            memory_id: Some(mem.id.clone()),
            relationship_id: None,
            entity_id: None,
            article_id: Some("art-002".to_string()),
            source_url: None,
            evidence_quote: Some("second source".to_string()),
            observed_at: ts.clone(),
            created_at: ts,
        };
        store.create_memory(&mem, &[ev1, ev2]).await.unwrap();

        let evidence = store.get_evidence(&mem.id).await.unwrap();
        assert_eq!(evidence.len(), 2);
    }

    // --- Recall (sensitivity filtering) ---

    #[tokio::test]
    async fn recall_filters_by_sensitivity() {
        let store = setup_store().await;
        let entity = make_entity("Alice", EntityType::Person);
        store.create_entity(&entity).await.unwrap();

        let ts = now();
        let shareable_mem = Memory {
            id: uuid::Uuid::new_v4().to_string(),
            entity_id: entity.id.clone(),
            fact: "Alice is a Rust developer".to_string(),
            confidence: 0.9,
            disposition: FactDisposition::AutoStored,
            sensitivity: Sensitivity::Shareable,
            valid_from: ts.clone(),
            valid_to: None,
            status: MemoryStatus::Active,
            superseded_by: None,
            created_at: ts.clone(),
            updated_at: ts.clone(),
            fact_type: None,
            authored_by: None,
            supersedes: None,
            depends_on: Vec::new(),
            fsrs: None,
        };
        let ev1 = make_evidence(&shareable_mem.id);
        store.create_memory(&shareable_mem, &[ev1]).await.unwrap();

        let private_mem = Memory {
            id: uuid::Uuid::new_v4().to_string(),
            entity_id: entity.id.clone(),
            fact: "Alice earns a high salary from Rust consulting".to_string(),
            confidence: 0.8,
            disposition: FactDisposition::AutoStored,
            sensitivity: Sensitivity::Private,
            valid_from: ts.clone(),
            valid_to: None,
            status: MemoryStatus::Active,
            superseded_by: None,
            created_at: ts.clone(),
            updated_at: ts,
            fact_type: None,
            authored_by: None,
            supersedes: None,
            depends_on: Vec::new(),
            fsrs: None,
        };
        let ev2 = make_evidence(&private_mem.id);
        store.create_memory(&private_mem, &[ev2]).await.unwrap();

        // Shareable max -> only shareable memories
        let shareable_results = store
            .recall("Rust", Sensitivity::Shareable, 10)
            .await
            .unwrap();
        let shareable_facts: Vec<&str> = shareable_results
            .iter()
            .flat_map(|r| r.memories.iter().map(|m| m.fact.as_str()))
            .collect();
        assert!(shareable_facts.contains(&"Alice is a Rust developer"));
        assert!(!shareable_facts.iter().any(|f| f.contains("salary")));

        // Private max -> both
        let private_results = store
            .recall("Rust", Sensitivity::Private, 10)
            .await
            .unwrap();
        let all_facts: Vec<&str> = private_results
            .iter()
            .flat_map(|r| r.memories.iter().map(|m| m.fact.as_str()))
            .collect();
        assert!(all_facts.len() >= 2);
    }

    // --- Memory space tests ---

    #[tokio::test]
    async fn entity_default_space_is_knowledge() {
        let store = setup_store().await;
        let entity = make_entity("Alice", EntityType::Person);
        store.create_entity(&entity).await.unwrap();

        let fetched = store.get_entity(&entity.id).await.unwrap();
        assert_eq!(fetched.space, MemorySpace::Knowledge);
    }

    #[tokio::test]
    async fn create_entity_in_self_space() {
        let store = setup_store().await;
        let entity =
            make_entity_in_space("Preferences", EntityType::Preference, MemorySpace::Identity);
        store.create_entity(&entity).await.unwrap();

        let fetched = store.get_entity(&entity.id).await.unwrap();
        assert_eq!(fetched.space, MemorySpace::Identity);
    }

    #[tokio::test]
    async fn create_entity_in_methodology_space() {
        let store = setup_store().await;
        let entity =
            make_entity_in_space("Git Workflow", EntityType::Task, MemorySpace::Operations);
        store.create_entity(&entity).await.unwrap();

        let fetched = store.get_entity(&entity.id).await.unwrap();
        assert_eq!(fetched.space, MemorySpace::Operations);
    }

    #[tokio::test]
    async fn find_entities_by_space() {
        let store = setup_store().await;

        let knowledge_entity = make_entity("Rust Facts", EntityType::Concept);
        let self_entity = make_entity_in_space(
            "My Preferences",
            EntityType::Preference,
            MemorySpace::Identity,
        );
        let method_entity =
            make_entity_in_space("Deploy Process", EntityType::Task, MemorySpace::Operations);
        store.create_entity(&knowledge_entity).await.unwrap();
        store.create_entity(&self_entity).await.unwrap();
        store.create_entity(&method_entity).await.unwrap();

        let knowledge = store
            .find_entities_by_space(MemorySpace::Knowledge, 10)
            .await
            .unwrap();
        assert_eq!(knowledge.len(), 1);
        assert_eq!(knowledge[0].name, "Rust Facts");

        let self_results = store
            .find_entities_by_space(MemorySpace::Identity, 10)
            .await
            .unwrap();
        assert_eq!(self_results.len(), 1);
        assert_eq!(self_results[0].name, "My Preferences");

        let methodology = store
            .find_entities_by_space(MemorySpace::Operations, 10)
            .await
            .unwrap();
        assert_eq!(methodology.len(), 1);
        assert_eq!(methodology[0].name, "Deploy Process");
    }

    #[tokio::test]
    async fn find_entities_by_space_empty() {
        let store = setup_store().await;

        let entity = make_entity("Rust Facts", EntityType::Concept);
        store.create_entity(&entity).await.unwrap();

        let results = store
            .find_entities_by_space(MemorySpace::Identity, 10)
            .await
            .unwrap();
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn search_memories_in_space_filters_correctly() {
        let store = setup_store().await;

        let knowledge_entity = make_entity("Rust", EntityType::Concept);
        let self_entity =
            make_entity_in_space("Preferences", EntityType::Preference, MemorySpace::Identity);
        store.create_entity(&knowledge_entity).await.unwrap();
        store.create_entity(&self_entity).await.unwrap();

        let km = make_memory(&knowledge_entity.id, "Rust has zero-cost abstractions");
        let ke = make_evidence(&km.id);
        store.create_memory(&km, &[ke]).await.unwrap();

        let sm = make_memory(&self_entity.id, "I prefer Rust over Go");
        let se = make_evidence(&sm.id);
        store.create_memory(&sm, &[se]).await.unwrap();

        // Search for "Rust" in knowledge space only
        let knowledge_results = store
            .search_memories_in_space("Rust", MemorySpace::Knowledge, 10)
            .await
            .unwrap();
        assert_eq!(knowledge_results.len(), 1);
        assert!(knowledge_results[0].fact.contains("zero-cost"));

        // Search for "Rust" in self space only
        let self_results = store
            .search_memories_in_space("Rust", MemorySpace::Identity, 10)
            .await
            .unwrap();
        assert_eq!(self_results.len(), 1);
        assert!(self_results[0].fact.contains("prefer"));

        // Search for "Rust" in methodology space (should be empty)
        let meth_results = store
            .search_memories_in_space("Rust", MemorySpace::Operations, 10)
            .await
            .unwrap();
        assert!(meth_results.is_empty());
    }

    #[tokio::test]
    async fn recall_in_space_filters_by_space_and_sensitivity() {
        let store = setup_store().await;

        let knowledge_entity = make_entity("Rust", EntityType::Concept);
        let self_entity =
            make_entity_in_space("Prefs", EntityType::Preference, MemorySpace::Identity);
        store.create_entity(&knowledge_entity).await.unwrap();
        store.create_entity(&self_entity).await.unwrap();

        let ts = now();
        let km = Memory {
            id: uuid::Uuid::new_v4().to_string(),
            entity_id: knowledge_entity.id.clone(),
            fact: "Rust is memory safe".to_string(),
            confidence: 0.9,
            disposition: FactDisposition::AutoStored,
            sensitivity: Sensitivity::Shareable,
            valid_from: ts.clone(),
            valid_to: None,
            status: MemoryStatus::Active,
            superseded_by: None,
            created_at: ts.clone(),
            updated_at: ts.clone(),
            fact_type: None,
            authored_by: None,
            supersedes: None,
            depends_on: Vec::new(),
            fsrs: None,
        };
        let ke = make_evidence(&km.id);
        store.create_memory(&km, &[ke]).await.unwrap();

        let sm = Memory {
            id: uuid::Uuid::new_v4().to_string(),
            entity_id: self_entity.id.clone(),
            fact: "I enjoy Rust programming".to_string(),
            confidence: 0.8,
            disposition: FactDisposition::AutoStored,
            sensitivity: Sensitivity::Private,
            valid_from: ts.clone(),
            valid_to: None,
            status: MemoryStatus::Active,
            superseded_by: None,
            created_at: ts.clone(),
            updated_at: ts,
            fact_type: None,
            authored_by: None,
            supersedes: None,
            depends_on: Vec::new(),
            fsrs: None,
        };
        let se = make_evidence(&sm.id);
        store.create_memory(&sm, &[se]).await.unwrap();

        // Recall in knowledge space
        let knowledge_results = store
            .recall_in_space("Rust", Sensitivity::Private, MemorySpace::Knowledge, 10)
            .await
            .unwrap();
        assert_eq!(knowledge_results.len(), 1);
        assert_eq!(knowledge_results[0].entity.space, MemorySpace::Knowledge);

        // Recall in self space
        let self_results = store
            .recall_in_space("Rust", Sensitivity::Private, MemorySpace::Identity, 10)
            .await
            .unwrap();
        assert_eq!(self_results.len(), 1);
        assert_eq!(self_results[0].entity.space, MemorySpace::Identity);

        // Recall in self space with shareable sensitivity (should filter out private)
        let shareable_self = store
            .recall_in_space("Rust", Sensitivity::Shareable, MemorySpace::Identity, 10)
            .await
            .unwrap();
        assert!(shareable_self.is_empty());
    }

    #[tokio::test]
    async fn recall_cross_space_returns_all_spaces() {
        let store = setup_store().await;

        let knowledge_entity = make_entity("Rust", EntityType::Concept);
        let self_entity =
            make_entity_in_space("Prefs", EntityType::Preference, MemorySpace::Identity);
        store.create_entity(&knowledge_entity).await.unwrap();
        store.create_entity(&self_entity).await.unwrap();

        let km = make_memory(&knowledge_entity.id, "Rust has ownership model");
        let ke = make_evidence(&km.id);
        store.create_memory(&km, &[ke]).await.unwrap();

        let sm = make_memory(&self_entity.id, "I prefer Rust borrow checker");
        let se = make_evidence(&sm.id);
        store.create_memory(&sm, &[se]).await.unwrap();

        let results = store
            .recall_cross_space("Rust", Sensitivity::Private, 10)
            .await
            .unwrap();

        // Should find results from both spaces
        let spaces: Vec<MemorySpace> = results.iter().map(|r| r.entity.space).collect();
        assert!(spaces.contains(&MemorySpace::Knowledge));
        assert!(spaces.contains(&MemorySpace::Identity));
    }

    #[tokio::test]
    async fn space_field_persists_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("memory_spaces.db");

        {
            let store = SqliteMemoryStore::open(&db_path).unwrap();
            store.initialize().await.unwrap();
            let entity =
                make_entity_in_space("My Skills", EntityType::Task, MemorySpace::Operations);
            store.create_entity(&entity).await.unwrap();
        }

        {
            let store = SqliteMemoryStore::open(&db_path).unwrap();
            store.initialize().await.unwrap();
            let results = store
                .find_entities_by_space(MemorySpace::Operations, 10)
                .await
                .unwrap();
            assert_eq!(results.len(), 1);
            assert_eq!(results[0].name, "My Skills");
            assert_eq!(results[0].space, MemorySpace::Operations);
        }
    }

    #[tokio::test]
    async fn same_name_different_spaces_allowed() {
        let store = setup_store().await;

        // "Rust" in knowledge space
        let knowledge_entity = make_entity("Rust", EntityType::Concept);
        // "Rust" in methodology space (different entity type to avoid dedup)
        let method_entity = make_entity_in_space("Rust", EntityType::Task, MemorySpace::Operations);

        store.create_entity(&knowledge_entity).await.unwrap();
        store.create_entity(&method_entity).await.unwrap();

        let k_results = store
            .find_entities_by_space(MemorySpace::Knowledge, 10)
            .await
            .unwrap();
        assert_eq!(k_results.len(), 1);

        let m_results = store
            .find_entities_by_space(MemorySpace::Operations, 10)
            .await
            .unwrap();
        assert_eq!(m_results.len(), 1);
    }

    // --- Link operations (wikilink graph) ---

    fn make_link(source_id: &str, target_id: &str, link_text: &str, context: &str) -> EntityLink {
        let ts = now();
        EntityLink {
            id: uuid::Uuid::new_v4().to_string(),
            source_entity_id: source_id.to_string(),
            target_entity_id: target_id.to_string(),
            link_text: link_text.to_string(),
            context: context.to_string(),
            created_at: ts,
        }
    }

    #[tokio::test]
    async fn create_and_get_outgoing_links() {
        let store = setup_store().await;
        let alice = make_entity("Alice", EntityType::Person);
        let bob = make_entity("Bob", EntityType::Person);
        store.create_entity(&alice).await.unwrap();
        store.create_entity(&bob).await.unwrap();

        let link = make_link(&alice.id, &bob.id, "Bob", "See [[Bob]] for details");
        store.create_link(&link).await.unwrap();

        let outgoing = store.get_outgoing_links(&alice.id).await.unwrap();
        assert_eq!(outgoing.len(), 1);
        assert_eq!(outgoing[0].source_entity_id, alice.id);
        assert_eq!(outgoing[0].target_entity_id, bob.id);
        assert_eq!(outgoing[0].link_text, "Bob");
    }

    #[tokio::test]
    async fn create_and_get_backlinks() {
        let store = setup_store().await;
        let alice = make_entity("Alice", EntityType::Person);
        let bob = make_entity("Bob", EntityType::Person);
        store.create_entity(&alice).await.unwrap();
        store.create_entity(&bob).await.unwrap();

        let link = make_link(&alice.id, &bob.id, "Bob", "mentions [[Bob]]");
        store.create_link(&link).await.unwrap();

        let backlinks = store.get_backlinks(&bob.id).await.unwrap();
        assert_eq!(backlinks.len(), 1);
        assert_eq!(backlinks[0].source_entity_id, alice.id);
        assert_eq!(backlinks[0].target_entity_id, bob.id);
    }

    #[tokio::test]
    async fn multiple_links_between_entities() {
        let store = setup_store().await;
        let alice = make_entity("Alice", EntityType::Person);
        let bob = make_entity("Bob", EntityType::Person);
        store.create_entity(&alice).await.unwrap();
        store.create_entity(&bob).await.unwrap();

        let link1 = make_link(&alice.id, &bob.id, "Bob", "context 1");
        let link2 = make_link(&alice.id, &bob.id, "Robert", "context 2");
        store.create_link(&link1).await.unwrap();
        store.create_link(&link2).await.unwrap();

        let outgoing = store.get_outgoing_links(&alice.id).await.unwrap();
        assert_eq!(outgoing.len(), 2);
    }

    #[tokio::test]
    async fn duplicate_link_ignored() {
        let store = setup_store().await;
        let alice = make_entity("Alice", EntityType::Person);
        let bob = make_entity("Bob", EntityType::Person);
        store.create_entity(&alice).await.unwrap();
        store.create_entity(&bob).await.unwrap();

        let link1 = make_link(&alice.id, &bob.id, "Bob", "context");
        store.create_link(&link1).await.unwrap();

        // Same source, target, and link_text — should be silently ignored
        let link2 = make_link(&alice.id, &bob.id, "Bob", "different context");
        store.create_link(&link2).await.unwrap();

        let outgoing = store.get_outgoing_links(&alice.id).await.unwrap();
        assert_eq!(outgoing.len(), 1);
    }

    #[tokio::test]
    async fn no_links_returns_empty() {
        let store = setup_store().await;
        let alice = make_entity("Alice", EntityType::Person);
        store.create_entity(&alice).await.unwrap();

        let outgoing = store.get_outgoing_links(&alice.id).await.unwrap();
        assert!(outgoing.is_empty());

        let backlinks = store.get_backlinks(&alice.id).await.unwrap();
        assert!(backlinks.is_empty());
    }

    #[tokio::test]
    async fn links_integration_parse_store_retrieve() {
        use crate::wikilink::extract_wikilinks;

        let store = setup_store().await;

        // Create entities with distinct names to avoid dedup fuzzy matching
        let rust_safety = make_entity("Rust Safety", EntityType::Concept);
        let ownership_model = make_entity("Ownership Model", EntityType::Concept);
        let borrowing = make_entity("Borrowing Rules", EntityType::Concept);
        store.create_entity(&rust_safety).await.unwrap();
        store.create_entity(&ownership_model).await.unwrap();
        store.create_entity(&borrowing).await.unwrap();

        // Simulate parsing a note body
        let body = "This note references [[Ownership Model]] and also [[Borrowing Rules|see borrowing]] for context.";
        let refs = extract_wikilinks(body);
        assert_eq!(refs.len(), 2);

        // Map parsed wikilinks to entity links (simulating a real lookup)
        let link1 = EntityLink {
            id: uuid::Uuid::new_v4().to_string(),
            source_entity_id: rust_safety.id.clone(),
            target_entity_id: ownership_model.id.clone(),
            link_text: refs[0].target.clone(),
            context: refs[0].context.clone(),
            created_at: now(),
        };
        let link2 = EntityLink {
            id: uuid::Uuid::new_v4().to_string(),
            source_entity_id: rust_safety.id.clone(),
            target_entity_id: borrowing.id.clone(),
            link_text: refs[1].target.clone(),
            context: refs[1].context.clone(),
            created_at: now(),
        };

        store.create_link(&link1).await.unwrap();
        store.create_link(&link2).await.unwrap();

        // Verify outgoing links from Rust Safety
        let outgoing = store.get_outgoing_links(&rust_safety.id).await.unwrap();
        assert_eq!(outgoing.len(), 2);

        // Verify backlinks to Ownership Model
        let backlinks_om = store.get_backlinks(&ownership_model.id).await.unwrap();
        assert_eq!(backlinks_om.len(), 1);
        assert_eq!(backlinks_om[0].source_entity_id, rust_safety.id);

        // Verify backlinks to Borrowing Rules
        let backlinks_br = store.get_backlinks(&borrowing.id).await.unwrap();
        assert_eq!(backlinks_br.len(), 1);
        assert_eq!(backlinks_br[0].source_entity_id, rust_safety.id);

        // Ownership Model and Borrowing Rules should have no outgoing links
        let outgoing_om = store.get_outgoing_links(&ownership_model.id).await.unwrap();
        assert!(outgoing_om.is_empty());
    }

    // --- Persistence ---

    #[tokio::test]
    async fn persistence_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("memory.db");

        {
            let store = SqliteMemoryStore::open(&db_path).unwrap();
            store.initialize().await.unwrap();
            let entity = make_entity("Alice", EntityType::Person);
            store.create_entity(&entity).await.unwrap();

            let mem = make_memory(&entity.id, "Alice uses Symbiotic");
            let ev = make_evidence(&mem.id);
            store.create_memory(&mem, &[ev]).await.unwrap();
        }

        {
            let store = SqliteMemoryStore::open(&db_path).unwrap();
            store.initialize().await.unwrap();

            let entities = store
                .find_entities_by_type(EntityType::Person, 10)
                .await
                .unwrap();
            assert_eq!(entities.len(), 1);
            assert_eq!(entities[0].name, "Alice");

            let memories = store.get_memories(&entities[0].id, None).await.unwrap();
            assert_eq!(memories.len(), 1);
            assert_eq!(memories[0].fact, "Alice uses Symbiotic");
        }
    }
}
