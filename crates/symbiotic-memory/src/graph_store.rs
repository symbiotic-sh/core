//! SQLite-backed adapter from the memory schema to `symbiotic-context` graph retrieval.
//!
//! This keeps retrieval honest: the Recall Gateway can traverse the live
//! Neural Graph stored in `memory.db` without coupling `symbiotic-context`
//! to storage details.

use std::path::Path;
use std::sync::Mutex;

use chrono::DateTime;
use rusqlite::{Connection, OptionalExtension};
use symbiotic_context::graph::{
    betweenness_centrality, EntityType as GraphEntityType, FsrsParams, GraphEdge, GraphEntity,
    GraphRetrievalError, GraphStore, Memory as GraphMemory,
};
use symbiotic_context::Sensitivity as GraphSensitivity;
use symbiotic_core::now_unix;

use crate::sqlite_schema::{
    ensure_schema, row_to_entity, row_to_entity_link, row_to_evidence, row_to_memory,
    row_to_relationship,
};
use crate::types::{Entity, EntityLink, MemoryStatus, Relationship, Sensitivity};

const EDGE_WEIGHT_DECAY_PER_DAY: f64 = 0.99;
const EDGE_WEIGHT_REINFORCE_DELTA: f64 = 0.15;
const EDGE_WEIGHT_MAX: f64 = 3.0;
const STRUCTURAL_BOOST_MAX: f64 = 1.2;
const GRAPH_METRICS_MAX_AGE_SECS: u64 = 300;

/// SQLite-backed graph adapter over the live memory schema.
pub struct SqliteGraphStore {
    conn: Mutex<Connection>,
}

impl SqliteGraphStore {
    /// Open or create a graph store at the given SQLite path.
    pub fn open(path: &Path) -> Result<Self, GraphRetrievalError> {
        let conn =
            Connection::open(path).map_err(|e| GraphRetrievalError::StoreError(e.to_string()))?;
        ensure_schema(&conn).map_err(|e| GraphRetrievalError::StoreError(e.to_string()))?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Create an in-memory graph store for tests.
    pub fn open_in_memory() -> Result<Self, GraphRetrievalError> {
        let conn = Connection::open_in_memory()
            .map_err(|e| GraphRetrievalError::StoreError(e.to_string()))?;
        ensure_schema(&conn).map_err(|e| GraphRetrievalError::StoreError(e.to_string()))?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn with_conn<T>(
        &self,
        f: impl FnOnce(&Connection) -> Result<T, GraphRetrievalError>,
    ) -> Result<T, GraphRetrievalError> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| GraphRetrievalError::StoreError(e.to_string()))?;
        f(&conn)
    }
}

impl GraphStore for SqliteGraphStore {
    fn find_seed_entities(&self, query: &str) -> Result<Vec<GraphEntity>, GraphRetrievalError> {
        self.with_conn(|conn| {
            let normalized = query.trim();
            let entities = if normalized.is_empty() {
                let mut stmt = conn
                    .prepare("SELECT * FROM entities WHERE status = 'active' LIMIT 20")
                    .map_err(|e| GraphRetrievalError::StoreError(e.to_string()))?;
                let rows = stmt
                    .query_map([], row_to_entity)
                    .map_err(|e| GraphRetrievalError::StoreError(e.to_string()))?
                    .filter_map(|row| row.ok())
                    .collect::<Vec<_>>();
                rows
            } else {
                let fts_query = format!("\"{}\"", normalized.replace('"', "\"\""));
                let mut stmt = conn
                    .prepare(
                        "SELECT e.* FROM entities e
                         JOIN entities_fts fts ON e.rowid = fts.rowid
                         WHERE entities_fts MATCH ?1
                         AND e.status = 'active'
                         LIMIT 20",
                    )
                    .map_err(|e| GraphRetrievalError::StoreError(e.to_string()))?;
                let rows = stmt
                    .query_map(rusqlite::params![fts_query], row_to_entity)
                    .map_err(|e| GraphRetrievalError::StoreError(e.to_string()))?
                    .filter_map(|row| row.ok())
                    .collect::<Vec<_>>();
                rows
            };

            entities
                .into_iter()
                .map(|entity| load_graph_entity(conn, entity))
                .collect()
        })
    }

