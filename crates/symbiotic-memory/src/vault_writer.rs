//! Agent write path for the Vault-as-Truth architecture.
//!
//! Provides surgical Markdown editing operations:
//! - Add facts to `## Facts`
//! - Archive facts into `## History` with visible archival metadata
//! - Add relationships to `## Relationships`
//! - Remove relationships into `## History`
//! - Create new entity files
//!
//! All operations preserve the file structure and update the `updated` timestamp.
//! See `docs/design/vault-as-truth.md` for the architecture.

use std::path::{Path, PathBuf};

use chrono::Utc;

use crate::vault_layout::{canonical_entity_relative_path, find_canonical_entity_file};
use crate::vault_linter::lint_content_at_path;
use crate::{EntityType, FactType, MemorySpace, Sensitivity};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Metadata for a new fact being added.
#[derive(Debug, Clone)]
pub struct NewFactMetadata {
    pub source: String,
    pub fact_type: Option<FactType>,
    pub confidence: Option<f64>,
}

/// Errors during vault write operations.
#[derive(Debug, thiserror::Error)]
pub enum VaultWriteError {
    #[error("entity file not found: {0}")]
    EntityNotFound(String),
    #[error("fact not found in ## Facts: {0}")]
    FactNotFound(String),
    #[error("relationship not found in ## Relationships: {0} -> {1}")]
    RelationshipNotFound(String, String),
    #[error("invalid relationship replacement: {0}")]
    InvalidRelationshipReplacement(String),
    #[error("file already exists: {0}")]
    FileAlreadyExists(String),
    #[error("malformed entity file (missing section: {0})")]
    MissingSection(String),
    #[error("lint failed: {0:?}")]
    LintFailed(Vec<String>),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

/// Describes a mutation performed by the writer, for commit message generation.
#[derive(Debug, Clone)]
pub struct VaultMutation {
    /// Path to the modified file (relative to vault root).
    pub file_path: String,
    /// Entity ID affected.
    pub entity_id: String,
    /// What changed.
    pub kind: MutationKind,
}

/// Kind of vault mutation.
#[derive(Debug, Clone)]
pub enum MutationKind {
    FactAdded(String),
    FactArchived {
        fact: String,
        reason: String,
    },
    RelationshipAdded {
        rel_type: String,
        target: String,
    },
    RelationshipRemoved {
        rel_type: String,
        target: String,
        reason: String,
    },
    RelationshipReplaced {
        rel_type: String,
        old_target: String,
        new_target: String,
        reason: String,
    },
    EntityCreated,
}

// ---------------------------------------------------------------------------
// VaultWriter
// ---------------------------------------------------------------------------

/// Surgically edits vault Markdown entity files.
pub struct VaultWriter {
    vault_root: PathBuf,
}

impl VaultWriter {
    pub fn new(vault_root: &Path) -> Self {
        Self {
            vault_root: vault_root.to_path_buf(),
        }
    }

    /// Add a fact to an entity's `## Facts` section.
    ///
    /// Returns the mutation descriptor for commit message generation.
    pub fn add_fact(
        &self,
        entity_id: &str,
        fact_text: &str,
        metadata: &NewFactMetadata,
    ) -> Result<VaultMutation, VaultWriteError> {
        let (file_path, content) = self.read_entity_file(entity_id)?;
        let rel_path = self.relative_path(&file_path);

        // Build the fact line
        let fact_line = format_fact_line(fact_text, metadata);

        // Insert into ## Facts section
        let updated_content = insert_into_section(&content, "Facts", &fact_line)?;

        // Update the `updated` timestamp in frontmatter
        let final_content = update_frontmatter_timestamp(&updated_content);

        self.validate_before_write(&file_path, &final_content)?;
        std::fs::write(&file_path, final_content)?;

        Ok(VaultMutation {
            file_path: rel_path,
            entity_id: entity_id.to_string(),
            kind: MutationKind::FactAdded(fact_text.to_string()),
        })
    }

    /// Archive a fact — move it from `## Facts` into
    /// `## History` -> `### Archived Facts`.
    ///
    /// The archival remains visible in Markdown and is also preserved in Git history.
    /// The `fact_text` must match an existing fact line (metadata-stripped text).
    pub fn archive_fact(
        &self,
        entity_id: &str,
        fact_text: &str,
        reason: &str,
    ) -> Result<VaultMutation, VaultWriteError> {
        let (file_path, content) = self.read_entity_file(entity_id)?;
        let rel_path = self.relative_path(&file_path);

        // Find and remove the fact from ## Facts
        let (updated_content, removed_line) = remove_fact_from_section(&content, fact_text)?;
        let archived_line = format_archived_fact_line(&removed_line, reason);
        let archived_content =
            insert_into_history_subsection(&updated_content, "Archived Facts", &archived_line);

        let final_content = update_frontmatter_timestamp(&archived_content);
        self.validate_before_write(&file_path, &final_content)?;
        std::fs::write(&file_path, final_content)?;

        Ok(VaultMutation {
            file_path: rel_path,
            entity_id: entity_id.to_string(),
            kind: MutationKind::FactArchived {
                fact: fact_text.to_string(),
                reason: reason.to_string(),
            },
        })
    }

