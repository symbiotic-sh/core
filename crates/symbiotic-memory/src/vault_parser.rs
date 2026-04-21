//! Entity file format parser for the Vault-as-Truth architecture.
//!
//! Parses YAML frontmatter + `## Facts` / `## Relationships` / `## History`
//! sections from Markdown entity files in `knowledge-base/`.
//!
//! See `docs/design/vault-as-truth.md` for the full format specification.

use hex::encode as hex_encode;
use regex::Regex;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::sync::LazyLock;

use crate::{
    AllowedModels, Entity, EntityStatus, EntityType, FactDisposition, FactType, Memory,
    MemorySpace, MemoryStatus, Relationship, RelationshipStatus, Sensitivity,
};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Result of parsing a single entity Markdown file.
#[derive(Debug, Clone)]
pub struct ParsedEntityFile {
    /// The entity record parsed from frontmatter + heading.
    pub entity: Entity,
    /// Active facts from `## Facts` + archived facts from
    /// `## History` → `### Archived Facts`.
    pub memories: Vec<Memory>,
    /// Directed edges from `## Relationships`.
    pub relationships: Vec<Relationship>,
}

/// Errors that can occur during vault file parsing.
#[derive(Debug, thiserror::Error)]
pub enum VaultParseError {
    #[error("missing YAML frontmatter (expected --- delimiters)")]
    MissingFrontmatter,
    #[error("invalid YAML frontmatter: {0}")]
    InvalidYaml(String),
    #[error("missing required frontmatter field: {0}")]
    MissingField(String),
    #[error("invalid entity type: {0}")]
    InvalidEntityType(String),
    #[error("invalid memory space: {0}")]
    InvalidSpace(String),
    #[error("invalid sensitivity: {0}")]
    InvalidSensitivity(String),
}

// ---------------------------------------------------------------------------
// Internal frontmatter representation
// ---------------------------------------------------------------------------

/// YAML frontmatter as it appears in entity Markdown files.
#[derive(Debug, Deserialize)]
struct EntityFrontmatter {
    id: String,
    #[serde(rename = "type")]
    entity_type: String,
    #[serde(default)]
    aliases: Vec<String>,
    #[serde(default)]
    schema: Option<String>,
    space: String,
    #[serde(default = "default_sensitivity_str")]
    sensitivity: String,
    created: String,
    updated: String,
}

fn default_sensitivity_str() -> String {
    "private".to_string()
}

// ---------------------------------------------------------------------------
// Compiled regexes (lazy statics)
// ---------------------------------------------------------------------------

/// Matches inline metadata brackets: `[key: value]`
pub static METADATA_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\[(\w+):\s*([^\]]+)\]").expect("metadata regex"));

/// Matches strikethrough content: `~~text~~`
static STRIKETHROUGH_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"~~(.+?)~~").expect("strikethrough regex"));