    fn get_edges(&self, entity_id: &str) -> Result<Vec<GraphEdge>, GraphRetrievalError> {
        self.with_conn(|conn| {
            let mut edges = Vec::new();

            let mut rel_stmt = conn
                .prepare(
                    "SELECT * FROM relationships
                     WHERE (from_entity = ?1 OR to_entity = ?1)
                     AND status = 'active'",
                )
                .map_err(|e| GraphRetrievalError::StoreError(e.to_string()))?;
            let relationships: Vec<Relationship> = rel_stmt
                .query_map([entity_id], row_to_relationship)
                .map_err(|e| GraphRetrievalError::StoreError(e.to_string()))?
                .filter_map(|row| row.ok())
                .collect();
            edges.extend(relationships.into_iter().map(|rel| {
                let relationship = rel.relation_type;
                let weight =
                    load_edge_weight(conn, &rel.from_entity, &rel.to_entity, &relationship)
                        .unwrap_or(1.0);
                GraphEdge {
                    source_id: rel.from_entity,
                    target_id: rel.to_entity,
                    relationship,
                    strength: rel.strength.clamp(0.0, 1.0),
                    weight,
                }
            }));

            let mut link_stmt = conn
                .prepare(
                    "SELECT * FROM links
                     WHERE source_entity_id = ?1 OR target_entity_id = ?1",
                )
                .map_err(|e| GraphRetrievalError::StoreError(e.to_string()))?;
            let links: Vec<EntityLink> = link_stmt
                .query_map([entity_id], row_to_entity_link)
                .map_err(|e| GraphRetrievalError::StoreError(e.to_string()))?
                .filter_map(|row| row.ok())
                .collect();
            edges.extend(links.into_iter().map(|link| {
                let relationship = if link.link_text.trim().is_empty() {
                    "wikilink".to_string()
                } else {
                    format!("wikilink:{}", link.link_text)
                };
                let weight = load_edge_weight(
                    conn,
                    &link.source_entity_id,
                    &link.target_entity_id,
                    &relationship,
                )
                .unwrap_or(1.0);
                GraphEdge {
                    source_id: link.source_entity_id,
                    target_id: link.target_entity_id,
                    relationship,
                    strength: 0.6,
                    weight,
                }
            }));

            Ok(edges)
        })
    }

    fn get_entity(&self, entity_id: &str) -> Result<Option<GraphEntity>, GraphRetrievalError> {
        self.with_conn(|conn| {
            let entity = match conn.query_row(
                "SELECT * FROM entities WHERE id = ?1 AND status = 'active'",
                [entity_id],
                row_to_entity,
            ) {
                Ok(entity) => entity,
                Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(None),
                Err(e) => return Err(GraphRetrievalError::StoreError(e.to_string())),
            };

            load_graph_entity(conn, entity).map(Some)
        })
    }

    fn update_memory_fsrs(
        &self,
        memory_id: &str,
        fsrs: &FsrsParams,
    ) -> Result<(), GraphRetrievalError> {
        self.with_conn(|conn| {
            conn.execute(
                "UPDATE memories
                 SET fsrs_stability = ?1, fsrs_difficulty = ?2, fsrs_last_access = ?3
                 WHERE id = ?4",
                rusqlite::params![fsrs.stability, fsrs.difficulty, fsrs.last_access, memory_id,],
            )
            .map_err(|e| GraphRetrievalError::StoreError(e.to_string()))?;
            Ok(())
        })
    }

    fn reinforce_edges(&self, edges: &[GraphEdge]) -> Result<(), GraphRetrievalError> {
        self.with_conn(|conn| {
            let now = now_unix();
            for edge in edges {
                let current =
                    load_edge_weight(conn, &edge.source_id, &edge.target_id, &edge.relationship)?;
                let next = (current + EDGE_WEIGHT_REINFORCE_DELTA).min(EDGE_WEIGHT_MAX);
                conn.execute(
                    "INSERT INTO graph_edge_weights (source_entity_id, target_entity_id, relationship, weight, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5)
                     ON CONFLICT(source_entity_id, target_entity_id, relationship)
                     DO UPDATE SET weight = excluded.weight, updated_at = excluded.updated_at",
                    rusqlite::params![
                        edge.source_id,
                        edge.target_id,
                        edge.relationship,
                        next,
                        now
                    ],
                )
                .map_err(|e| GraphRetrievalError::StoreError(e.to_string()))?;
            }
            Ok(())
        })
    }