    /// Replace an active fact atomically by archiving the old fact and adding
    /// the new fact in a single file write.
    pub fn replace_fact(
        &self,
        entity_id: &str,
        old_fact_text: &str,
        new_fact_text: &str,
        reason: &str,
        metadata: &NewFactMetadata,
    ) -> Result<Vec<VaultMutation>, VaultWriteError> {
        let (file_path, content) = self.read_entity_file(entity_id)?;
        let rel_path = self.relative_path(&file_path);

        let (updated_content, removed_line) = remove_fact_from_section(&content, old_fact_text)?;
        let archived_line = format_archived_fact_line(&removed_line, reason);
        let archived_content =
            insert_into_history_subsection(&updated_content, "Archived Facts", &archived_line);
        let fact_line = format_fact_line(new_fact_text, metadata);
        let replaced_content = insert_into_section(&archived_content, "Facts", &fact_line)?;
        let final_content = update_frontmatter_timestamp(&replaced_content);
        self.validate_before_write(&file_path, &final_content)?;
        std::fs::write(&file_path, final_content)?;

        Ok(vec![
            VaultMutation {
                file_path: rel_path.clone(),
                entity_id: entity_id.to_string(),
                kind: MutationKind::FactArchived {
                    fact: old_fact_text.to_string(),
                    reason: reason.to_string(),
                },
            },
            VaultMutation {
                file_path: rel_path,
                entity_id: entity_id.to_string(),
                kind: MutationKind::FactAdded(new_fact_text.to_string()),
            },
        ])
    }

    /// Add a relationship to an entity's `## Relationships` section.
    pub fn add_relationship(
        &self,
        entity_id: &str,
        rel_type: &str,
        target: &str,
        since: Option<&str>,
    ) -> Result<VaultMutation, VaultWriteError> {
        let (file_path, content) = self.read_entity_file(entity_id)?;
        let rel_path = self.relative_path(&file_path);

        let today = Utc::now().format("%Y-%m-%d").to_string();
        let since_val = since.unwrap_or(&today);

        let rel_line = format!("- {}: [[{}]] [since: {}]", rel_type, target, since_val);

        // Insert into ## Relationships section (create if missing)
        let updated_content =
            insert_into_section_or_create(&content, "Relationships", &rel_line, "Archived");

        let final_content = update_frontmatter_timestamp(&updated_content);

        self.validate_before_write(&file_path, &final_content)?;
        std::fs::write(&file_path, final_content)?;

        Ok(VaultMutation {
            file_path: rel_path,
            entity_id: entity_id.to_string(),
            kind: MutationKind::RelationshipAdded {
                rel_type: rel_type.to_string(),
                target: target.to_string(),
            },
        })
    }

    /// Remove an outgoing relationship from an entity's `## Relationships`
    /// section and record the semantic change in `## History`.
    pub fn remove_relationship(
        &self,
        entity_id: &str,
        rel_type: &str,
        target: &str,
        reason: &str,
    ) -> Result<VaultMutation, VaultWriteError> {
        let (file_path, content) = self.read_entity_file(entity_id)?;
        let rel_path = self.relative_path(&file_path);

        let updated_content = remove_relationship_from_section(&content, rel_type, target)?.content;
        let history_line = format_relationship_history_line(rel_type, target, reason);
        let history_content =
            insert_into_history_subsection(&updated_content, "Relationship Changes", &history_line);
        let final_content = update_frontmatter_timestamp(&history_content);

        self.validate_before_write(&file_path, &final_content)?;
        std::fs::write(&file_path, final_content)?;

        Ok(VaultMutation {
            file_path: rel_path,
            entity_id: entity_id.to_string(),
            kind: MutationKind::RelationshipRemoved {
                rel_type: rel_type.to_string(),
                target: target.to_string(),
                reason: reason.to_string(),
            },
        })
    }