/// Matches wikilinks: `[[target]]`
static WIKILINK_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\[\[([^\]]+)\]\]").expect("wikilink regex"));

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Parse a vault entity Markdown file into structured types.
///
/// The content should be the full text of a `.md` file from `knowledge-base/`.
pub fn parse_entity_file(content: &str) -> Result<ParsedEntityFile, VaultParseError> {
    let (frontmatter_str, body) = extract_frontmatter(content)?;
    let fm: EntityFrontmatter = serde_yml::from_str(&frontmatter_str)
        .map_err(|e| VaultParseError::InvalidYaml(e.to_string()))?;

    let entity_type = parse_entity_type(&fm.entity_type)?;
    let space = parse_space(&fm.space)?;
    let sensitivity = parse_sensitivity(&fm.sensitivity)?;

    // Extract entity name from the first `# Heading` in the body, falling back to id.
    let name = extract_heading(&body).unwrap_or_else(|| fm.id.clone());

    let mut attributes = serde_json::Map::new();
    if !fm.aliases.is_empty() {
        attributes.insert("aliases".to_string(), serde_json::json!(fm.aliases));
    }
    if let Some(schema) = fm.schema.clone() {
        attributes.insert("schema".to_string(), serde_json::Value::String(schema));
    }

    let entity = Entity {
        id: fm.id.clone(),
        entity_type,
        name,
        attributes: serde_json::Value::Object(attributes),
        sensitivity,
        allowed_models: AllowedModels::Any,
        space,
        status: EntityStatus::Active,
        merged_into: None,
        created_at: fm.created.clone(),
        updated_at: fm.updated.clone(),
    };

    // Parse body sections.
    let sections = split_sections(&body);

    let mut memories = Vec::new();

    // Parse ## Facts
    if let Some(facts_text) = sections.get("facts") {
        for line in fact_lines(facts_text) {
            if let Some(mem) = parse_fact_line(
                &line,
                &fm.id,
                sensitivity,
                &fm.created,
                &fm.updated,
                MemoryStatus::Active,
            ) {
                memories.push(mem);
            }
        }
    }

    // Parse ## History -> ### Archived Facts
    if let Some(archived_text) = history_subsection_lines(sections.get("history"), "Archived Facts")
    {
        for line in archived_text {
            if let Some(mem) =
                parse_archived_line(&line, &fm.id, sensitivity, &fm.created, &fm.updated)
            {
                memories.push(mem);
            }
        }
    }

    // Parse ## Relationships
    let mut relationships = Vec::new();
    if let Some(rels_text) = sections.get("relationships") {
        for line in fact_lines(rels_text) {
            let mut parsed =
                parse_relationship_line(&line, &fm.id, sensitivity, &fm.created, &fm.updated);
            relationships.append(&mut parsed);
        }
    }

    Ok(ParsedEntityFile {
        entity,
        memories,
        relationships,
    })
}

/// Generate a deterministic fact ID from entity ID and fact text.
///
/// Format: `{entity_id}:{sha256(fact_text)[:12]}`
pub fn fact_id(entity_id: &str, fact_text: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(fact_text.as_bytes());
    let hash = hex_encode(hasher.finalize());
    format!("{}:{}", entity_id, &hash[..12])
}

// ---------------------------------------------------------------------------
// Internal parsing helpers
// ---------------------------------------------------------------------------

/// Extract YAML frontmatter from between `---` delimiters.
fn extract_frontmatter(content: &str) -> Result<(String, String), VaultParseError> {
    let trimmed = content.trim_start();
    if !trimmed.starts_with("---") {
        return Err(VaultParseError::MissingFrontmatter);
    }

    // Find the closing ---
    let after_first = &trimmed[3..];
    let close_pos = after_first
        .find("\n---")
        .ok_or(VaultParseError::MissingFrontmatter)?;

    let yaml = after_first[..close_pos].trim().to_string();
    // Body starts after the closing --- and its newline
    let body_start = close_pos + 4; // "\n---"
    let body = if body_start < after_first.len() {
        after_first[body_start..].to_string()
    } else {
        String::new()
    };

    Ok((yaml, body))
}

/// Extract the first `# Heading` from the body text.
fn extract_heading(body: &str) -> Option<String> {
    for line in body.lines() {
        let trimmed = line.trim();
        if let Some(heading) = trimmed.strip_prefix("# ") {
            return Some(heading.trim().to_string());
        }
    }
    None
}

/// Split body into named sections by `## Heading`.
/// Returns a map of lowercase section name → section content.
fn split_sections(body: &str) -> std::collections::HashMap<String, String> {
    let mut sections = std::collections::HashMap::new();
    let mut current_section: Option<String> = None;
    let mut current_content = String::new();

    for line in body.lines() {
        let trimmed = line.trim();
        if let Some(heading) = trimmed.strip_prefix("## ") {
            // Save previous section
            if let Some(ref name) = current_section {
                sections.insert(name.clone(), current_content.trim().to_string());
            }
            current_section = Some(heading.trim().to_lowercase());
            current_content = String::new();
        } else if current_section.is_some() {
            current_content.push_str(line);
            current_content.push('\n');
        }
    }

    // Save last section
    if let Some(name) = current_section {
        sections.insert(name, current_content.trim().to_string());
    }

    sections
}