    fn structural_boost(&self, entity_id: &str) -> Result<f64, GraphRetrievalError> {
        self.with_conn(|conn| load_structural_boost(conn, entity_id))
    }
}

fn load_edge_weight(
    conn: &Connection,
    source_id: &str,
    target_id: &str,
    relationship: &str,
) -> Result<f64, GraphRetrievalError> {
    match conn.query_row(
        "SELECT weight, updated_at
         FROM graph_edge_weights
         WHERE source_entity_id = ?1 AND target_entity_id = ?2 AND relationship = ?3",
        rusqlite::params![source_id, target_id, relationship],
        |row| Ok((row.get::<_, f64>(0)?, row.get::<_, u64>(1)?)),
    ) {
        Ok((weight, updated_at)) => Ok(decay_edge_weight(weight, updated_at, now_unix())),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(1.0),
        Err(e) => Err(GraphRetrievalError::StoreError(e.to_string())),
    }
}

fn decay_edge_weight(weight: f64, updated_at: u64, now: u64) -> f64 {
    let clamped = weight.clamp(0.0, EDGE_WEIGHT_MAX);
    if updated_at == 0 || now <= updated_at {
        return clamped;
    }

    let age_days = now.saturating_sub(updated_at) as f64 / 86_400.0;
    let decay = EDGE_WEIGHT_DECAY_PER_DAY.powf(age_days);
    (1.0 + (clamped - 1.0) * decay).clamp(0.0, EDGE_WEIGHT_MAX)
}