    /// Replace an outgoing relationship target while keeping an explicit
    /// semantic history record in `## History`.
    pub fn replace_relationship(
        &self,
        entity_id: &str,
        rel_type: &str,
        old_target: &str,
        new_target: &str,
        reason: &str,
        since: Option<&str>,
    ) -> Result<VaultMutation, VaultWriteError> {
        let trimmed_old_target = old_target.trim();
        let trimmed_new_target = new_target.trim();
        if trimmed_old_target.is_empty() || trimmed_new_target.is_empty() {
            return Err(VaultWriteError::InvalidRelationshipReplacement(
                "old and new targets are required".to_string(),
            ));
        }
        if trimmed_old_target == trimmed_new_target {
            return Err(VaultWriteError::InvalidRelationshipReplacement(
                "old and new targets must differ".to_string(),
            ));
        }

        let (file_path, content) = self.read_entity_file(entity_id)?;
        let rel_path = self.relative_path(&file_path);
        let removed = remove_relationship_from_section(&content, rel_type, old_target)?;
        let replacement_line = rebuild_relationship_line(
            rel_type,
            &[trimmed_new_target.to_string()],
            relationship_replacement_metadata(removed.removed_trailing_metadata.as_deref(), since)
                .as_deref(),
        );
        let replaced_content =
            insert_into_section(&removed.content, "Relationships", &replacement_line)?;
        let history_line =
            format_relationship_replaced_history_line(rel_type, old_target, new_target, reason);
        let history_content = insert_into_history_subsection(
            &replaced_content,
            "Relationship Changes",
            &history_line,
        );
        let final_content = update_frontmatter_timestamp(&history_content);

        self.validate_before_write(&file_path, &final_content)?;
        std::fs::write(&file_path, final_content)?;

        Ok(VaultMutation {
            file_path: rel_path,
            entity_id: entity_id.to_string(),
            kind: MutationKind::RelationshipReplaced {
                rel_type: rel_type.trim().to_string(),
                old_target: trimmed_old_target.to_string(),
                new_target: trimmed_new_target.to_string(),
                reason: reason.trim().to_string(),
            },
        })
    }

    /// Create a new entity Markdown file.
    ///
    /// The file is created in the appropriate subdirectory based on `space`.
    pub fn create_entity(
        &self,
        id: &str,
        name: &str,
        entity_type: EntityType,
        space: MemorySpace,
        sensitivity: Option<Sensitivity>,
    ) -> Result<VaultMutation, VaultWriteError> {
        let rel_path = canonical_entity_relative_path(id, entity_type, space);
        let file_path = self.vault_root.join(&rel_path);
        let dir = file_path
            .parent()
            .ok_or_else(|| VaultWriteError::EntityNotFound(id.to_string()))?;
        std::fs::create_dir_all(dir)?;
        if file_path.exists() {
            return Err(VaultWriteError::FileAlreadyExists(
                file_path.to_string_lossy().to_string(),
            ));
        }

        let now = Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
        let sens = sensitivity.unwrap_or(Sensitivity::Private);

        let content = format!(
            "---\n\
             id: {}\n\
             type: {}\n\
             space: {}\n\
             sensitivity: {}\n\
             created: {}\n\
             updated: {}\n\
             ---\n\
             \n\
             # {}\n\
             \n\
             ## Facts\n\
             \n\
             ## Relationships\n\
             \n\
             ## History\n",
            id,
            entity_type.frontmatter_str(),
            space.as_str(),
            sens.as_str(),
            now,
            now,
            name,
        );

        self.validate_before_write(&file_path, &content)?;
        std::fs::write(&file_path, content)?;

        Ok(VaultMutation {
            file_path: rel_path.to_string_lossy().to_string(),
            entity_id: id.to_string(),
            kind: MutationKind::EntityCreated,
        })
    }

    // --- Internal helpers ---

    /// Find and read an entity file by ID across all vault subdirectories.
    fn read_entity_file(&self, entity_id: &str) -> Result<(PathBuf, String), VaultWriteError> {
        if let Some(path) = find_canonical_entity_file(&self.vault_root, entity_id)? {
            let content = std::fs::read_to_string(&path)?;
            return Ok((path, content));
        }
        Err(VaultWriteError::EntityNotFound(entity_id.to_string()))
    }

    /// Compute relative path from vault root.
    fn relative_path(&self, path: &Path) -> String {
        path.strip_prefix(&self.vault_root)
            .unwrap_or(path)
            .to_string_lossy()
            .to_string()
    }