/// Extract list item lines (starting with `- `) from section text.
fn fact_lines(section: &str) -> Vec<String> {
    section
        .lines()
        .filter_map(|line| line.trim().strip_prefix("- ").map(|rest| rest.to_string()))
        .collect()
}

fn history_subsection_lines(
    history_section: Option<&String>,
    subsection_name: &str,
) -> Option<Vec<String>> {
    let history_section = history_section?;
    let header = format!("### {}", subsection_name);
    let mut in_subsection = false;
    let mut lines = Vec::new();

    for raw_line in history_section.lines() {
        let trimmed = raw_line.trim();
        if trimmed.starts_with("### ") {
            in_subsection = trimmed == header;
            continue;
        }
        if in_subsection {
            if let Some(rest) = trimmed.strip_prefix("- ") {
                lines.push(rest.to_string());
            }
        }
    }

    if lines.is_empty() {
        None
    } else {
        Some(lines)
    }
}

/// Extract all `[key: value]` metadata pairs from a line.
fn extract_metadata(line: &str) -> std::collections::HashMap<String, String> {
    METADATA_RE
        .captures_iter(line)
        .map(|cap| (cap[1].to_lowercase(), cap[2].trim().to_string()))
        .collect()
}

/// Strip all `[key: value]` metadata from a line, returning clean fact text.
fn strip_metadata(line: &str) -> String {
    METADATA_RE.replace_all(line, "").trim().to_string()
}

/// Parse a fact line from `## Facts` into a `Memory`.
fn parse_fact_line(
    line: &str,
    entity_id: &str,
    sensitivity: Sensitivity,
    created_at: &str,
    updated_at: &str,
    status: MemoryStatus,
) -> Option<Memory> {
    let metadata = extract_metadata(line);
    let fact_text = strip_metadata(line);

    if fact_text.is_empty() {
        return None;
    }

    let confidence = metadata
        .get("confidence")
        .and_then(|v| v.parse::<f64>().ok())
        .unwrap_or(0.8);

    let fact_type = metadata
        .get("type")
        .and_then(|v| v.parse::<FactType>().ok());

    let source = metadata.get("source").cloned();

    let id = fact_id(entity_id, &fact_text);

    Some(Memory {
        id,
        entity_id: entity_id.to_string(),
        fact: fact_text,
        confidence,
        disposition: FactDisposition::UserConfirmed,
        sensitivity,
        valid_from: created_at.to_string(),
        valid_to: None,
        status,
        superseded_by: None,
        created_at: created_at.to_string(),
        updated_at: updated_at.to_string(),
        fact_type,
        authored_by: source.as_deref().map(|s| {
            if s == "manual" {
                "user".to_string()
            } else {
                "agent".to_string()
            }
        }),
        supersedes: None,
        depends_on: Vec::new(),
        fsrs: None,
    })
}