fn load_structural_boost(conn: &Connection, entity_id: &str) -> Result<f64, GraphRetrievalError> {
    ensure_graph_metrics_current(conn)?;

    match conn.query_row(
        "SELECT betweenness FROM graph_node_metrics WHERE entity_id = ?1",
        [entity_id],
        |row| row.get::<_, f64>(0),
    ) {
        Ok(centrality) => Ok(centrality_to_boost(centrality)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(1.0),
        Err(e) => Err(GraphRetrievalError::StoreError(e.to_string())),
    }
}

fn centrality_to_boost(centrality: f64) -> f64 {
    let normalized = centrality.clamp(0.0, 1.0);
    1.0 + (STRUCTURAL_BOOST_MAX - 1.0) * normalized
}

fn ensure_graph_metrics_current(conn: &Connection) -> Result<(), GraphRetrievalError> {
    let metrics = current_metrics_state(conn)?;
    let topology = current_topology_state(conn)?;
    let now = now_unix();

    let missing = metrics.node_count == 0 && topology.node_count > 0;
    let stale = metrics
        .updated_at
        .is_some_and(|updated_at| now.saturating_sub(updated_at) > GRAPH_METRICS_MAX_AGE_SECS);
    let topology_changed = metrics.node_count != topology.node_count
        || metrics.relationship_count != topology.relationship_count
        || metrics.link_count != topology.link_count
        || metrics.topology_updated_at < topology.updated_at;

    if missing || stale || topology_changed {
        recompute_graph_metrics(conn, now)?;
    }

    Ok(())
}

#[derive(Debug, Default)]
struct MetricsState {
    node_count: usize,
    relationship_count: usize,
    link_count: usize,
    updated_at: Option<u64>,
    topology_updated_at: u64,
}

#[derive(Debug, Default)]
struct TopologyState {
    node_count: usize,
    relationship_count: usize,
    link_count: usize,
    updated_at: u64,
}

fn current_metrics_state(conn: &Connection) -> Result<MetricsState, GraphRetrievalError> {
    let node_count = conn
        .query_row("SELECT COUNT(*) FROM graph_node_metrics", [], |row| {
            row.get::<_, usize>(0)
        })
        .map_err(|e| GraphRetrievalError::StoreError(e.to_string()))?;
    let meta = conn
        .query_row(
            "SELECT relationship_count, link_count, topology_updated_at, updated_at
             FROM graph_metrics_meta
             WHERE singleton = 1",
            [],
            |row| {
                Ok((
                    row.get::<_, usize>(0)?,
                    row.get::<_, usize>(1)?,
                    row.get::<_, u64>(2)?,
                    row.get::<_, u64>(3)?,
                ))
            },
        )
        .optional()
        .map_err(|e| GraphRetrievalError::StoreError(e.to_string()))?;

    Ok(match meta {
        Some((relationship_count, link_count, topology_updated_at, updated_at)) => MetricsState {
            node_count,
            relationship_count,
            link_count,
            updated_at: Some(updated_at),
            topology_updated_at,
        },
        None => MetricsState {
            node_count,
            ..MetricsState::default()
        },
    })
}

fn current_topology_state(conn: &Connection) -> Result<TopologyState, GraphRetrievalError> {
    let node_count = conn
        .query_row(
            "SELECT COUNT(*) FROM entities WHERE status = 'active'",
            [],
            |row| row.get::<_, usize>(0),
        )
        .map_err(|e| GraphRetrievalError::StoreError(e.to_string()))?;
    let relationship_count = conn
        .query_row(
            "SELECT COUNT(*) FROM relationships WHERE status = 'active'",
            [],
            |row| row.get::<_, usize>(0),
        )
        .map_err(|e| GraphRetrievalError::StoreError(e.to_string()))?;
    let link_count = conn
        .query_row("SELECT COUNT(*) FROM links", [], |row| {
            row.get::<_, usize>(0)
        })
        .map_err(|e| GraphRetrievalError::StoreError(e.to_string()))?;

    let relationships_updated = conn
        .query_row(
            "SELECT MAX(updated_at) FROM relationships WHERE status = 'active'",
            [],
            |row| row.get::<_, Option<String>>(0),
        )
        .map_err(|e| GraphRetrievalError::StoreError(e.to_string()))?
        .as_deref()
        .and_then(parse_rfc3339_ts)
        .unwrap_or(0);
    let links_updated = conn
        .query_row("SELECT MAX(created_at) FROM links", [], |row| {
            row.get::<_, Option<String>>(0)
        })
        .map_err(|e| GraphRetrievalError::StoreError(e.to_string()))?
        .as_deref()
        .and_then(parse_rfc3339_ts)
        .unwrap_or(0);
    let entities_updated = conn
        .query_row(
            "SELECT MAX(updated_at) FROM entities WHERE status = 'active'",
            [],
            |row| row.get::<_, Option<String>>(0),
        )
        .map_err(|e| GraphRetrievalError::StoreError(e.to_string()))?
        .as_deref()
        .and_then(parse_rfc3339_ts)
        .unwrap_or(0);

    Ok(TopologyState {
        node_count,
        relationship_count,
        link_count,
        updated_at: relationships_updated
            .max(links_updated)
            .max(entities_updated),
    })
}

fn recompute_graph_metrics(conn: &Connection, now: u64) -> Result<(), GraphRetrievalError> {
    let topology = current_topology_state(conn)?;
    let mut entity_stmt = conn
        .prepare("SELECT id FROM entities WHERE status = 'active'")
        .map_err(|e| GraphRetrievalError::StoreError(e.to_string()))?;
    let entity_ids = entity_stmt
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|e| GraphRetrievalError::StoreError(e.to_string()))?
        .filter_map(|row| row.ok())
        .collect::<Vec<_>>();

    let mut edge_stmt = conn
        .prepare(
            "SELECT from_entity, to_entity, relation_type, strength
             FROM relationships
             WHERE status = 'active'",
        )
        .map_err(|e| GraphRetrievalError::StoreError(e.to_string()))?;
    let mut edges = edge_stmt
        .query_map([], |row| {
            Ok(GraphEdge {
                source_id: row.get::<_, String>(0)?,
                target_id: row.get::<_, String>(1)?,
                relationship: row.get::<_, String>(2)?,
                strength: row.get::<_, f64>(3)?,
                weight: 1.0,
            })
        })
        .map_err(|e| GraphRetrievalError::StoreError(e.to_string()))?
        .filter_map(|row| row.ok())
        .collect::<Vec<_>>();

    let mut link_stmt = conn
        .prepare("SELECT source_entity_id, target_entity_id, link_text FROM links")
        .map_err(|e| GraphRetrievalError::StoreError(e.to_string()))?;
    let link_edges = link_stmt
        .query_map([], |row| {
            let link_text = row.get::<_, String>(2)?;
            Ok(GraphEdge {
                source_id: row.get::<_, String>(0)?,
                target_id: row.get::<_, String>(1)?,
                relationship: if link_text.trim().is_empty() {
                    "wikilink".to_string()
                } else {
                    format!("wikilink:{link_text}")
                },
                strength: 0.6,
                weight: 1.0,
            })
        })
        .map_err(|e| GraphRetrievalError::StoreError(e.to_string()))?
        .filter_map(|row| row.ok())
        .collect::<Vec<_>>();
    edges.extend(link_edges);

    let raw = betweenness_centrality(&entity_ids, &edges);
    let max_raw = raw.values().copied().fold(0.0_f64, f64::max);

    conn.execute("DELETE FROM graph_node_metrics", [])
        .map_err(|e| GraphRetrievalError::StoreError(e.to_string()))?;

    for entity_id in entity_ids {
        let normalized = if max_raw > 0.0 {
            raw.get(&entity_id).copied().unwrap_or(0.0) / max_raw
        } else {
            0.0
        };
        conn.execute(
            "INSERT INTO graph_node_metrics (entity_id, betweenness, updated_at)
             VALUES (?1, ?2, ?3)",
            rusqlite::params![entity_id, normalized, now],
        )
        .map_err(|e| GraphRetrievalError::StoreError(e.to_string()))?;
    }

    conn.execute(
        "INSERT INTO graph_metrics_meta (
             singleton, node_count, relationship_count, link_count, topology_updated_at, updated_at
         ) VALUES (1, ?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(singleton) DO UPDATE SET
             node_count = excluded.node_count,
             relationship_count = excluded.relationship_count,
             link_count = excluded.link_count,
             topology_updated_at = excluded.topology_updated_at,
             updated_at = excluded.updated_at",
        rusqlite::params![
            topology.node_count,
            topology.relationship_count,
            topology.link_count,
            topology.updated_at,
            now
        ],
    )
    .map_err(|e| GraphRetrievalError::StoreError(e.to_string()))?;

    Ok(())
}