    fn validate_before_write(
        &self,
        file_path: &Path,
        content: &str,
    ) -> Result<(), VaultWriteError> {
        let report = lint_content_at_path(content, Some(file_path));
        if report.is_valid() {
            Ok(())
        } else {
            Err(VaultWriteError::LintFailed(report.errors))
        }
    }
}

// ---------------------------------------------------------------------------
// Text manipulation helpers
// ---------------------------------------------------------------------------

/// Format a fact line with inline metadata.
fn format_fact_line(fact_text: &str, metadata: &NewFactMetadata) -> String {
    let mut line = format!("- {}", fact_text);

    line.push_str(&format!(" [source: {}]", metadata.source));

    if let Some(ft) = &metadata.fact_type {
        line.push_str(&format!(" [type: {}]", ft));
    }

    if let Some(conf) = metadata.confidence {
        line.push_str(&format!(" [confidence: {}]", conf));
    }

    line
}

fn format_archived_fact_line(original_line: &str, reason: &str) -> String {
    let trimmed = original_line.trim();
    let body = trimmed.strip_prefix("- ").unwrap_or(trimmed);
    let metadata_start = body.find(" [");
    let (fact_text, existing_metadata) = if let Some(index) = metadata_start {
        (&body[..index], &body[index..])
    } else {
        (body, "")
    };
    let archived_at = Utc::now().format("%Y-%m-%d").to_string();
    format!(
        "- ~~{}~~{} [archived: {}, reason: {}]",
        fact_text.trim(),
        existing_metadata,
        archived_at,
        reason.trim()
    )
}

fn format_relationship_history_line(rel_type: &str, target: &str, reason: &str) -> String {
    let changed_at = Utc::now().format("%Y-%m-%d").to_string();
    format!(
        "- removed: {} -> [[{}]] [changed: {}, reason: {}]",
        rel_type.trim(),
        target.trim(),
        changed_at,
        reason.trim()
    )
}

fn format_relationship_replaced_history_line(
    rel_type: &str,
    old_target: &str,
    new_target: &str,
    reason: &str,
) -> String {
    let changed_at = Utc::now().format("%Y-%m-%d").to_string();
    format!(
        "- replaced: {} -> [[{}]] => [[{}]] [changed: {}, reason: {}]",
        rel_type.trim(),
        old_target.trim(),
        new_target.trim(),
        changed_at,
        reason.trim()
    )
}

/// Insert a line at the end of a `## Section` in the Markdown content.
fn insert_into_section(
    content: &str,
    section_name: &str,
    line: &str,
) -> Result<String, VaultWriteError> {
    let header = format!("## {}", section_name);
    let lines: Vec<&str> = content.lines().collect();

    // Find the section header
    let section_idx = lines
        .iter()
        .position(|l| l.trim() == header)
        .ok_or_else(|| VaultWriteError::MissingSection(section_name.to_string()))?;

    // Find the end of this section (next ## or end of file)
    let section_end = lines
        .iter()
        .enumerate()
        .skip(section_idx + 1)
        .find(|(_, l)| l.trim().starts_with("## "))
        .map(|(i, _)| i)
        .unwrap_or(lines.len());

    // Find last non-empty line in section to insert after
    let insert_pos = lines[section_idx + 1..section_end]
        .iter()
        .enumerate()
        .rev()
        .find(|(_, l)| !l.trim().is_empty())
        .map(|(i, _)| section_idx + 1 + i + 1)
        .unwrap_or(section_idx + 1);

    let mut output = String::new();
    for (i, l) in lines.iter().enumerate() {
        if i == insert_pos {
            output.push_str(line);
            output.push('\n');
        }
        output.push_str(l);
        output.push('\n');
    }

    // If insert_pos is at the end of file
    if insert_pos >= lines.len() {
        output.push_str(line);
        output.push('\n');
    }

    Ok(output)
}

/// Insert a line into a section, creating the section if it doesn't exist.
///
/// `after_section` is the section name after which to create the new section.
fn insert_into_section_or_create(
    content: &str,
    section_name: &str,
    line: &str,
    after_section: &str,
) -> String {
    let header = format!("## {}", section_name);

    // Check if section exists
    if content.lines().any(|l| l.trim() == header) {
        // Section exists — insert into it
        return insert_into_section(content, section_name, line)
            .unwrap_or_else(|_| content.to_string());
    }

    // Section doesn't exist — create it after `after_section`
    let after_header = format!("## {}", after_section);
    let lines: Vec<&str> = content.lines().collect();

    // Find the section after which to insert
    let after_idx = lines.iter().position(|l| l.trim() == after_header);

    if let Some(idx) = after_idx {
        // Find end of that section
        let section_end = lines
            .iter()
            .enumerate()
            .skip(idx + 1)
            .find(|(_, l)| l.trim().starts_with("## "))
            .map(|(i, _)| i)
            .unwrap_or(lines.len());

        let mut output = String::new();
        for (i, l) in lines.iter().enumerate() {
            output.push_str(l);
            output.push('\n');
            if i == section_end.saturating_sub(1)
                || (section_end == lines.len() && i == lines.len() - 1)
            {
                output.push('\n');
                output.push_str(&header);
                output.push('\n');
                output.push_str(line);
                output.push('\n');
            }
        }
        output
    } else {
        // Can't find reference section — append at end
        let mut output = content.to_string();
        if !output.ends_with('\n') {
            output.push('\n');
        }
        output.push('\n');
        output.push_str(&header);
        output.push('\n');
        output.push_str(line);
        output.push('\n');
        output
    }
}

/// Remove a fact line from `## Facts` by matching the stripped text.
///
/// Returns (updated content, removed full line).
fn remove_fact_from_section(
    content: &str,
    fact_text: &str,
) -> Result<(String, String), VaultWriteError> {
    let lines: Vec<&str> = content.lines().collect();
    let facts_header = "## Facts";

    // Find ## Facts section
    let section_start = lines
        .iter()
        .position(|l| l.trim() == facts_header)
        .ok_or_else(|| VaultWriteError::MissingSection("Facts".to_string()))?;

    let section_end = lines
        .iter()
        .enumerate()
        .skip(section_start + 1)
        .find(|(_, l)| l.trim().starts_with("## "))
        .map(|(i, _)| i)
        .unwrap_or(lines.len());

    // Find the fact line by matching stripped text
    let fact_idx = lines[section_start + 1..section_end]
        .iter()
        .enumerate()
        .find(|(_, l)| {
            let trimmed = l.trim();
            if let Some(rest) = trimmed.strip_prefix("- ") {
                strip_metadata_inline(rest) == fact_text
            } else {
                false
            }
        })
        .map(|(i, _)| section_start + 1 + i);

    let fact_idx = fact_idx.ok_or_else(|| VaultWriteError::FactNotFound(fact_text.to_string()))?;

    let removed_line = lines[fact_idx].to_string();

    // Build content without the removed line
    let mut output = String::new();
    for (i, l) in lines.iter().enumerate() {
        if i != fact_idx {
            output.push_str(l);
            output.push('\n');
        }
    }

    Ok((output, removed_line))
}

#[derive(Debug)]
struct RelationshipSectionUpdate {
    content: String,
    removed_trailing_metadata: Option<String>,
}

fn remove_relationship_from_section(
    content: &str,
    rel_type: &str,
    target: &str,
) -> Result<RelationshipSectionUpdate, VaultWriteError> {
    let lines: Vec<&str> = content.lines().collect();
    let relationships_header = "## Relationships";
    let rel_type = rel_type.trim();
    let target = target.trim();

    let section_start = lines
        .iter()
        .position(|l| l.trim() == relationships_header)
        .ok_or_else(|| VaultWriteError::MissingSection("Relationships".to_string()))?;

    let section_end = lines
        .iter()
        .enumerate()
        .skip(section_start + 1)
        .find(|(_, l)| l.trim().starts_with("## "))
        .map(|(i, _)| i)
        .unwrap_or(lines.len());

    let mut output_lines = lines
        .iter()
        .map(|line| (*line).to_string())
        .collect::<Vec<_>>();
    let mut matched = false;
    let mut removed_trailing_metadata = None;

    for idx in (section_start + 1)..section_end {
        let line = lines[idx].trim();
        let Some(parsed) = parse_relationship_line(line) else {
            continue;
        };
        if parsed.rel_type != rel_type || !parsed.targets.iter().any(|value| value == target) {
            continue;
        }

        matched = true;
        removed_trailing_metadata = parsed.trailing_metadata.clone();
        let remaining_targets = parsed
            .targets
            .into_iter()
            .filter(|value| value != target)
            .collect::<Vec<_>>();

        if remaining_targets.is_empty() {
            output_lines.remove(idx);
        } else {
            output_lines[idx] = rebuild_relationship_line(
                &parsed.rel_type,
                &remaining_targets,
                parsed.trailing_metadata.as_deref(),
            );
        }
        break;
    }

    if !matched {
        return Err(VaultWriteError::RelationshipNotFound(
            rel_type.to_string(),
            target.to_string(),
        ));
    }

    Ok(RelationshipSectionUpdate {
        content: output_lines.join("\n") + "\n",
        removed_trailing_metadata,
    })
}

/// Strip `[key: value]` metadata from inline text.
fn strip_metadata_inline(line: &str) -> String {
    let re = &*crate::vault_parser::METADATA_RE;
    re.replace_all(line, "").trim().to_string()
}

#[derive(Debug)]
struct ParsedRelationshipLine {
    rel_type: String,
    targets: Vec<String>,
    trailing_metadata: Option<String>,
}

fn parse_relationship_line(line: &str) -> Option<ParsedRelationshipLine> {
    let rest = line.strip_prefix("- ")?;
    let colon_idx = rest.find(':')?;
    let rel_type = rest[..colon_idx].trim().to_string();
    let value = rest[colon_idx + 1..].trim();
    let wikilink_re = regex::Regex::new(r"\[\[([^\]]+)\]\]").ok()?;
    let targets = wikilink_re
        .captures_iter(value)
        .filter_map(|captures| captures.get(1).map(|m| m.as_str().trim().to_string()))
        .collect::<Vec<_>>();
    if targets.is_empty() {
        return None;
    }
    let trailing_metadata = wikilink_re.find_iter(value).last().and_then(|match_| {
        let tail = value[match_.end()..].trim();
        if tail.is_empty() {
            None
        } else {
            Some(tail.to_string())
        }
    });
    Some(ParsedRelationshipLine {
        rel_type,
        targets,
        trailing_metadata,
    })
}

fn rebuild_relationship_line(
    rel_type: &str,
    targets: &[String],
    trailing_metadata: Option<&str>,
) -> String {
    let mut rebuilt = format!(
        "- {}: {}",
        rel_type.trim(),
        targets
            .iter()
            .map(|target| format!("[[{target}]]"))
            .collect::<Vec<_>>()
            .join(", ")
    );
    if let Some(metadata) = trailing_metadata.filter(|value| !value.trim().is_empty()) {
        rebuilt.push(' ');
        rebuilt.push_str(metadata.trim());
    }
    rebuilt
}

fn relationship_replacement_metadata(
    existing_metadata: Option<&str>,
    since: Option<&str>,
) -> Option<String> {
    if let Some(value) = since.map(str::trim).filter(|value| !value.is_empty()) {
        return Some(format!("[since: {value}]"));
    }
    existing_metadata.map(str::trim).and_then(|value| {
        if value.is_empty() {
            None
        } else {
            Some(value.to_string())
        }
    })
}

fn insert_into_history_subsection(content: &str, subsection_name: &str, line: &str) -> String {
    let lines: Vec<&str> = content.lines().collect();
    let history_header = "## History";
    let subsection_header = format!("### {}", subsection_name);

    let Some(history_start) = lines.iter().position(|l| l.trim() == history_header) else {
        let mut output = content.to_string();
        if !output.ends_with('\n') {
            output.push('\n');
        }
        output.push('\n');
        output.push_str(history_header);
        output.push('\n');
        output.push_str(&subsection_header);
        output.push('\n');
        output.push_str(line);
        output.push('\n');
        return output;
    };

    let history_end = lines
        .iter()
        .enumerate()
        .skip(history_start + 1)
        .find(|(_, l)| l.trim().starts_with("## "))
        .map(|(i, _)| i)
        .unwrap_or(lines.len());

    if let Some(subsection_start) = lines[history_start + 1..history_end]
        .iter()
        .position(|l| l.trim() == subsection_header)
        .map(|offset| history_start + 1 + offset)
    {
        let subsection_end = lines
            .iter()
            .enumerate()
            .skip(subsection_start + 1)
            .find(|(idx, l)| {
                *idx >= history_end || l.trim().starts_with("### ") || l.trim().starts_with("## ")
            })
            .map(|(i, _)| i)
            .unwrap_or(history_end);

        let insert_pos = lines[subsection_start + 1..subsection_end]
            .iter()
            .enumerate()
            .rev()
            .find(|(_, l)| !l.trim().is_empty())
            .map(|(offset, _)| subsection_start + 1 + offset + 1)
            .unwrap_or(subsection_start + 1);

        let mut output = String::new();
        for (idx, current) in lines.iter().enumerate() {
            if idx == insert_pos {
                output.push_str(line);
                output.push('\n');
            }
            output.push_str(current);
            output.push('\n');
        }
        if insert_pos >= lines.len() {
            output.push_str(line);
            output.push('\n');
        }
        return output;
    }

    let mut output = String::new();
    for (idx, current) in lines.iter().enumerate() {
        output.push_str(current);
        output.push('\n');
        if idx == history_end.saturating_sub(1) {
            output.push('\n');
            output.push_str(&subsection_header);
            output.push('\n');
            output.push_str(line);
            output.push('\n');
        }
    }
    output
}

/// Update the `updated:` field in YAML frontmatter to now.
fn update_frontmatter_timestamp(content: &str) -> String {
    let now = Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let mut output = String::new();
    let mut in_frontmatter = false;
    let mut frontmatter_seen = false;

    for line in content.lines() {
        if line.trim() == "---" {
            if !frontmatter_seen {
                in_frontmatter = true;
                frontmatter_seen = true;
            } else {
                in_frontmatter = false;
            }
            output.push_str(line);
            output.push('\n');
            continue;
        }

        if in_frontmatter && line.trim().starts_with("updated:") {
            output.push_str(&format!("updated: {}", now));
            output.push('\n');
        } else {
            output.push_str(line);
            output.push('\n');
        }
    }

    output
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    const RUST_ENTITY: &str = "\
---
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

## History
";

    fn setup_vault() -> (TempDir, VaultWriter) {
        let vault = TempDir::new().unwrap();
        let dir = vault.path().join("ledger/tools/rust");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("rust.md"), RUST_ENTITY).unwrap();
        let writer = VaultWriter::new(vault.path());
        (vault, writer)
    }