/// Parse an archived fact line from `## History` -> `### Archived Facts`
/// into a `Memory`.
fn parse_archived_line(
    line: &str,
    entity_id: &str,
    sensitivity: Sensitivity,
    created_at: &str,
    updated_at: &str,
) -> Option<Memory> {
    // Extract strikethrough content
    let fact_text = if let Some(caps) = STRIKETHROUGH_RE.captures(line) {
        caps[1].to_string()
    } else {
        // Fallback: strip ~~ manually if regex doesn't match
        let stripped = line.replace("~~", "");
        strip_metadata(&stripped)
    };

    let metadata = extract_metadata(line);
    let clean_fact = strip_metadata(&fact_text);

    if clean_fact.is_empty() {
        return None;
    }

    let confidence = metadata
        .get("confidence")
        .and_then(|v| v.parse::<f64>().ok())
        .unwrap_or(0.8);

    let fact_type = metadata
        .get("type")
        .and_then(|v| v.parse::<FactType>().ok());

    let source = metadata.get("source").cloned();

    // For archived facts, the ID is based on the original fact text
    let id = fact_id(entity_id, &clean_fact);

    // Parse archived date if present (in `[archived: date, reason: text]` format)
    let archived_date = metadata.get("archived").map(|v| {
        // The value may be "2026-03-23, reason: cost analysis contradicts"
        // In that case, just take the date part (before the comma)
        if let Some(comma_pos) = v.find(',') {
            v[..comma_pos].trim().to_string()
        } else {
            v.clone()
        }
    });

    Some(Memory {
        id,
        entity_id: entity_id.to_string(),
        fact: clean_fact,
        confidence,
        disposition: FactDisposition::UserConfirmed,
        sensitivity,
        valid_from: created_at.to_string(),
        valid_to: archived_date,
        status: MemoryStatus::Archived,
        superseded_by: None,
        created_at: created_at.to_string(),
        updated_at: updated_at.to_string(),
        fact_type,
        authored_by: source.as_deref().map(|s| {
            if s == "manual" {
                "user".to_string()
            } else {
                "agent".to_string()
            }
        }),
        supersedes: None,
        depends_on: Vec::new(),
        fsrs: None,
    })
}

/// Parse a relationship line into one or more `Relationship`s.
///
/// Format: `{relation_type}: [[target1]], [[target2]] [since: date]`
fn parse_relationship_line(
    line: &str,
    from_entity: &str,
    sensitivity: Sensitivity,
    created_at: &str,
    updated_at: &str,
) -> Vec<Relationship> {
    // Split on first `:` to get relation type
    let colon_pos = match line.find(':') {
        Some(pos) => pos,
        None => return Vec::new(),
    };

    let relation_type = line[..colon_pos].trim().to_string();
    let rest = &line[colon_pos + 1..];

    let metadata = extract_metadata(rest);
    let valid_from = metadata
        .get("since")
        .cloned()
        .unwrap_or_else(|| created_at.to_string());

    // Extract all [[wikilink]] targets
    WIKILINK_RE
        .captures_iter(rest)
        .map(|cap| {
            let target = cap[1].trim().to_string();
            let id = format!("{}:{}:{}", from_entity, relation_type, target);

            Relationship {
                id,
                from_entity: from_entity.to_string(),
                to_entity: target,
                relation_type: relation_type.clone(),
                strength: 1.0,
                valid_from: valid_from.clone(),
                valid_to: None,
                sensitivity,
                allowed_models: AllowedModels::Any,
                status: RelationshipStatus::Active,
                created_at: created_at.to_string(),
                updated_at: updated_at.to_string(),
            }
        })
        .collect()
}

/// Parse entity type string, handling aliases.
fn parse_entity_type(s: &str) -> Result<EntityType, VaultParseError> {
    match s.to_lowercase().as_str() {
        "person" => Ok(EntityType::Person),
        "project" => Ok(EntityType::Project),
        "org" | "organization" | "organisation" => Ok(EntityType::Org),
        "tool" => Ok(EntityType::Tool),
        "preference" => Ok(EntityType::Preference),
        "concept" => Ok(EntityType::Concept),
        "task" => Ok(EntityType::Task),
        _ => Err(VaultParseError::InvalidEntityType(s.to_string())),
    }
}

/// Parse memory space string.
fn parse_space(s: &str) -> Result<MemorySpace, VaultParseError> {
    match s.to_lowercase().as_str() {
        "knowledge" => Ok(MemorySpace::Knowledge),
        "identity" => Ok(MemorySpace::Identity),
        "operations" => Ok(MemorySpace::Operations),
        _ => Err(VaultParseError::InvalidSpace(s.to_string())),
    }
}