fn load_graph_entity(
    conn: &Connection,
    entity: Entity,
) -> Result<GraphEntity, GraphRetrievalError> {
    let mut stmt = conn
        .prepare("SELECT * FROM memories WHERE entity_id = ?1")
        .map_err(|e| GraphRetrievalError::StoreError(e.to_string()))?;
    let memories = stmt
        .query_map([entity.id.as_str()], row_to_memory)
        .map_err(|e| GraphRetrievalError::StoreError(e.to_string()))?
        .filter_map(|row| row.ok())
        .collect::<Vec<_>>();

    let memories = memories
        .into_iter()
        .map(|memory| {
            let mut ev_stmt = conn
                .prepare("SELECT * FROM evidence WHERE memory_id = ?1")
                .map_err(|e| GraphRetrievalError::StoreError(e.to_string()))?;
            let evidence = ev_stmt
                .query_map([memory.id.as_str()], row_to_evidence)
                .map_err(|e| GraphRetrievalError::StoreError(e.to_string()))?
                .filter_map(|row| row.ok())
                .flat_map(|e| {
                    [e.article_id, e.source_url]
                        .into_iter()
                        .flatten()
                        .collect::<Vec<_>>()
                })
                .collect::<Vec<_>>();

            Ok(GraphMemory {
                id: memory.id,
                content: memory.fact,
                sensitivity: map_sensitivity(memory.sensitivity),
                evidence,
                updated_at: parse_rfc3339_ts(&memory.updated_at),
                archived: memory.status == MemoryStatus::Archived,
                fsrs: memory.fsrs.as_ref().map(|fsrs| FsrsParams {
                    stability: fsrs.stability,
                    difficulty: fsrs.difficulty,
                    last_access: fsrs.last_access,
                }),
            })
        })
        .collect::<Result<Vec<_>, GraphRetrievalError>>()?;

    Ok(GraphEntity {
        id: entity.id,
        name: entity.name,
        entity_type: map_entity_type(entity.entity_type),
        sensitivity: map_sensitivity(entity.sensitivity),
        memories,
    })
}