    #[test]
    fn add_fact_appends_to_facts_section() {
        let (_vault, writer) = setup_vault();

        let metadata = NewFactMetadata {
            source: "chat-2026-03-23".to_string(),
            fact_type: Some(FactType::Decision),
            confidence: Some(0.9),
        };

        let mutation = writer
            .add_fact("rust", "Best language for systems", &metadata)
            .unwrap();

        assert_eq!(mutation.entity_id, "rust");
        assert!(matches!(mutation.kind, MutationKind::FactAdded(_)));

        // Read back the file
        let content =
            std::fs::read_to_string(_vault.path().join("ledger/tools/rust/rust.md")).unwrap();

        assert!(content.contains("- Best language for systems [source: chat-2026-03-23] [type: decision] [confidence: 0.9]"));
        // Original facts still present
        assert!(content.contains("Systems programming language"));
        assert!(content.contains("Memory safe without GC"));
    }

    #[test]
    fn add_fact_updates_timestamp() {
        let (_vault, writer) = setup_vault();

        let metadata = NewFactMetadata {
            source: "manual".to_string(),
            fact_type: None,
            confidence: None,
        };

        writer.add_fact("rust", "New fact", &metadata).unwrap();

        let content =
            std::fs::read_to_string(_vault.path().join("ledger/tools/rust/rust.md")).unwrap();

        // Timestamp should no longer be the original
        assert!(!content.contains("updated: 2026-01-01T00:00:00Z"));
        assert!(content.contains("updated: 20")); // starts with current year
    }