/// Parse sensitivity string.
fn parse_sensitivity(s: &str) -> Result<Sensitivity, VaultParseError> {
    match s.to_lowercase().as_str() {
        "shareable" => Ok(Sensitivity::Shareable),
        "restricted" => Ok(Sensitivity::Restricted),
        "private" => Ok(Sensitivity::Private),
        _ => Err(VaultParseError::InvalidSensitivity(s.to_string())),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// The example from the design doc.
    const KUBERNETES_EXAMPLE: &str = r#"---
id: kubernetes
type: concept
aliases: [k8s, kube]
space: knowledge
sensitivity: shareable
created: 2026-01-15T00:00:00Z
updated: 2026-03-23T14:30:00Z
---

# Kubernetes

## Facts
- Preferred orchestration platform for production [source: chat-2026-01-15] [type: decision] [confidence: 0.9]
- Supports auto-scaling via HPA and VPA [source: article-abc123] [type: finding] [confidence: 0.95]
- Monthly cost: $2400 for current workload [source: chat-2026-03-23] [type: finding] [confidence: 0.85]

## Relationships
- replaced_by: [[nomad]] [since: 2026-03-23]
- used_with: [[docker]], [[helm]]
- part_of: [[infrastructure]]

## History
### Archived Facts
- ~~Scales well for our use case~~ [archived: 2026-03-23, reason: cost analysis contradicts] [source: chat-2026-01-15]
"#;

    #[test]
    fn parse_design_doc_example() {
        let result = parse_entity_file(KUBERNETES_EXAMPLE).unwrap();

        // Entity
        assert_eq!(result.entity.id, "kubernetes");
        assert_eq!(result.entity.entity_type, EntityType::Concept);
        assert_eq!(result.entity.name, "Kubernetes");
        assert_eq!(result.entity.space, MemorySpace::Knowledge);
        assert_eq!(result.entity.sensitivity, Sensitivity::Shareable);
        assert_eq!(result.entity.status, EntityStatus::Active);
        assert_eq!(result.entity.created_at, "2026-01-15T00:00:00Z");
        assert_eq!(result.entity.updated_at, "2026-03-23T14:30:00Z");

        // Aliases stored in attributes
        let aliases = result.entity.attributes["aliases"].as_array().unwrap();
        assert_eq!(aliases.len(), 2);
        assert_eq!(aliases[0], "k8s");
        assert_eq!(aliases[1], "kube");
    }

    #[test]
    fn parse_optional_schema_metadata_into_attributes() {
        let content = r#"---
id: 5x5-strength
type: task
schema: training_program
space: operations
created: 2026-04-06T00:00:00Z
updated: 2026-04-06T00:00:00Z
---

# 5x5 Strength
"#;

        let result = parse_entity_file(content).unwrap();
        assert_eq!(result.entity.entity_type, EntityType::Task);
        assert_eq!(result.entity.space, MemorySpace::Operations);
        assert_eq!(result.entity.attributes["schema"], "training_program");
    }

    #[test]
    fn parse_facts_section() {
        let result = parse_entity_file(KUBERNETES_EXAMPLE).unwrap();

        // 3 active facts + 1 archived = 4 total
        assert_eq!(result.memories.len(), 4);

        // Active facts
        let active: Vec<_> = result
            .memories
            .iter()
            .filter(|m| m.status == MemoryStatus::Active)
            .collect();
        assert_eq!(active.len(), 3);

        // First fact
        assert_eq!(
            active[0].fact,
            "Preferred orchestration platform for production"
        );
        assert!((active[0].confidence - 0.9).abs() < f64::EPSILON);
        assert_eq!(active[0].fact_type, Some(FactType::Decision));
        assert_eq!(active[0].entity_id, "kubernetes");
        assert_eq!(active[0].authored_by, Some("agent".to_string()));

        // Second fact
        assert_eq!(active[1].fact, "Supports auto-scaling via HPA and VPA");
        assert!((active[1].confidence - 0.95).abs() < f64::EPSILON);
        assert_eq!(active[1].fact_type, Some(FactType::Finding));

        // Third fact
        assert!(active[2].fact.contains("$2400"));
        assert!((active[2].confidence - 0.85).abs() < f64::EPSILON);
    }

    #[test]
    fn parse_archived_facts_from_history_section() {
        let result = parse_entity_file(KUBERNETES_EXAMPLE).unwrap();

        let archived: Vec<_> = result
            .memories
            .iter()
            .filter(|m| m.status == MemoryStatus::Archived)
            .collect();
        assert_eq!(archived.len(), 1);
        assert_eq!(archived[0].fact, "Scales well for our use case");
        assert_eq!(archived[0].valid_to, Some("2026-03-23".to_string()));
    }

    #[test]
    fn parse_relationships_section() {
        let result = parse_entity_file(KUBERNETES_EXAMPLE).unwrap();

        // replaced_by: 1 target, used_with: 2 targets, part_of: 1 target = 4 total
        assert_eq!(result.relationships.len(), 4);

        let replaced = result
            .relationships
            .iter()
            .find(|r| r.relation_type == "replaced_by")
            .unwrap();
        assert_eq!(replaced.from_entity, "kubernetes");
        assert_eq!(replaced.to_entity, "nomad");
        assert_eq!(replaced.valid_from, "2026-03-23");

        let used_with: Vec<_> = result
            .relationships
            .iter()
            .filter(|r| r.relation_type == "used_with")
            .collect();
        assert_eq!(used_with.len(), 2);
        let targets: Vec<&str> = used_with.iter().map(|r| r.to_entity.as_str()).collect();
        assert!(targets.contains(&"docker"));
        assert!(targets.contains(&"helm"));
    }

    #[test]
    fn fact_id_is_deterministic() {
        let id1 = fact_id("kubernetes", "Preferred orchestration platform");
        let id2 = fact_id("kubernetes", "Preferred orchestration platform");
        assert_eq!(id1, id2);
        assert!(id1.starts_with("kubernetes:"));
        // 12 hex chars after the colon
        assert_eq!(id1.split(':').nth(1).unwrap().len(), 12);
    }

    #[test]
    fn fact_id_changes_with_text() {
        let id1 = fact_id("k8s", "fact A");
        let id2 = fact_id("k8s", "fact B");
        assert_ne!(id1, id2);
    }

    #[test]
    fn parse_minimal_entity() {
        let content = r#"---
id: rust
type: tool
space: knowledge
created: 2026-01-01T00:00:00Z
updated: 2026-01-01T00:00:00Z
---

# Rust

## Facts
- Systems programming language [source: manual]
"#;
        let result = parse_entity_file(content).unwrap();
        assert_eq!(result.entity.id, "rust");
        assert_eq!(result.entity.entity_type, EntityType::Tool);
        assert_eq!(result.entity.sensitivity, Sensitivity::Private); // default
        assert_eq!(result.memories.len(), 1);
        assert_eq!(result.memories[0].fact, "Systems programming language");
        assert_eq!(result.memories[0].authored_by, Some("user".to_string())); // source: manual
        assert_eq!(result.relationships.len(), 0);
    }

    #[test]
    fn parse_no_facts_section() {
        let content = r#"---
id: empty
type: concept
space: knowledge
created: 2026-01-01T00:00:00Z
updated: 2026-01-01T00:00:00Z
---

# Empty Entity
"#;
        let result = parse_entity_file(content).unwrap();
        assert_eq!(result.entity.id, "empty");
        assert_eq!(result.memories.len(), 0);
        assert_eq!(result.relationships.len(), 0);
    }

    #[test]
    fn parse_organization_alias() {
        let content = r#"---
id: acme
type: organization
space: knowledge
created: 2026-01-01T00:00:00Z
updated: 2026-01-01T00:00:00Z
---

# Acme Corp
"#;
        let result = parse_entity_file(content).unwrap();
        assert_eq!(result.entity.entity_type, EntityType::Org);
    }

    #[test]
    fn parse_identity_space() {
        let content = r#"---
id: preferences
type: preference
space: identity
sensitivity: private
created: 2026-01-01T00:00:00Z
updated: 2026-01-01T00:00:00Z
---

# My Preferences

## Facts
- Prefers dark mode [source: manual] [type: preference]
"#;
        let result = parse_entity_file(content).unwrap();
        assert_eq!(result.entity.space, MemorySpace::Identity);
        assert_eq!(result.memories[0].fact_type, Some(FactType::Preference));
    }

    #[test]
    fn missing_frontmatter_errors() {
        let content = "# No Frontmatter\n\nJust a regular markdown file.";
        let err = parse_entity_file(content).unwrap_err();
        assert!(matches!(err, VaultParseError::MissingFrontmatter));
    }

    #[test]
    fn invalid_yaml_errors() {
        let content = "---\n[invalid yaml\n---\n";
        let err = parse_entity_file(content).unwrap_err();
        assert!(matches!(err, VaultParseError::InvalidYaml(_)));
    }

    #[test]
    fn invalid_entity_type_errors() {
        let content = r#"---
id: test
type: spaceship
space: knowledge
created: 2026-01-01T00:00:00Z
updated: 2026-01-01T00:00:00Z
---
"#;
        let err = parse_entity_file(content).unwrap_err();
        assert!(matches!(err, VaultParseError::InvalidEntityType(_)));
    }

    #[test]
    fn default_confidence_is_0_8() {
        let content = r#"---
id: test
type: concept
space: knowledge
created: 2026-01-01T00:00:00Z
updated: 2026-01-01T00:00:00Z
---

# Test

## Facts
- A fact without confidence metadata [source: manual]
"#;
        let result = parse_entity_file(content).unwrap();
        assert!((result.memories[0].confidence - 0.8).abs() < f64::EPSILON);
    }

    #[test]
    fn archived_fact_extracts_reason_from_history_metadata() {
        let content = r#"---
id: test
type: concept
space: knowledge
created: 2026-01-01T00:00:00Z
updated: 2026-03-23T00:00:00Z
---

# Test

## Relationships

## History
### Archived Facts
- ~~Old fact text~~ [archived: 2026-03-23, reason: superseded by new data] [source: chat-001]

"#;
        let result = parse_entity_file(content).unwrap();
        assert_eq!(result.memories.len(), 1);
        assert_eq!(result.memories[0].status, MemoryStatus::Archived);
        assert_eq!(result.memories[0].fact, "Old fact text");
        assert_eq!(result.memories[0].valid_to, Some("2026-03-23".to_string()));
    }

    #[test]
    fn heading_used_as_entity_name() {
        let content = r#"---
id: k8s
type: tool
space: knowledge
created: 2026-01-01T00:00:00Z
updated: 2026-01-01T00:00:00Z
---

# Kubernetes (Container Orchestration)
"#;
        let result = parse_entity_file(content).unwrap();
        assert_eq!(result.entity.name, "Kubernetes (Container Orchestration)");
    }

    #[test]
    fn fallback_to_id_when_no_heading() {
        let content = r#"---
id: my-entity
type: concept
space: knowledge
created: 2026-01-01T00:00:00Z
updated: 2026-01-01T00:00:00Z
---

No heading here, just text.
"#;
        let result = parse_entity_file(content).unwrap();
        assert_eq!(result.entity.name, "my-entity");
    }

    #[test]
    fn multiple_relationships_per_line() {
        let content = r#"---
id: devops
type: concept
space: knowledge
created: 2026-01-01T00:00:00Z
updated: 2026-01-01T00:00:00Z
---

# DevOps

## Relationships
- includes: [[ci-cd]], [[monitoring]], [[logging]]
"#;
        let result = parse_entity_file(content).unwrap();
        assert_eq!(result.relationships.len(), 3);
        let targets: Vec<&str> = result
            .relationships
            .iter()
            .map(|r| r.to_entity.as_str())
            .collect();
        assert!(targets.contains(&"ci-cd"));
        assert!(targets.contains(&"monitoring"));
        assert!(targets.contains(&"logging"));
        assert!(result
            .relationships
            .iter()
            .all(|r| r.relation_type == "includes"));
    }
}
