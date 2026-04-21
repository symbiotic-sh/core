//! Auto-generated entity profile Markdown documents.
//!
//! Produces one Markdown brief per entity in the Neural Graph, stored beside
//! the canonical record as `knowledge-base/ledger/{type}/{slug}/{slug}.brief.md`.
//! These briefs aggregate all
//! known facts, relationships, and thread references for a given entity.
//!
//! Entity profiles are auto-maintained by the Reweave stage and periodic
//! synthesis. The user never needs to edit them directly (though they can).
//!
//! See `docs/design/memory-system.md` Layer 3 — Entity Profiles.

use std::fmt::Write as FmtWrite;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use symbiotic_matrix::events::MatrixEventEnvelope;

// Re-exported domain types from symbiotic-memory.
// These will resolve once the crate dependency is wired in Cargo.toml.
use symbiotic_memory::types::{Entity, Memory, MemoryStatus, Relationship};
use symbiotic_memory::vault_layout::{find_canonical_entity_file, generated_brief_relative_path};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EntityProfileViewData {
    pub summary: Option<String>,
    pub facts: Vec<EntityProfileFactView>,
    pub archived_facts: Vec<EntityProfileArchivedFactView>,
    pub relationships: Vec<EntityProfileRelationshipView>,
    pub relationship_history: Vec<EntityProfileRelationshipHistoryView>,
    pub referenced_in: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EntityProfileFactView {
    pub text: String,
    pub date: Option<String>,
    pub confidence: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EntityProfileArchivedFactView {
    pub text: String,
    pub archived_at: Option<String>,
    pub confidence: Option<f64>,
    pub commit: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EntityProfileRelationshipView {
    pub rel_type: String,
    pub direction: String,
    pub target: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EntityProfileRelationshipHistoryView {
    pub action: String,
    pub rel_type: String,
    pub target: String,
    pub replacement_target: Option<String>,
    pub changed_at: Option<String>,
    pub reason: Option<String>,
    pub commit: Option<String>,
}

/// Generates entity profile Markdown files.
///
/// One generated brief per entity at
/// `{kb_root}/ledger/{type_dir}/{slug}/{slug}.brief.md`.
pub struct EntityProfileGenerator {
    kb_root: PathBuf,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct EntityProfileHistoryData {
    archived_facts: Vec<EntityProfileArchivedFactView>,
    relationship_history: Vec<EntityProfileRelationshipHistoryView>,
}

impl EntityProfileGenerator {
    /// Create a new generator targeting the given knowledge-base root.
    pub fn new(kb_root: impl AsRef<Path>) -> Self {
        Self {
            kb_root: kb_root.as_ref().to_path_buf(),
        }
    }

    /// Generate an entity brief at
    /// `knowledge-base/ledger/{type}/{slug}/{slug}.brief.md`.
    ///
    /// Returns the path to the written file.
    ///
    /// # Arguments
    /// * `entity` — the entity to generate a profile for
    /// * `facts` — all active memories (facts) for this entity
    /// * `relationships` — all relationships involving this entity
    /// * `referenced_in` — thread titles where this entity appears (as wikilinks)
    pub fn generate(
        &self,
        entity: &Entity,
        facts: &[Memory],
        relationships: &[Relationship],
        referenced_in: &[String],
    ) -> Result<PathBuf> {
        let history = Self::load_history_data(&self.kb_root, &entity.id)?;
        let view = Self::build_view(entity, facts, relationships, &history, referenced_in);
        let slug = slugify(&entity.name);
        let output_path = self
            .kb_root
            .join(generated_brief_relative_path(&slug, entity.entity_type));
        let entity_dir = output_path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("generated brief path missing parent"))?;
        fs::create_dir_all(entity_dir).with_context(|| {
            format!(
                "failed to create entity artifact directory {}",
                entity_dir.display()
            )
        })?;

        let doc = Self::render_markdown(entity, &view);

        // --- Write file ---
        fs::write(&output_path, &doc)
            .with_context(|| format!("failed to write entity profile {}", output_path.display()))?;

        Ok(output_path)
    }

    pub(crate) fn build_view(
        entity: &Entity,
        facts: &[Memory],
        relationships: &[Relationship],
        history: &EntityProfileHistoryData,
        referenced_in: &[String],
    ) -> EntityProfileViewData {
        let summary = entity
            .attributes
            .get("description")
            .and_then(|value| value.as_str())
            .map(str::to_string);

        let active_facts = facts
            .iter()
            .filter(|fact| fact.status == MemoryStatus::Active)
            .map(|fact| EntityProfileFactView {
                text: fact.fact.clone(),
                date: Some(truncate_date(&fact.valid_from).to_string()),
                confidence: Some(fact.confidence),
            })
            .collect();

        let archived_facts = if history.archived_facts.is_empty() {
            facts
                .iter()
                .filter(|fact| fact.status == MemoryStatus::Archived)
                .map(|fact| EntityProfileArchivedFactView {
                    text: fact.fact.clone(),
                    archived_at: fact
                        .valid_to
                        .as_deref()
                        .map(truncate_date)
                        .map(str::to_string),
                    confidence: Some(fact.confidence),
                    commit: None,
                })
                .collect()
        } else {
            history.archived_facts.clone()
        };

        let relationships = relationships
            .iter()
            .map(|rel| {
                let (direction, target) = if rel.from_entity == entity.id {
                    ("->".to_string(), rel.to_entity.clone())
                } else {
                    ("<-".to_string(), rel.from_entity.clone())
                };
                EntityProfileRelationshipView {
                    rel_type: rel.relation_type.clone(),
                    direction,
                    target,
                }
            })
            .collect();

        EntityProfileViewData {
            summary,
            facts: active_facts,
            archived_facts,
            relationships,
            relationship_history: history.relationship_history.clone(),
            referenced_in: referenced_in.to_vec(),
        }
    }

    fn render_markdown(entity: &Entity, view: &EntityProfileViewData) -> String {
        let mut doc = String::with_capacity(2048);

        // --- YAML frontmatter ---
        writeln!(doc, "---").unwrap();
        writeln!(doc, "type: brief").unwrap();
        writeln!(doc, "entity_type: {}", entity.entity_type.frontmatter_str()).unwrap();
        writeln!(doc, "id: {}", entity.id).unwrap();

        // Aliases from attributes.
        if let Some(aliases) = entity.attributes.get("aliases").and_then(|v| v.as_array()) {
            let alias_strs: Vec<&str> = aliases.iter().filter_map(|a| a.as_str()).collect();
            if !alias_strs.is_empty() {
                writeln!(doc, "aliases: [{}]", alias_strs.join(", ")).unwrap();
            }
        }

        let fact_dates: Vec<&str> = view
            .facts
            .iter()
            .filter_map(|fact| fact.date.as_deref())
            .collect();
        if let Some(earliest) = fact_dates.iter().copied().min() {
            writeln!(doc, "first_seen: {earliest}").unwrap();
        }
        if let Some(latest) = fact_dates.iter().copied().max() {
            writeln!(doc, "last_referenced: {latest}").unwrap();
        }

        writeln!(doc, "---").unwrap();
        writeln!(doc).unwrap();

        // --- Title ---
        writeln!(doc, "# {}", entity.name).unwrap();
        writeln!(doc).unwrap();

        // --- Type line ---
        writeln!(
            doc,
            "**Type:** {}",
            capitalize(entity.entity_type.singular_label())
        )
        .unwrap();
        writeln!(doc).unwrap();

        // --- Summary from attributes ---
        if let Some(desc) = &view.summary {
            writeln!(doc, "## Summary").unwrap();
            writeln!(doc).unwrap();
            writeln!(doc, "{desc}").unwrap();
            writeln!(doc).unwrap();
        }

        // --- Facts ---
        if !view.facts.is_empty() {
            writeln!(doc, "## Facts").unwrap();
            writeln!(doc).unwrap();
            for fact in &view.facts {
                match (fact.date.as_deref(), fact.confidence) {
                    (Some(date), Some(confidence)) => {
                        writeln!(
                            doc,
                            "- {} ({}, confidence: {:.2})",
                            fact.text, date, confidence
                        )
                        .unwrap();
                    }
                    (Some(date), None) => {
                        writeln!(doc, "- {} ({})", fact.text, date).unwrap();
                    }
                    (None, Some(confidence)) => {
                        writeln!(doc, "- {} (confidence: {:.2})", fact.text, confidence).unwrap();
                    }
                    (None, None) => {
                        writeln!(doc, "- {}", fact.text).unwrap();
                    }
                }
            }
            writeln!(doc).unwrap();
        }

        // --- Referenced In ---
        if !view.referenced_in.is_empty() {
            writeln!(doc, "## Referenced In").unwrap();
            writeln!(doc).unwrap();
            for thread_title in &view.referenced_in {
                writeln!(doc, "- [[Thread: {thread_title}]]").unwrap();
            }
            writeln!(doc).unwrap();
        }

        // --- Relationships ---
        if !view.relationships.is_empty() {
            writeln!(doc, "## Relationships").unwrap();
            writeln!(doc).unwrap();
            for rel in &view.relationships {
                writeln!(
                    doc,
                    "- {} {} [[{}]]",
                    rel.rel_type, rel.direction, rel.target
                )
                .unwrap();
            }
            writeln!(doc).unwrap();
        }

        if !view.archived_facts.is_empty() || !view.relationship_history.is_empty() {
            writeln!(doc, "## History").unwrap();
            if !view.archived_facts.is_empty() {
                writeln!(doc, "### Archived Facts").unwrap();
                for fact in &view.archived_facts {
                    match (fact.archived_at.as_deref(), fact.confidence) {
                        (Some(archived_at), Some(confidence)) => {
                            let commit = fact
                                .commit
                                .as_ref()
                                .map(|value| format!(", commit: {value}"))
                                .unwrap_or_default();
                            writeln!(
                                doc,
                                "- ~~{}~~ [archived: {}, confidence: {:.2}{}]",
                                fact.text, archived_at, confidence, commit
                            )
                            .unwrap();
                        }
                        (Some(archived_at), None) => {
                            let commit = fact
                                .commit
                                .as_ref()
                                .map(|value| format!(", commit: {value}"))
                                .unwrap_or_default();
                            writeln!(
                                doc,
                                "- ~~{}~~ [archived: {}{}]",
                                fact.text, archived_at, commit
                            )
                            .unwrap();
                        }
                        (None, Some(confidence)) => {
                            let commit = fact
                                .commit
                                .as_ref()
                                .map(|value| format!(", commit: {value}"))
                                .unwrap_or_default();
                            writeln!(
                                doc,
                                "- ~~{}~~ [confidence: {:.2}{}]",
                                fact.text, confidence, commit
                            )
                            .unwrap();
                        }
                        (None, None) => {
                            if let Some(commit) = &fact.commit {
                                writeln!(doc, "- ~~{}~~ [commit: {}]", fact.text, commit).unwrap();
                            } else {
                                writeln!(doc, "- ~~{}~~", fact.text).unwrap();
                            }
                        }
                    }
                }
                writeln!(doc).unwrap();
            }
            if !view.relationship_history.is_empty() {
                writeln!(doc, "### Relationship Changes").unwrap();
                for change in &view.relationship_history {
                    let mut metadata = Vec::new();
                    if let Some(changed_at) = &change.changed_at {
                        metadata.push(format!("changed: {changed_at}"));
                    }
                    if let Some(reason) = &change.reason {
                        metadata.push(format!("reason: {reason}"));
                    }
                    if let Some(commit) = &change.commit {
                        metadata.push(format!("commit: {commit}"));
                    }
                    let metadata = if metadata.is_empty() {
                        String::new()
                    } else {
                        format!(" [{}]", metadata.join(", "))
                    };
                    match change.action.as_str() {
                        "replaced" => {
                            let replacement =
                                change.replacement_target.as_deref().unwrap_or("unknown");
                            writeln!(
                                doc,
                                "- replaced: {} -> [[{}]] => [[{}]]{}",
                                change.rel_type, change.target, replacement, metadata
                            )
                            .unwrap();
                        }
                        _ => {
                            writeln!(
                                doc,
                                "- {}: {} -> [[{}]]{}",
                                change.action, change.rel_type, change.target, metadata
                            )
                            .unwrap();
                        }
                    }
                }
            }
            writeln!(doc).unwrap();
        }

        doc
    }

    pub fn load_relationship_history(
        kb_root: &Path,
        entity_id: &str,
    ) -> Result<Vec<EntityProfileRelationshipHistoryView>> {
        Ok(Self::load_history_data(kb_root, entity_id)?.relationship_history)
    }

    pub(crate) fn load_history_data(
        kb_root: &Path,
        entity_id: &str,
    ) -> Result<EntityProfileHistoryData> {
        let Some(path) = find_canonical_entity_file(kb_root, entity_id)
            .with_context(|| format!("failed to resolve canonical entity file for {entity_id}"))?
        else {
            return Ok(EntityProfileHistoryData::default());
        };
        let content = fs::read_to_string(&path)
            .with_context(|| format!("failed to read canonical entity file {}", path.display()))?;
        let repo_root = resolve_git_repo_root(kb_root);
        let rel_path = path
            .strip_prefix(&repo_root)
            .unwrap_or(&path)
            .to_string_lossy()
            .to_string();
        let line_commits = derive_history_line_commits(&repo_root, &rel_path, &content)?;
        Ok(EntityProfileHistoryData {
            archived_facts: parse_archived_facts(&content, &line_commits),
            relationship_history: parse_relationship_history(&content, &line_commits),
        })
    }

    /// Resolve an entity name to an existing entity ID.
    ///
    /// Resolution priority:
    /// 1. Exact match on name (case-insensitive)
    /// 2. Alias match (from entity attributes)
    /// 3. Fuzzy match (edit distance <= 2)
    /// 4. None
    pub fn resolve_entity(name: &str, existing: &[Entity]) -> Option<String> {
        let name_lower = name.to_lowercase();

        // 1. Exact match (case-insensitive).
        if let Some(entity) = existing
            .iter()
            .find(|e| e.name.to_lowercase() == name_lower)
        {
            return Some(entity.id.clone());
        }

        // 2. Alias match.
        for entity in existing {
            if let Some(aliases) = entity.attributes.get("aliases").and_then(|v| v.as_array()) {
                for alias in aliases {
                    if let Some(alias_str) = alias.as_str() {
                        if alias_str.to_lowercase() == name_lower {
                            return Some(entity.id.clone());
                        }
                    }
                }
            }
        }

        // 3. Fuzzy match (Levenshtein edit distance <= 2).
        let mut best_match: Option<(&Entity, usize)> = None;
        for entity in existing {
            let dist = levenshtein(&name_lower, &entity.name.to_lowercase());
            if dist <= 2 {
                match best_match {
                    Some((_, best_dist)) if dist < best_dist => {
                        best_match = Some((entity, dist));
                    }
                    None => {
                        best_match = Some((entity, dist));
                    }
                    _ => {}
                }
            }
        }
        if let Some((entity, _)) = best_match {
            return Some(entity.id.clone());
        }

        // 4. No match.
        None
    }

    /// Return the expected file path for a given entity.
    pub fn profile_path(&self, entity: &Entity) -> PathBuf {
        let slug = slugify(&entity.name);
        self.kb_root
            .join(generated_brief_relative_path(&slug, entity.entity_type))
    }
}

/// Preserve existing thread-reference titles from a previously generated
/// entity profile until they have a first-class query path in the memory store.
pub fn preserved_referenced_in_titles(profile_path: &Path) -> Result<Vec<String>> {
    if !profile_path.exists() {
        return Ok(Vec::new());
    }

    let content = std::fs::read_to_string(profile_path).with_context(|| {
        format!(
            "failed to read existing entity profile {}",
            profile_path.display()
        )
    })?;

    let mut in_referenced_in = false;
    let mut titles = Vec::new();
    for raw_line in content.lines() {
        let line = raw_line.trim();
        if line.starts_with("## ") {
            in_referenced_in = line == "## Referenced In";
            continue;
        }
        if !in_referenced_in || !line.starts_with("- [[") || !line.ends_with("]]") {
            continue;
        }

        let inner = &line[4..line.len() - 2];
        let title = inner
            .strip_prefix("Thread: ")
            .unwrap_or(inner)
            .trim()
            .to_string();
        if !title.is_empty() {
            titles.push(title);
        }
    }

    Ok(titles)
}

/// Build a `RoutedMatrixEnvelope` for an `entity_profile.updated` event.
pub fn build_entity_profile_updated_envelope(
    entity_id: &str,
    content_hash: &str,
    doc_path: &Path,
    now: u64,
    target_room: &str,
) -> crate::events::RoutedMatrixEnvelope {
    let body = format!("Entity profile updated for {entity_id}");
    let envelope = MatrixEventEnvelope::state("entity_profile.updated", now, &body)
        .with_detail_field("entity_id", entity_id)
        .with_detail_field("content_hash", content_hash)
        .with_detail_field("updated_at", now)
        .with_detail_field("path", doc_path.display().to_string())
        .with_detail_field("event_type", "entity_profile.updated");

    crate::events::RoutedMatrixEnvelope {
        room_id: target_room.to_string(),
        envelope,
    }
}

// --- Helpers ---

/// Convert a name to a filesystem-safe slug.
/// "Vue.js" → "vue-js", "Sarah (Acme Corp)" → "sarah-acme-corp"
fn slugify(name: &str) -> String {
    name.to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<&str>>()
        .join("-")
}

/// Truncate a datetime string to YYYY-MM-DD.
fn truncate_date(date_str: &str) -> &str {
    if date_str.len() >= 10 {
        &date_str[..10]
    } else {
        date_str
    }
}

/// Capitalize the first letter of a string.
fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        None => String::new(),
        Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
    }
}

fn parse_archived_facts(
    content: &str,
    line_commits: &std::collections::HashMap<String, String>,
) -> Vec<EntityProfileArchivedFactView> {
    let mut in_history = false;
    let mut in_archived_facts = false;
    let mut facts = Vec::new();

    for raw_line in content.lines() {
        let line = raw_line.trim();
        if line.starts_with("## ") {
            in_history = line == "## History";
            in_archived_facts = false;
            continue;
        }
        if !in_history {
            continue;
        }
        if line.starts_with("### ") {
            in_archived_facts = line == "### Archived Facts";
            continue;
        }
        if !in_archived_facts || !line.starts_with("- ") {
            continue;
        }

        let text = parse_archived_fact_text(line);
        if text.is_empty() {
            continue;
        }

        facts.push(EntityProfileArchivedFactView {
            text,
            archived_at: parse_history_metadata_fragment(line, "archived"),
            confidence: parse_history_metadata_fragment(line, "confidence")
                .and_then(|value| value.parse::<f64>().ok()),
            commit: line_commits.get(line).cloned(),
        });
    }

    facts
}

fn parse_relationship_history(
    content: &str,
    line_commits: &std::collections::HashMap<String, String>,
) -> Vec<EntityProfileRelationshipHistoryView> {
    let mut in_history = false;
    let mut in_relationship_changes = false;
    let mut changes = Vec::new();

    for raw_line in content.lines() {
        let line = raw_line.trim();
        if line.starts_with("## ") {
            in_history = line == "## History";
            in_relationship_changes = false;
            continue;
        }
        if !in_history {
            continue;
        }
        if line.starts_with("### ") {
            in_relationship_changes = line == "### Relationship Changes";
            continue;
        }
        if !in_relationship_changes || !line.starts_with("- ") {
            continue;
        }

        if let Some((rel_type, target, metadata)) = parse_removed_relationship_history_line(line) {
            changes.push(EntityProfileRelationshipHistoryView {
                action: "removed".to_string(),
                rel_type,
                target,
                replacement_target: None,
                changed_at: parse_history_metadata(metadata, "changed"),
                reason: parse_history_metadata(metadata, "reason"),
                commit: parse_history_metadata(metadata, "commit")
                    .or_else(|| line_commits.get(line).cloned()),
            });
            continue;
        }

        if let Some((rel_type, target, replacement_target, metadata)) =
            parse_replaced_relationship_history_line(line)
        {
            changes.push(EntityProfileRelationshipHistoryView {
                action: "replaced".to_string(),
                rel_type,
                target,
                replacement_target: Some(replacement_target),
                changed_at: parse_history_metadata(metadata, "changed"),
                reason: parse_history_metadata(metadata, "reason"),
                commit: parse_history_metadata(metadata, "commit")
                    .or_else(|| line_commits.get(line).cloned()),
            });
        }
    }

    changes
}

fn parse_removed_relationship_history_line(line: &str) -> Option<(String, String, &str)> {
    let body = line.strip_prefix("- removed: ")?;
    let (main, metadata) = split_history_metadata_suffix(body);
    let (rel_type, target) = main.split_once(" -> [[")?;
    let target = target.strip_suffix("]]")?;
    Some((
        rel_type.trim().to_string(),
        target.trim().to_string(),
        metadata,
    ))
}

fn parse_replaced_relationship_history_line(line: &str) -> Option<(String, String, String, &str)> {
    let body = line.strip_prefix("- replaced: ")?;
    let (main, metadata) = split_history_metadata_suffix(body);
    let (rel_type, rest) = main.split_once(" -> [[")?;
    let (target, replacement) = rest.split_once("]] => [[")?;
    let replacement = replacement.strip_suffix("]]")?;
    Some((
        rel_type.trim().to_string(),
        target.trim().to_string(),
        replacement.trim().to_string(),
        metadata,
    ))
}

fn split_history_metadata_suffix(body: &str) -> (&str, &str) {
    if let Some(index) = body.rfind(" [") {
        let main = &body[..index];
        let metadata = body[index + 2..].trim_end_matches(']').trim();
        (main, metadata)
    } else {
        (body, "")
    }
}

fn parse_history_metadata(metadata: &str, key: &str) -> Option<String> {
    metadata.split(',').find_map(|part| {
        let trimmed = part.trim();
        let prefix = format!("{key}: ");
        trimmed.strip_prefix(&prefix).map(str::to_string)
    })
}

fn parse_history_metadata_fragment(line: &str, key: &str) -> Option<String> {
    let prefix = format!("{key}: ");
    let index = line.find(&prefix)?;
    let rest = &line[index + prefix.len()..];
    let end = rest.find([',', ']']).unwrap_or(rest.len());
    Some(rest[..end].trim().to_string())
}

fn parse_archived_fact_text(line: &str) -> String {
    let body = line.trim().strip_prefix("- ").unwrap_or(line.trim());
    if let Some(start) = body.find("~~") {
        let remainder = &body[start + 2..];
        if let Some(end) = remainder.find("~~") {
            return remainder[..end].trim().to_string();
        }
    }
    body.replace("~~", "")
        .split(" [")
        .next()
        .unwrap_or("")
        .trim()
        .to_string()
}

fn derive_history_line_commits(
    repo_root: &Path,
    file_path: &str,
    content: &str,
) -> Result<std::collections::HashMap<String, String>> {
    let tracked_lines: std::collections::HashSet<String> = content
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with("- "))
        .map(str::to_string)
        .collect();
    if tracked_lines.is_empty() {
        return Ok(std::collections::HashMap::new());
    }

    let output = Command::new("git")
        .args(["log", "--format=commit:%H", "--patch", "--", file_path])
        .current_dir(repo_root)
        .output()
        .with_context(|| format!("failed to read git history for {file_path}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // Treat "not a git repo" and "empty repo" cases as "no history" so
        // tests (and bootstrap scenarios) that exercise parsing against a
        // non-initialized knowledge-base still work.
        if stderr.contains("does not have any commits")
            || stderr.contains("unknown revision")
            || stderr.contains("not a git repository")
        {
            return Ok(std::collections::HashMap::new());
        }
        anyhow::bail!("git log failed for {file_path}: {stderr}");
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut current_commit: Option<String> = None;
    let mut commits = std::collections::HashMap::new();
    for raw_line in stdout.lines() {
        if let Some(commit) = raw_line.strip_prefix("commit:") {
            current_commit = Some(commit.trim().to_string());
            continue;
        }
        if !raw_line.starts_with('+') || raw_line.starts_with("+++") {
            continue;
        }
        let added = raw_line[1..].trim();
        if !tracked_lines.contains(added) || commits.contains_key(added) {
            continue;
        }
        if let Some(commit) = &current_commit {
            commits.insert(added.to_string(), commit.clone());
        }
    }

    Ok(commits)
}

fn resolve_git_repo_root(kb_root: &Path) -> PathBuf {
    let output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(kb_root)
        .output();
    if let Ok(output) = output {
        if output.status.success() {
            let root = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !root.is_empty() {
                return PathBuf::from(root);
            }
        }
    }
    kb_root.parent().unwrap_or(kb_root).to_path_buf()
}

/// Simple Levenshtein edit distance between two strings.
fn levenshtein(a: &str, b: &str) -> usize {
    let a_chars: Vec<char> = a.chars().collect();
    let b_chars: Vec<char> = b.chars().collect();
    let m = a_chars.len();
    let n = b_chars.len();

    if m == 0 {
        return n;
    }
    if n == 0 {
        return m;
    }

    let mut prev: Vec<usize> = (0..=n).collect();
    let mut curr = vec![0usize; n + 1];

    for (i, a_ch) in a_chars.iter().enumerate() {
        curr[0] = i + 1;
        for (j, b_ch) in b_chars.iter().enumerate() {
            let cost = if a_ch == b_ch { 0 } else { 1 };
            curr[j + 1] = (prev[j + 1] + 1).min(curr[j] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }

    prev[n]
}

#[cfg(test)]
mod tests {
    use super::*;
    use symbiotic_memory::types::{
        AllowedModels, EntityStatus, EntityType, FactDisposition, MemoryStatus, RelationshipStatus,
        Sensitivity,
    };

    fn make_entity(id: &str, name: &str, etype: EntityType) -> Entity {
        Entity {
            id: id.to_string(),
            entity_type: etype,
            name: name.to_string(),
            attributes: serde_json::json!({}),
            sensitivity: Sensitivity::Shareable,
            allowed_models: AllowedModels::Any,
            space: symbiotic_core::MemorySpace::Knowledge,
            status: EntityStatus::Active,
            merged_into: None,
            created_at: "2026-03-10T00:00:00Z".to_string(),
            updated_at: "2026-03-10T00:00:00Z".to_string(),
        }
    }

    fn make_entity_with_aliases(id: &str, name: &str, aliases: &[&str]) -> Entity {
        let mut e = make_entity(id, name, EntityType::Tool);
        e.attributes = serde_json::json!({ "aliases": aliases });
        e
    }

    fn make_memory(id: &str, entity_id: &str, fact: &str, date: &str) -> Memory {
        Memory {
            id: id.to_string(),
            entity_id: entity_id.to_string(),
            fact: fact.to_string(),
            confidence: 0.9,
            disposition: FactDisposition::AutoStored,
            sensitivity: Sensitivity::Shareable,
            valid_from: date.to_string(),
            valid_to: None,
            status: MemoryStatus::Active,
            superseded_by: None,
            created_at: date.to_string(),
            updated_at: date.to_string(),
            fact_type: None,
            authored_by: None,
            supersedes: None,
            depends_on: vec![],
            fsrs: None,
        }
    }

    fn make_relationship(id: &str, from: &str, to: &str, rel_type: &str) -> Relationship {
        Relationship {
            id: id.to_string(),
            from_entity: from.to_string(),
            to_entity: to.to_string(),
            relation_type: rel_type.to_string(),
            strength: 1.0,
            valid_from: "2026-03-10T00:00:00Z".to_string(),
            valid_to: None,
            sensitivity: Sensitivity::Shareable,
            allowed_models: AllowedModels::Any,
            status: RelationshipStatus::Active,
            created_at: "2026-03-10T00:00:00Z".to_string(),
            updated_at: "2026-03-10T00:00:00Z".to_string(),
        }
    }

    #[test]
    fn slugify_produces_safe_names() {
        assert_eq!(slugify("Vue.js"), "vue-js");
        assert_eq!(slugify("Sarah (Acme Corp)"), "sarah-acme-corp");
        assert_eq!(slugify("PostgreSQL"), "postgresql");
        assert_eq!(slugify("  spaces  "), "spaces");
        assert_eq!(slugify("A--B"), "a-b");
    }

    #[test]
    fn entity_type_dir_maps_correctly() {
        assert_eq!(EntityType::Person.plural_dir(), "people");
        assert_eq!(EntityType::Project.plural_dir(), "projects");
        assert_eq!(EntityType::Tool.plural_dir(), "tools");
        assert_eq!(EntityType::Org.plural_dir(), "organizations");
    }

    #[test]
    fn levenshtein_distance() {
        assert_eq!(levenshtein("kitten", "sitting"), 3);
        assert_eq!(levenshtein("vue", "vue"), 0);
        assert_eq!(levenshtein("vue", "veu"), 2); // transpose = 2 ops
        assert_eq!(levenshtein("", "abc"), 3);
        assert_eq!(levenshtein("abc", ""), 3);
    }

    #[test]
    fn resolve_entity_exact_match() {
        let entities = vec![
            make_entity("vue-001", "Vue", EntityType::Tool),
            make_entity("react-001", "React", EntityType::Tool),
        ];
        assert_eq!(
            EntityProfileGenerator::resolve_entity("vue", &entities),
            Some("vue-001".to_string())
        );
        assert_eq!(
            EntityProfileGenerator::resolve_entity("Vue", &entities),
            Some("vue-001".to_string())
        );
    }

    #[test]
    fn resolve_entity_alias_match() {
        let entities = vec![make_entity_with_aliases(
            "vue-001",
            "Vue",
            &["Vue.js", "VueJS", "Vue 3"],
        )];
        assert_eq!(
            EntityProfileGenerator::resolve_entity("Vue.js", &entities),
            Some("vue-001".to_string())
        );
        assert_eq!(
            EntityProfileGenerator::resolve_entity("vuejs", &entities),
            Some("vue-001".to_string())
        );
    }

    #[test]
    fn resolve_entity_fuzzy_match() {
        let entities = vec![make_entity("vue-001", "Vue", EntityType::Tool)];
        // Edit distance 1 (extra char)
        assert_eq!(
            EntityProfileGenerator::resolve_entity("Vuee", &entities),
            Some("vue-001".to_string())
        );
        // Edit distance 2
        assert_eq!(
            EntityProfileGenerator::resolve_entity("Vuees", &entities),
            Some("vue-001".to_string())
        );
    }

    #[test]
    fn resolve_entity_no_match() {
        let entities = vec![make_entity("vue-001", "Vue", EntityType::Tool)];
        // Edit distance 5 — too far
        assert_eq!(
            EntityProfileGenerator::resolve_entity("PostgreSQL", &entities),
            None
        );
    }

    #[test]
    fn generate_produces_valid_profile() {
        let dir = tempfile::tempdir().unwrap();
        let gen = EntityProfileGenerator::new(dir.path());

        let mut entity = make_entity("vue-001", "Vue", EntityType::Tool);
        entity.attributes = serde_json::json!({
            "description": "JavaScript framework for building user interfaces",
            "aliases": ["Vue.js", "VueJS"]
        });

        let facts = vec![
            make_memory(
                "m1",
                "vue-001",
                "Chosen over React for better SSR story",
                "2026-03-14T10:00:00Z",
            ),
            make_memory(
                "m2",
                "vue-001",
                "Nuxt 3 template available",
                "2026-03-12T08:00:00Z",
            ),
        ];

        let rels = vec![make_relationship("r1", "vue-001", "saas-001", "used_in")];

        let referenced_in = vec![
            "SaaS Product".to_string(),
            "Competitor Analysis".to_string(),
        ];

        let path = gen
            .generate(&entity, &facts, &rels, &referenced_in)
            .unwrap();

        assert!(path.exists());
        assert!(path
            .to_str()
            .unwrap()
            .contains("ledger/tools/vue/vue.brief.md"));

        let content = fs::read_to_string(&path).unwrap();

        // Frontmatter
        assert!(content.contains("type: brief"));
        assert!(content.contains("entity_type: tool"));
        assert!(content.contains("id: vue-001"));
        assert!(content.contains("aliases: [Vue.js, VueJS]"));
        assert!(content.contains("first_seen: 2026-03-12"));
        assert!(content.contains("last_referenced: 2026-03-14"));

        // Body
        assert!(content.contains("# Vue"));
        assert!(content.contains("**Type:** Tool"));
        assert!(content.contains("## Summary"));
        assert!(content.contains("JavaScript framework"));
        assert!(content.contains("## Facts"));
        assert!(content.contains("Chosen over React"));
        assert!(content.contains("## Referenced In"));
        assert!(content.contains("[[Thread: SaaS Product]]"));
        assert!(content.contains("## Relationships"));
        assert!(content.contains("used_in -> [[saas-001]]"));
    }

    #[test]
    fn generate_with_superseded_facts_omitted() {
        let dir = tempfile::tempdir().unwrap();
        let gen = EntityProfileGenerator::new(dir.path());

        let entity = make_entity("fw-001", "Frontend Framework", EntityType::Concept);
        let mut superseded = make_memory("m1", "fw-001", "React selected", "2026-03-10T00:00:00Z");
        superseded.status = MemoryStatus::Superseded;
        superseded.superseded_by = Some("m2".to_string());

        let active = make_memory("m2", "fw-001", "Vue selected", "2026-03-14T00:00:00Z");

        let path = gen
            .generate(&entity, &[superseded, active], &[], &[])
            .unwrap();

        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("Vue selected"));
        assert!(!content.contains("React selected"));
        assert!(!content.contains("SUPERSEDED"));
        assert!(!content.contains("## Decision History"));
    }

    #[test]
    fn build_view_returns_typed_sections() {
        let mut entity = make_entity("vue-001", "Vue", EntityType::Tool);
        entity.attributes = serde_json::json!({
            "description": "A progressive UI framework",
        });

        let facts = vec![
            make_memory("m1", "vue-001", "Fast rendering", "2026-03-14T10:00:00Z"),
            make_memory("m2", "vue-001", "Nuxt-ready", "2026-03-12T08:00:00Z"),
        ];
        let rels = vec![
            make_relationship("r1", "vue-001", "nuxt-001", "used_with"),
            make_relationship("r2", "react-001", "vue-001", "compared_to"),
        ];

        let view = EntityProfileGenerator::build_view(
            &entity,
            &facts,
            &rels,
            &EntityProfileHistoryData::default(),
            &["Frontend Platform".to_string()],
        );

        assert_eq!(view.summary.as_deref(), Some("A progressive UI framework"));
        assert_eq!(view.facts.len(), 2);
        assert_eq!(view.facts[0].text, "Fast rendering");
        assert_eq!(view.facts[0].date.as_deref(), Some("2026-03-14"));
        assert_eq!(view.relationships.len(), 2);
        assert_eq!(view.relationships[0].rel_type, "used_with");
        assert_eq!(view.relationships[0].direction, "->");
        assert_eq!(view.relationships[1].direction, "<-");
        assert_eq!(view.referenced_in, vec!["Frontend Platform"]);
    }

    #[test]
    fn load_relationship_history_parses_removed_entries() {
        let dir = tempfile::tempdir().unwrap();
        let entity_dir = dir.path().join("ledger/tools/rust");
        fs::create_dir_all(entity_dir.as_path()).unwrap();
        fs::write(
            entity_dir.join("rust.md"),
            "\
# Rust

## Facts

## Relationships

## History
### Relationship Changes
- removed: used_with -> [[cargo]] [changed: 2026-04-06, reason: tooling retired]
",
        )
        .unwrap();

        let history = EntityProfileGenerator::load_relationship_history(dir.path(), "rust")
            .expect("relationship history");
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].action, "removed");
        assert_eq!(history[0].rel_type, "used_with");
        assert_eq!(history[0].target, "cargo");
        assert_eq!(history[0].changed_at.as_deref(), Some("2026-04-06"));
        assert_eq!(history[0].reason.as_deref(), Some("tooling retired"));
    }

    #[test]
    fn load_history_data_derives_commits_for_archived_facts_and_relationships() {
        let dir = tempfile::tempdir().unwrap();
        let repo_root = dir.path();
        let kb_root = repo_root.join("knowledge-base");
        let entity_dir = kb_root.join("ledger/tools/rust");
        fs::create_dir_all(&entity_dir).unwrap();
        let file_path = entity_dir.join("rust.md");

        fs::write(
            &file_path,
            "\
---
id: rust
type: tool
---

# Rust

## Facts
- Fast.

## Relationships

## History
",
        )
        .unwrap();

        let init = Command::new("git")
            .args(["init"])
            .current_dir(repo_root)
            .output()
            .unwrap();
        assert!(init.status.success());
        let email = Command::new("git")
            .args(["config", "user.email", "test@example.com"])
            .current_dir(repo_root)
            .output()
            .unwrap();
        assert!(email.status.success());
        let name = Command::new("git")
            .args(["config", "user.name", "Test User"])
            .current_dir(repo_root)
            .output()
            .unwrap();
        assert!(name.status.success());
        let add = Command::new("git")
            .args(["add", "knowledge-base/ledger/tools/rust/rust.md"])
            .current_dir(repo_root)
            .output()
            .unwrap();
        assert!(add.status.success());
        let first_commit = Command::new("git")
            .args(["commit", "-m", "seed"])
            .current_dir(repo_root)
            .output()
            .unwrap();
        assert!(first_commit.status.success());

        fs::write(
            &file_path,
            "\
---
id: rust
type: tool
---

# Rust

## Facts

## Relationships

## History
### Archived Facts
- ~~Fast.~~ [archived: 2026-04-07, confidence: 0.90]

### Relationship Changes
- removed: used_with -> [[cargo]] [changed: 2026-04-07, reason: retired]
",
        )
        .unwrap();
        let add = Command::new("git")
            .args(["add", "knowledge-base/ledger/tools/rust/rust.md"])
            .current_dir(repo_root)
            .output()
            .unwrap();
        assert!(add.status.success());
        let second_commit = Command::new("git")
            .args(["commit", "-m", "history"])
            .current_dir(repo_root)
            .output()
            .unwrap();
        assert!(second_commit.status.success());
        let hash_output = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(repo_root)
            .output()
            .unwrap();
        assert!(hash_output.status.success());
        let commit_hash = String::from_utf8_lossy(&hash_output.stdout)
            .trim()
            .to_string();

        let history = EntityProfileGenerator::load_history_data(kb_root.as_path(), "rust").unwrap();
        assert_eq!(history.archived_facts.len(), 1);
        assert_eq!(
            history.archived_facts[0].commit.as_deref(),
            Some(commit_hash.as_str())
        );
        assert_eq!(history.relationship_history.len(), 1);
        assert_eq!(
            history.relationship_history[0].commit.as_deref(),
            Some(commit_hash.as_str())
        );
    }

    #[test]
    fn generate_empty_entity() {
        let dir = tempfile::tempdir().unwrap();
        let gen = EntityProfileGenerator::new(dir.path());

        let entity = make_entity("p-001", "Alice", EntityType::Person);

        let path = gen.generate(&entity, &[], &[], &[]).unwrap();
        let content = fs::read_to_string(&path).unwrap();

        assert!(content.contains("# Alice"));
        assert!(content.contains("**Type:** Person"));
        // No sections for empty data
        assert!(!content.contains("## Facts"));
        assert!(!content.contains("## Referenced In"));
        assert!(!content.contains("## Relationships"));
    }

    #[test]
    fn profile_path_is_correct() {
        let gen = EntityProfileGenerator::new("/kb");
        let entity = make_entity("vue-001", "Vue", EntityType::Tool);
        assert_eq!(
            gen.profile_path(&entity),
            PathBuf::from("/kb/ledger/tools/vue/vue.brief.md")
        );
    }
}