    #[test]
    fn archive_fact_moves_fact_to_history_section() {
        let (_vault, writer) = setup_vault();

        let mutation = writer
            .archive_fact("rust", "Memory safe without GC", "outdated claim")
            .unwrap();

        assert!(matches!(mutation.kind, MutationKind::FactArchived { .. }));

        let content =
            std::fs::read_to_string(_vault.path().join("ledger/tools/rust/rust.md")).unwrap();

        // Fact should be removed from ## Facts
        let facts_section = extract_section(&content, "Facts");
        assert!(!facts_section.contains("Memory safe without GC"));

        let history_section = extract_section(&content, "History");
        assert!(history_section.contains("### Archived Facts"));
        assert!(history_section.contains("~~Memory safe without GC~~"));
        assert!(history_section.contains("[archived: "));
        assert!(history_section.contains("reason: outdated claim"));
    }

    #[test]
    fn replace_fact_archives_old_and_adds_new_atomically() {
        let (_vault, writer) = setup_vault();
        let metadata = NewFactMetadata {
            source: "manual".to_string(),
            fact_type: Some(FactType::Finding),
            confidence: Some(0.91),
        };

        let mutations = writer
            .replace_fact(
                "rust",
                "Memory safe without GC",
                "Ownership enforces memory safety without GC",
                "clarified wording",
                &metadata,
            )
            .unwrap();

        assert_eq!(mutations.len(), 2);
        assert!(matches!(
            mutations[0].kind,
            MutationKind::FactArchived { .. }
        ));
        assert!(matches!(mutations[1].kind, MutationKind::FactAdded(_)));

        let content =
            std::fs::read_to_string(_vault.path().join("ledger/tools/rust/rust.md")).unwrap();

        let facts_section = extract_section(&content, "Facts");
        assert!(!facts_section.contains("Memory safe without GC [source: article-001]"));
        assert!(facts_section.contains("Ownership enforces memory safety without GC"));

        let history_section = extract_section(&content, "History");
        assert!(history_section.contains("### Archived Facts"));
        assert!(history_section.contains("~~Memory safe without GC~~"));
        assert!(history_section.contains("reason: clarified wording"));
    }