fn map_sensitivity(value: Sensitivity) -> GraphSensitivity {
    match value {
        Sensitivity::Shareable => GraphSensitivity::Shareable,
        Sensitivity::Restricted => GraphSensitivity::Restricted,
        Sensitivity::Private => GraphSensitivity::Private,
    }
}

fn map_entity_type(value: crate::types::EntityType) -> GraphEntityType {
    match value {
        crate::types::EntityType::Person => GraphEntityType::Person,
        crate::types::EntityType::Project => GraphEntityType::Project,
        crate::types::EntityType::Org => GraphEntityType::Organization,
        crate::types::EntityType::Tool => GraphEntityType::Tool,
        crate::types::EntityType::Preference => GraphEntityType::Concept,
        crate::types::EntityType::Concept => GraphEntityType::Concept,
        crate::types::EntityType::Task => GraphEntityType::Task,
    }
}

fn parse_rfc3339_ts(value: &str) -> Option<u64> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .and_then(|dt| u64::try_from(dt.timestamp()).ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::MemoryStore;
    use crate::types::{
        AllowedModels, Entity, EntityLink, EntityStatus, Evidence, FactDisposition, Memory,
        MemorySpace, MemoryStatus, Relationship, RelationshipStatus, Sensitivity,
    };
    use chrono::Utc;
    use symbiotic_context::graph::{
        BfsGraphRetriever, GraphRetrievalConfig, GraphRetriever, GraphStore,
    };

    fn now() -> String {
        Utc::now().to_rfc3339()
    }

    fn make_entity(id: &str, name: &str, entity_type: crate::types::EntityType) -> Entity {
        let ts = now();
        Entity {
            id: id.to_string(),
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

    fn make_memory(id: &str, entity_id: &str, fact: &str) -> Memory {
        let ts = now();
        Memory {
            id: id.to_string(),
            entity_id: entity_id.to_string(),
            fact: fact.to_string(),
            confidence: 0.9,
            disposition: FactDisposition::UserConfirmed,
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

    fn make_evidence(memory_id: &str, article_id: &str) -> Evidence {
        let ts = now();
        Evidence {
            id: format!("ev-{memory_id}"),
            memory_id: Some(memory_id.to_string()),
            relationship_id: None,
            entity_id: None,
            article_id: Some(article_id.to_string()),
            source_url: Some(format!("https://example.com/{article_id}")),
            evidence_quote: None,
            observed_at: ts.clone(),
            created_at: ts,
        }
    }

    fn make_relationship(from: &str, to: &str) -> Relationship {
        let ts = now();
        Relationship {
            id: format!("rel-{from}-{to}"),
            from_entity: from.to_string(),
            to_entity: to.to_string(),
            relation_type: "supports".to_string(),
            strength: 0.9,
            valid_from: ts.clone(),
            valid_to: None,
            sensitivity: Sensitivity::Private,
            allowed_models: AllowedModels::LocalOnly,
            status: RelationshipStatus::Active,
            created_at: ts.clone(),
            updated_at: ts,
        }
    }

    #[tokio::test]
    async fn sqlite_graph_store_powers_bfs_retrieval_from_live_memory_db() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("memory.db");
        let store = crate::SqliteMemoryStore::open(&db_path).expect("open memory store");
        store.initialize().await.expect("initialize memory store");

        let rust = make_entity("rust", "Rust safety", crate::types::EntityType::Concept);
        let ownership = make_entity(
            "ownership",
            "Ownership model",
            crate::types::EntityType::Concept,
        );
        store.create_entity(&rust).await.expect("create entity");
        store
            .create_entity(&ownership)
            .await
            .expect("create related entity");

        let memory = make_memory("m-rust", &rust.id, "Rust uses ownership for safety.");
        store
            .create_memory(&memory, &[make_evidence(&memory.id, "article-1")])
            .await
            .expect("create memory");
        store
            .create_relationship(&make_relationship(&rust.id, &ownership.id))
            .await
            .expect("create relationship");
        store
            .create_link(&EntityLink {
                id: "link-rust-ownership".to_string(),
                source_entity_id: rust.id.clone(),
                target_entity_id: ownership.id.clone(),
                link_text: "ownership".to_string(),
                context: "Rust safety references ownership".to_string(),
                created_at: now(),
            })
            .await
            .expect("create link");

        let graph_store = SqliteGraphStore::open(&db_path).expect("open graph store");
        let retriever = BfsGraphRetriever::new(graph_store);
        let result = retriever
            .retrieve(
                "Rust safety",
                &GraphRetrievalConfig::default(),
                GraphSensitivity::Private,
            )
            .expect("retrieve graph context");

        assert_eq!(result.seeds.len(), 1, "seed should come from SQLite FTS");
        assert_eq!(result.seeds[0].entity_id, rust.id);
        assert!(
            result.seeds[0]
                .memories
                .iter()
                .any(|memory| memory.evidence.iter().any(|ev| ev == "article-1")),
            "graph memory should carry live evidence from the memory DB"
        );
        assert!(
            result
                .related
                .iter()
                .any(|node| node.entity_id == ownership.id),
            "related entity should be discovered from live relationships/links"
        );

        let persisted_memories = store
            .get_memories(&rust.id, None)
            .await
            .expect("reload persisted memories");
        let persisted_fsrs = persisted_memories[0]
            .fsrs
            .as_ref()
            .expect("retrieval should persist fsrs state");
        assert!(
            persisted_fsrs.stability > 30.0,
            "successful recall should increase stability"
        );
        assert!(
            persisted_fsrs.difficulty < 0.1,
            "successful recall should slightly reduce difficulty"
        );
        assert!(
            persisted_fsrs.last_access > 0,
            "last_access should be updated"
        );
    }

    #[tokio::test]
    async fn sqlite_graph_store_persists_edge_reinforcement_between_retrievals() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("memory.db");
        let store = crate::SqliteMemoryStore::open(&db_path).expect("open memory store");
        store.initialize().await.expect("initialize memory store");

        let seed = make_entity("seed", "Seed topic", crate::types::EntityType::Concept);
        let hub = make_entity("hub", "Hub topic", crate::types::EntityType::Concept);
        let leaf = make_entity("leaf", "Leaf topic", crate::types::EntityType::Concept);
        store.create_entity(&seed).await.expect("create seed");
        store.create_entity(&hub).await.expect("create hub");
        store.create_entity(&leaf).await.expect("create leaf");

        let seed_memory = make_memory("m-seed", &seed.id, "Seed topic anchors this query.");
        let hub_memory = make_memory("m-hub", &hub.id, "Hub topic bridges the graph.");
        let leaf_memory = make_memory("m-leaf", &leaf.id, "Leaf topic hangs off the hub.");
        store
            .create_memory(
                &seed_memory,
                &[make_evidence(&seed_memory.id, "article-seed")],
            )
            .await
            .expect("create seed memory");
        store
            .create_memory(&hub_memory, &[make_evidence(&hub_memory.id, "article-hub")])
            .await
            .expect("create hub memory");
        store
            .create_memory(
                &leaf_memory,
                &[make_evidence(&leaf_memory.id, "article-leaf")],
            )
            .await
            .expect("create leaf memory");

        store
            .create_relationship(&make_relationship(&seed.id, &hub.id))
            .await
            .expect("create seed->hub relationship");
        store
            .create_relationship(&make_relationship(&hub.id, &leaf.id))
            .await
            .expect("create hub->leaf relationship");

        let graph_store = SqliteGraphStore::open(&db_path).expect("open graph store");
        let retriever = BfsGraphRetriever::new(graph_store);
        let config = GraphRetrievalConfig {
            max_depth: 2,
            decay_factor: 0.7,
            min_score: 0.0,
            half_life_days: 0.0,
            ..Default::default()
        };

        let first = retriever
            .retrieve("Seed topic", &config, GraphSensitivity::Private)
            .expect("first retrieve");
        let first_hub = first
            .related
            .iter()
            .find(|node| node.entity_id == hub.id)
            .expect("hub in first retrieval")
            .score;
        let first_leaf = first
            .related
            .iter()
            .find(|node| node.entity_id == leaf.id)
            .expect("leaf in first retrieval")
            .score;

        let second = retriever
            .retrieve("Seed topic", &config, GraphSensitivity::Private)
            .expect("second retrieve");
        let second_hub = second
            .related
            .iter()
            .find(|node| node.entity_id == hub.id)
            .expect("hub in second retrieval")
            .score;
        let second_leaf = second
            .related
            .iter()
            .find(|node| node.entity_id == leaf.id)
            .expect("leaf in second retrieval")
            .score;

        assert!(
            second_hub > first_hub,
            "directly reinforced edge should lift the hub score"
        );
        assert!(
            second_leaf > first_leaf,
            "reinforced upstream path should lift downstream leaf score"
        );

        let inspector = SqliteGraphStore::open(&db_path).expect("reopen graph store");
        let seed_edges = inspector.get_edges(&seed.id).expect("load seed edges");
        let hub_edges = inspector.get_edges(&hub.id).expect("load hub edges");
        let seed_hub = seed_edges
            .iter()
            .find(|edge| edge.source_id == seed.id && edge.target_id == hub.id)
            .expect("seed->hub edge");
        let hub_leaf = hub_edges
            .iter()
            .find(|edge| edge.source_id == hub.id && edge.target_id == leaf.id)
            .expect("hub->leaf edge");

        assert!(seed_hub.weight > 1.0, "seed->hub edge should be reinforced");
        assert!(hub_leaf.weight > 1.0, "hub->leaf edge should be reinforced");
    }

    #[tokio::test]
    async fn sqlite_graph_store_structural_boost_favors_bridge_nodes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("memory.db");
        let store = crate::SqliteMemoryStore::open(&db_path).expect("open memory store");
        store.initialize().await.expect("initialize memory store");

        let a = make_entity("a", "Alpha", crate::types::EntityType::Concept);
        let b = make_entity("b", "Bridge", crate::types::EntityType::Concept);
        let c = make_entity("c", "Charlie", crate::types::EntityType::Concept);
        let d = make_entity("d", "Delta", crate::types::EntityType::Concept);
        store.create_entity(&a).await.expect("create a");
        store.create_entity(&b).await.expect("create b");
        store.create_entity(&c).await.expect("create c");
        store.create_entity(&d).await.expect("create d");

        store
            .create_relationship(&make_relationship(&a.id, &b.id))
            .await
            .expect("create a-b");
        store
            .create_relationship(&make_relationship(&b.id, &c.id))
            .await
            .expect("create b-c");
        store
            .create_relationship(&make_relationship(&b.id, &d.id))
            .await
            .expect("create b-d");

        let graph_store = SqliteGraphStore::open(&db_path).expect("open graph store");
        let bridge_boost = graph_store
            .structural_boost(&b.id)
            .expect("bridge structural boost");
        let leaf_boost = graph_store
            .structural_boost(&a.id)
            .expect("leaf structural boost");

        assert!(
            bridge_boost > leaf_boost,
            "bridge node should get higher boost"
        );
        assert!(
            bridge_boost > 1.0,
            "bridge should get a positive structural boost"
        );
        assert_eq!(leaf_boost, 1.0, "leaf nodes should remain neutral");
    }

    #[tokio::test]
    async fn sqlite_graph_store_refreshes_metrics_when_topology_changes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("memory.db");
        let store = crate::SqliteMemoryStore::open(&db_path).expect("open memory store");
        store.initialize().await.expect("initialize memory store");

        let a = make_entity("a", "Alpha", crate::types::EntityType::Concept);
        let b = make_entity("b", "Beta", crate::types::EntityType::Concept);
        let c = make_entity("c", "Charlie", crate::types::EntityType::Concept);
        store.create_entity(&a).await.expect("create a");
        store.create_entity(&b).await.expect("create b");
        store.create_entity(&c).await.expect("create c");

        store
            .create_relationship(&make_relationship(&a.id, &b.id))
            .await
            .expect("create a-b");

        let graph_store = SqliteGraphStore::open(&db_path).expect("open graph store");
        let before = graph_store
            .structural_boost(&b.id)
            .expect("initial structural boost");
        assert_eq!(before, 1.0, "two-node graph should have neutral centrality");

        store
            .create_relationship(&make_relationship(&b.id, &c.id))
            .await
            .expect("create b-c");

        let after = graph_store
            .structural_boost(&b.id)
            .expect("refreshed structural boost");
        assert!(
            after > before,
            "adding a new branch should refresh metrics and raise the bridge boost"
        );
    }
}
