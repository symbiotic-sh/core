//! Schema definitions and row-mapping helpers for the SQLite memory store.

use rusqlite::Connection;

use crate::types::*;

pub(crate) const SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS entities (
    id TEXT PRIMARY KEY,
    entity_type TEXT NOT NULL,
    name TEXT NOT NULL,
    attributes TEXT NOT NULL DEFAULT '{}',
    sensitivity TEXT NOT NULL DEFAULT 'private',
    allowed_models TEXT NOT NULL DEFAULT 'local_only',
    space TEXT NOT NULL DEFAULT 'knowledge',
    status TEXT NOT NULL DEFAULT 'active',
    merged_into TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_entities_type ON entities(entity_type);
CREATE INDEX IF NOT EXISTS idx_entities_name ON entities(name);
CREATE INDEX IF NOT EXISTS idx_entities_status ON entities(status);
CREATE INDEX IF NOT EXISTS idx_entities_space ON entities(space);

CREATE VIRTUAL TABLE IF NOT EXISTS entities_fts USING fts5(
    name,
    attributes,
    content='entities',
    content_rowid='rowid'
);

-- Triggers to keep FTS in sync with entities table
CREATE TRIGGER IF NOT EXISTS entities_ai AFTER INSERT ON entities BEGIN
    INSERT INTO entities_fts(rowid, name, attributes)
    VALUES (new.rowid, new.name, new.attributes);
END;

CREATE TRIGGER IF NOT EXISTS entities_ad AFTER DELETE ON entities BEGIN
    INSERT INTO entities_fts(entities_fts, rowid, name, attributes)
    VALUES ('delete', old.rowid, old.name, old.attributes);
END;

CREATE TRIGGER IF NOT EXISTS entities_au AFTER UPDATE ON entities BEGIN
    INSERT INTO entities_fts(entities_fts, rowid, name, attributes)
    VALUES ('delete', old.rowid, old.name, old.attributes);
    INSERT INTO entities_fts(rowid, name, attributes)
    VALUES (new.rowid, new.name, new.attributes);
END;

CREATE TABLE IF NOT EXISTS relationships (
    id TEXT PRIMARY KEY,
    from_entity TEXT NOT NULL REFERENCES entities(id),
    to_entity TEXT NOT NULL REFERENCES entities(id),
    relation_type TEXT NOT NULL,
    strength REAL NOT NULL DEFAULT 0.5,
    valid_from TEXT NOT NULL,
    valid_to TEXT,
    sensitivity TEXT NOT NULL DEFAULT 'private',
    allowed_models TEXT NOT NULL DEFAULT 'local_only',
    status TEXT NOT NULL DEFAULT 'active',
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_rel_from ON relationships(from_entity);
CREATE INDEX IF NOT EXISTS idx_rel_to ON relationships(to_entity);
CREATE INDEX IF NOT EXISTS idx_rel_type ON relationships(relation_type);
CREATE INDEX IF NOT EXISTS idx_rel_status ON relationships(status);

CREATE TABLE IF NOT EXISTS memories (
    id TEXT PRIMARY KEY,
    entity_id TEXT NOT NULL REFERENCES entities(id),
    fact TEXT NOT NULL,
    confidence REAL NOT NULL,
    disposition TEXT NOT NULL,
    sensitivity TEXT NOT NULL DEFAULT 'private',
    valid_from TEXT NOT NULL,
    valid_to TEXT,
    status TEXT NOT NULL DEFAULT 'active',
    superseded_by TEXT,
    fsrs_stability REAL,
    fsrs_difficulty REAL,
    fsrs_last_access INTEGER,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_mem_entity ON memories(entity_id);
CREATE INDEX IF NOT EXISTS idx_mem_status ON memories(status);
CREATE INDEX IF NOT EXISTS idx_mem_confidence ON memories(confidence);

CREATE VIRTUAL TABLE IF NOT EXISTS memories_fts USING fts5(
    fact,
    content='memories',
    content_rowid='rowid'
);

-- Triggers to keep FTS in sync with memories table
CREATE TRIGGER IF NOT EXISTS memories_ai AFTER INSERT ON memories BEGIN
    INSERT INTO memories_fts(rowid, fact) VALUES (new.rowid, new.fact);
END;

CREATE TRIGGER IF NOT EXISTS memories_ad AFTER DELETE ON memories BEGIN
    INSERT INTO memories_fts(memories_fts, rowid, fact)
    VALUES ('delete', old.rowid, old.fact);
END;

CREATE TRIGGER IF NOT EXISTS memories_au AFTER UPDATE ON memories BEGIN
    INSERT INTO memories_fts(memories_fts, rowid, fact)
    VALUES ('delete', old.rowid, old.fact);
    INSERT INTO memories_fts(rowid, fact) VALUES (new.rowid, new.fact);
END;

CREATE TABLE IF NOT EXISTS evidence (
    id TEXT PRIMARY KEY,
    memory_id TEXT REFERENCES memories(id),
    relationship_id TEXT REFERENCES relationships(id),
    entity_id TEXT REFERENCES entities(id),
    article_id TEXT,
    source_url TEXT,
    evidence_quote TEXT,
    observed_at TEXT NOT NULL,
    created_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_evidence_memory ON evidence(memory_id);
CREATE INDEX IF NOT EXISTS idx_evidence_entity ON evidence(entity_id);
CREATE INDEX IF NOT EXISTS idx_evidence_article ON evidence(article_id);

CREATE TABLE IF NOT EXISTS links (
    id TEXT PRIMARY KEY,
    source_entity_id TEXT NOT NULL,
    target_entity_id TEXT NOT NULL,
    link_text TEXT NOT NULL,
    context TEXT,
    created_at TEXT NOT NULL,
    UNIQUE(source_entity_id, target_entity_id, link_text)
);
CREATE INDEX IF NOT EXISTS idx_links_source ON links(source_entity_id);
CREATE INDEX IF NOT EXISTS idx_links_target ON links(target_entity_id);

CREATE TABLE IF NOT EXISTS graph_edge_weights (
    source_entity_id TEXT NOT NULL,
    target_entity_id TEXT NOT NULL,
    relationship TEXT NOT NULL,
    weight REAL NOT NULL DEFAULT 1.0,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (source_entity_id, target_entity_id, relationship)
);
CREATE INDEX IF NOT EXISTS idx_graph_edge_weights_source
    ON graph_edge_weights(source_entity_id);
CREATE INDEX IF NOT EXISTS idx_graph_edge_weights_target
    ON graph_edge_weights(target_entity_id);

CREATE TABLE IF NOT EXISTS graph_node_metrics (
    entity_id TEXT PRIMARY KEY REFERENCES entities(id),
    betweenness REAL NOT NULL DEFAULT 0.0,
    updated_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_graph_node_metrics_updated_at
    ON graph_node_metrics(updated_at);

CREATE TABLE IF NOT EXISTS graph_metrics_meta (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    node_count INTEGER NOT NULL,
    relationship_count INTEGER NOT NULL,
    link_count INTEGER NOT NULL,
    topology_updated_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS recall_probe_runs (
    id TEXT PRIMARY KEY,
    started_at INTEGER NOT NULL,
    finished_at INTEGER,
    cohort TEXT,
    top_k INTEGER NOT NULL,
    subject_count INTEGER NOT NULL,
    matched_count INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_recall_probe_runs_started_at
    ON recall_probe_runs(started_at);
CREATE INDEX IF NOT EXISTS idx_recall_probe_runs_cohort
    ON recall_probe_runs(cohort, started_at);

CREATE TABLE IF NOT EXISTS recall_probe_results (
    run_id TEXT NOT NULL,
    target_kind TEXT NOT NULL,
    target_id TEXT NOT NULL,
    query TEXT NOT NULL,
    matched INTEGER NOT NULL,
    rank INTEGER,
    retrieval_mode TEXT NOT NULL,
    top_item_ids_json TEXT NOT NULL,
    remediation_flags_json TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (run_id, target_kind, target_id, query)
);
CREATE INDEX IF NOT EXISTS idx_recall_probe_results_target
    ON recall_probe_results(target_kind, target_id);
CREATE INDEX IF NOT EXISTS idx_recall_probe_results_run
    ON recall_probe_results(run_id);

CREATE TABLE IF NOT EXISTS recall_probe_summary (
    target_kind TEXT NOT NULL,
    target_id TEXT NOT NULL,
    last_checked_at INTEGER NOT NULL,
    last_run_id TEXT NOT NULL,
    success_rate REAL NOT NULL,
    consecutive_failures INTEGER NOT NULL,
    status TEXT NOT NULL,
    PRIMARY KEY (target_kind, target_id)
);
CREATE INDEX IF NOT EXISTS idx_recall_probe_summary_status
    ON recall_probe_summary(status);

CREATE TABLE IF NOT EXISTS recall_probe_baseline_targets (
    cohort TEXT NOT NULL,
    position INTEGER NOT NULL,
    target_kind TEXT NOT NULL,
    target_id TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (cohort, target_kind, target_id)
);
CREATE INDEX IF NOT EXISTS idx_recall_probe_baseline_targets_cohort
    ON recall_probe_baseline_targets(cohort, position);
"#;

pub(crate) fn ensure_schema(conn: &Connection) -> Result<(), rusqlite::Error> {
    conn.execute_batch(SCHEMA_SQL)?;
    ensure_column(conn, "memories", "fsrs_stability", "REAL")?;
    ensure_column(conn, "memories", "fsrs_difficulty", "REAL")?;
    ensure_column(conn, "memories", "fsrs_last_access", "INTEGER")?;
    ensure_column(conn, "recall_probe_runs", "cohort", "TEXT")?;
    Ok(())
}

fn ensure_column(
    conn: &Connection,
    table: &str,
    column: &str,
    column_def: &str,
) -> Result<(), rusqlite::Error> {
    let pragma = format!("PRAGMA table_info({table})");
    let mut stmt = conn.prepare(&pragma)?;
    let has_column = stmt
        .query_map([], |row| row.get::<_, String>("name"))?
        .any(|name| name.ok().is_some_and(|existing| existing == column));

    if !has_column {
        let alter = format!("ALTER TABLE {table} ADD COLUMN {column} {column_def}");
        conn.execute(&alter, [])?;
    }

    Ok(())
}

// --- Helper functions for reading rows ---

pub(crate) fn row_to_entity(row: &rusqlite::Row<'_>) -> Result<Entity, rusqlite::Error> {
    let entity_type_str: String = row.get("entity_type")?;
    let sensitivity_str: String = row.get("sensitivity")?;
    let allowed_models_str: String = row.get("allowed_models")?;
    let space_str: String = row
        .get::<_, String>("space")
        .unwrap_or_else(|_| "knowledge".to_string());
    let status_str: String = row.get("status")?;
    let attributes_str: String = row.get("attributes")?;

    Ok(Entity {
        id: row.get("id")?,
        entity_type: entity_type_str
            .parse()
            .map_err(|e: MemoryStoreError| rusqlite::Error::ToSqlConversionFailure(e.into()))?,
        name: row.get("name")?,
        attributes: serde_json::from_str(&attributes_str).unwrap_or_default(),
        sensitivity: sensitivity_str
            .parse()
            .map_err(|e: MemoryStoreError| rusqlite::Error::ToSqlConversionFailure(e.into()))?,
        allowed_models: allowed_models_str
            .parse()
            .map_err(|e: MemoryStoreError| rusqlite::Error::ToSqlConversionFailure(e.into()))?,
        space: space_str
            .parse()
            .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?,
        status: status_str
            .parse()
            .map_err(|e: MemoryStoreError| rusqlite::Error::ToSqlConversionFailure(e.into()))?,
        merged_into: row.get("merged_into")?,
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
    })
}

pub(crate) fn row_to_memory(row: &rusqlite::Row<'_>) -> Result<Memory, rusqlite::Error> {
    let disposition_str: String = row.get("disposition")?;
    let sensitivity_str: String = row.get("sensitivity")?;
    let status_str: String = row.get("status")?;

    Ok(Memory {
        id: row.get("id")?,
        entity_id: row.get("entity_id")?,
        fact: row.get("fact")?,
        confidence: row.get("confidence")?,
        disposition: disposition_str
            .parse()
            .map_err(|e: MemoryStoreError| rusqlite::Error::ToSqlConversionFailure(e.into()))?,
        sensitivity: sensitivity_str
            .parse()
            .map_err(|e: MemoryStoreError| rusqlite::Error::ToSqlConversionFailure(e.into()))?,
        valid_from: row.get("valid_from")?,
        valid_to: row.get("valid_to")?,
        status: status_str
            .parse()
            .map_err(|e: MemoryStoreError| rusqlite::Error::ToSqlConversionFailure(e.into()))?,
        superseded_by: row.get("superseded_by")?,
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
        // New Phase 2 fields — not stored in SQLite columns yet,
        // so default to empty/None for backward compatibility.
        fact_type: None,
        authored_by: None,
        supersedes: None,
        depends_on: Vec::new(),
        fsrs: match (
            row.get::<_, Option<f64>>("fsrs_stability")?,
            row.get::<_, Option<f64>>("fsrs_difficulty")?,
            row.get::<_, Option<u64>>("fsrs_last_access")?,
        ) {
            (Some(stability), Some(difficulty), Some(last_access)) => Some(FsrsState {
                stability,
                difficulty,
                last_access,
            }),
            _ => None,
        },
    })
}

pub(crate) fn row_to_relationship(
    row: &rusqlite::Row<'_>,
) -> Result<Relationship, rusqlite::Error> {
    let sensitivity_str: String = row.get("sensitivity")?;
    let allowed_models_str: String = row.get("allowed_models")?;
    let status_str: String = row.get("status")?;

    Ok(Relationship {
        id: row.get("id")?,
        from_entity: row.get("from_entity")?,
        to_entity: row.get("to_entity")?,
        relation_type: row.get("relation_type")?,
        strength: row.get("strength")?,
        valid_from: row.get("valid_from")?,
        valid_to: row.get("valid_to")?,
        sensitivity: sensitivity_str
            .parse()
            .map_err(|e: MemoryStoreError| rusqlite::Error::ToSqlConversionFailure(e.into()))?,
        allowed_models: allowed_models_str
            .parse()
            .map_err(|e: MemoryStoreError| rusqlite::Error::ToSqlConversionFailure(e.into()))?,
        status: status_str
            .parse()
            .map_err(|e: MemoryStoreError| rusqlite::Error::ToSqlConversionFailure(e.into()))?,
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
    })
}

pub(crate) fn row_to_evidence(row: &rusqlite::Row<'_>) -> Result<Evidence, rusqlite::Error> {
    Ok(Evidence {
        id: row.get("id")?,
        memory_id: row.get("memory_id")?,
        relationship_id: row.get("relationship_id")?,
        entity_id: row.get("entity_id")?,
        article_id: row.get("article_id")?,
        source_url: row.get("source_url")?,
        evidence_quote: row.get("evidence_quote")?,
        observed_at: row.get("observed_at")?,
        created_at: row.get("created_at")?,
    })
}

pub(crate) fn row_to_entity_link(row: &rusqlite::Row<'_>) -> Result<EntityLink, rusqlite::Error> {
    Ok(EntityLink {
        id: row.get("id")?,
        source_entity_id: row.get("source_entity_id")?,
        target_entity_id: row.get("target_entity_id")?,
        link_text: row.get("link_text")?,
        context: row.get::<_, Option<String>>("context")?.unwrap_or_default(),
        created_at: row.get("created_at")?,
    })
}