    #[test]
    fn archive_nonexistent_fact_errors() {
        let (_vault, writer) = setup_vault();

        let result = writer.archive_fact("rust", "No such fact", "reason");
        assert!(matches!(result, Err(VaultWriteError::FactNotFound(_))));
    }

    #[test]
    fn add_relationship_appends_to_relationships() {
        let (_vault, writer) = setup_vault();

        let mutation = writer
            .add_relationship("rust", "competes_with", "go", Some("2026-03-23"))
            .unwrap();

        assert!(matches!(
            mutation.kind,
            MutationKind::RelationshipAdded { .. }
        ));

        let content =
            std::fs::read_to_string(_vault.path().join("ledger/tools/rust/rust.md")).unwrap();

        assert!(content.contains("- competes_with: [[go]] [since: 2026-03-23]"));
        // Original relationship still present
        assert!(content.contains("used_with: [[cargo]]"));
    }

    #[test]
    fn remove_relationship_moves_entry_to_history_section() {
        let (_vault, writer) = setup_vault();

        let mutation = writer
            .remove_relationship("rust", "used_with", "cargo", "tooling retired")
            .unwrap();

        assert!(matches!(
            mutation.kind,
            MutationKind::RelationshipRemoved { .. }
        ));

        let content =
            std::fs::read_to_string(_vault.path().join("ledger/tools/rust/rust.md")).unwrap();

        let relationships_section = extract_section(&content, "Relationships");
        assert!(!relationships_section.contains("used_with: [[cargo]]"));

        let history_section = extract_section(&content, "History");
        assert!(history_section.contains("### Relationship Changes"));
        assert!(history_section.contains("removed: used_with -> [[cargo]]"));
        assert!(history_section.contains("reason: tooling retired"));
    }

    #[test]
    fn replace_relationship_updates_active_link_and_records_history() {
        let (_vault, writer) = setup_vault();

        let mutation = writer
            .replace_relationship(
                "rust",
                "used_with",
                "cargo",
                "rust-analyzer",
                "editor workflow changed",
                Some("2026-04-07"),
            )
            .unwrap();

        assert!(matches!(
            mutation.kind,
            MutationKind::RelationshipReplaced { .. }
        ));

        let content =
            std::fs::read_to_string(_vault.path().join("ledger/tools/rust/rust.md")).unwrap();

        let relationships_section = extract_section(&content, "Relationships");
        assert!(!relationships_section.contains("used_with: [[cargo]]"));
        assert!(relationships_section.contains("used_with: [[rust-analyzer]] [since: 2026-04-07]"));

        let history_section = extract_section(&content, "History");
        assert!(history_section.contains("### Relationship Changes"));
        assert!(history_section.contains("replaced: used_with -> [[cargo]] => [[rust-analyzer]]"));
        assert!(history_section.contains("reason: editor workflow changed"));
    }

    #[test]
    fn create_entity_writes_new_file() {
        let (vault, writer) = setup_vault();

        let mutation = writer
            .create_entity("go", "Go", EntityType::Tool, MemorySpace::Knowledge, None)
            .unwrap();

        assert!(matches!(mutation.kind, MutationKind::EntityCreated));
        assert_eq!(mutation.file_path, "ledger/tools/go/go.md");

        let content = std::fs::read_to_string(vault.path().join("ledger/tools/go/go.md")).unwrap();

        assert!(content.contains("id: go"));
        assert!(content.contains("type: tool"));
        assert!(content.contains("# Go"));
        assert!(content.contains("## Facts"));
        assert!(content.contains("## Relationships"));
        assert!(content.contains("## History"));
    }

    #[test]
    fn create_entity_in_identity_space() {
        let (vault, writer) = setup_vault();

        writer
            .create_entity(
                "prefs",
                "My Preferences",
                EntityType::Preference,
                MemorySpace::Identity,
                Some(Sensitivity::Private),
            )
            .unwrap();

        let path = vault.path().join("identity/prefs.md");
        assert!(path.exists());

        let content = std::fs::read_to_string(path).unwrap();
        assert!(content.contains("space: identity"));
        assert!(content.contains("sensitivity: private"));
    }

    #[test]
    fn create_duplicate_entity_errors() {
        let (_vault, writer) = setup_vault();

        let result = writer.create_entity(
            "rust",
            "Rust",
            EntityType::Tool,
            MemorySpace::Knowledge,
            None,
        );
        assert!(matches!(result, Err(VaultWriteError::FileAlreadyExists(_))));
    }

    #[test]
    fn entity_not_found_errors() {
        let (_vault, writer) = setup_vault();

        let metadata = NewFactMetadata {
            source: "manual".to_string(),
            fact_type: None,
            confidence: None,
        };

        let result = writer.add_fact("nonexistent", "fact", &metadata);
        assert!(matches!(result, Err(VaultWriteError::EntityNotFound(_))));
    }

    #[test]
    fn format_fact_line_with_all_metadata() {
        let meta = NewFactMetadata {
            source: "chat-123".to_string(),
            fact_type: Some(FactType::Decision),
            confidence: Some(0.85),
        };
        let line = format_fact_line("Chose Nomad over K8s", &meta);
        assert_eq!(
            line,
            "- Chose Nomad over K8s [source: chat-123] [type: decision] [confidence: 0.85]"
        );
    }

    #[test]
    fn format_fact_line_minimal_metadata() {
        let meta = NewFactMetadata {
            source: "manual".to_string(),
            fact_type: None,
            confidence: None,
        };
        let line = format_fact_line("Simple fact", &meta);
        assert_eq!(line, "- Simple fact [source: manual]");
    }

    /// Helper: extract a section's content by name.
    fn extract_section(content: &str, section_name: &str) -> String {
        let header = format!("## {}", section_name);
        let lines: Vec<&str> = content.lines().collect();

        let start = lines
            .iter()
            .position(|l| l.trim() == header)
            .unwrap_or(lines.len());

        let end = lines
            .iter()
            .enumerate()
            .skip(start + 1)
            .find(|(_, l)| l.trim().starts_with("## "))
            .map(|(i, _)| i)
            .unwrap_or(lines.len());

        lines[start..end].join("\n")
    }
}
