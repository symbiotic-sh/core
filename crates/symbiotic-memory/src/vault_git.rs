//! Git commit integration for the Vault-as-Truth architecture.
//!
//! Creates semantic git commits after vault mutations. Commit messages
//! follow the format: `memory(entity-name): description [source: id]`
//!
//! See `docs/design/vault-as-truth.md` for the architecture.

use std::path::Path;
use std::process::Command;

use crate::vault_writer::{MutationKind, VaultMutation};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Errors during git commit operations.
#[derive(Debug, thiserror::Error)]
pub enum VaultGitError {
    #[error("git command failed: {0}")]
    GitFailed(String),
    #[error("no mutations to commit")]
    NoMutations,
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

/// Result of a vault commit operation.
#[derive(Debug, Clone)]
pub struct VaultCommitResult {
    /// The full commit message used.
    pub message: String,
    /// Number of files staged.
    pub files_staged: usize,
    /// Git commit hash (if available).
    pub commit_hash: Option<String>,
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Commit vault mutations to git with a semantic commit message.
///
/// `vault_root` should point to the `knowledge-base/` directory (or its parent
/// git repo root).
///
/// `source` is an optional source identifier (e.g., "chat-2026-03-23")
/// included in the commit message.
pub fn commit_mutations(
    repo_root: &Path,
    mutations: &[VaultMutation],
    source: Option<&str>,
) -> Result<VaultCommitResult, VaultGitError> {
    if mutations.is_empty() {
        return Err(VaultGitError::NoMutations);
    }

    // Stage the affected files
    let mut files_staged = 0;
    for mutation in mutations {
        git_add(repo_root, &mutation.file_path)?;
        files_staged += 1;
    }

    // Build the commit message
    let message = build_commit_message(mutations, source);

    // Create the commit
    let commit_hash = git_commit(repo_root, &message)?;

    Ok(VaultCommitResult {
        message,
        files_staged,
        commit_hash,
    })
}

/// Build a semantic commit message from vault mutations.
///
/// This is public so callers can preview the message before committing.
pub fn build_commit_message(mutations: &[VaultMutation], source: Option<&str>) -> String {
    if mutations.is_empty() {
        return String::new();
    }

    // Group mutations by entity
    let mut by_entity: std::collections::BTreeMap<&str, Vec<&VaultMutation>> =
        std::collections::BTreeMap::new();
    for m in mutations {
        by_entity.entry(&m.entity_id).or_default().push(m);
    }

    // Single entity → simple format
    if by_entity.len() == 1 {
        let (entity_id, muts) = by_entity.into_iter().next().unwrap();
        let summary = summarize_mutations(muts);
        let source_suffix = source
            .map(|s| format!(" [source: {}]", s))
            .unwrap_or_default();

        return format!(
            "memory({}): {}{}\n\nCo-Authored-By: Symbiotic AI <noreply@symbiotic.sh>",
            entity_id, summary, source_suffix
        );
    }

    // Multiple entities → list format
    let entity_names: Vec<&str> = by_entity.keys().copied().collect();
    let source_suffix = source
        .map(|s| format!(" [source: {}]", s))
        .unwrap_or_default();

    let mut msg = format!(
        "memory: update {} entities{}\n\n",
        entity_names.len(),
        source_suffix
    );

    for (entity_id, muts) in &by_entity {
        let summary = summarize_mutations(muts.clone());
        msg.push_str(&format!("- {}: {}\n", entity_id, summary));
    }

    msg.push_str("\nCo-Authored-By: Symbiotic AI <noreply@symbiotic.sh>");
    msg
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Summarize a set of mutations for a single entity into a brief description.
fn summarize_mutations(mutations: Vec<&VaultMutation>) -> String {
    let mut added = 0;
    let mut archived = 0;
    let mut rels = 0;
    let mut created = false;

    for m in &mutations {
        match &m.kind {
            MutationKind::FactAdded(_) => added += 1,
            MutationKind::FactArchived { .. } => archived += 1,
            MutationKind::RelationshipAdded { .. } => rels += 1,
            MutationKind::RelationshipRemoved { .. } => rels += 1,
            MutationKind::RelationshipReplaced { .. } => rels += 1,
            MutationKind::EntityCreated => created = true,
        }
    }

    let mut parts = Vec::new();
    if created {
        parts.push("created".to_string());
    }
    if added > 0 {
        parts.push(format!(
            "added {} fact{}",
            added,
            if added == 1 { "" } else { "s" }
        ));
    }
    if archived > 0 {
        parts.push(format!(
            "archived {} fact{}",
            archived,
            if archived == 1 { "" } else { "s" }
        ));
    }
    if rels > 0 {
        parts.push(format!(
            "updated {} relationship{}",
            rels,
            if rels == 1 { "" } else { "s" }
        ));
    }

    parts.join(", ")
}

/// Stage a file in git.
fn git_add(repo_root: &Path, file_path: &str) -> Result<(), VaultGitError> {
    let output = Command::new("git")
        .args(["add", file_path])
        .current_dir(repo_root)
        .output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(VaultGitError::GitFailed(format!(
            "git add {} failed: {}",
            file_path, stderr
        )));
    }

    Ok(())
}

/// Create a git commit with the given message.
fn git_commit(repo_root: &Path, message: &str) -> Result<Option<String>, VaultGitError> {
    let output = Command::new("git")
        .args(["commit", "-m", message])
        .current_dir(repo_root)
        .output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // "nothing to commit" is not an error
        if stderr.contains("nothing to commit") {
            return Ok(None);
        }
        return Err(VaultGitError::GitFailed(format!(
            "git commit failed: {}",
            stderr
        )));
    }

    // Extract commit hash from output
    let stdout = String::from_utf8_lossy(&output.stdout);
    let hash = stdout
        .split_whitespace()
        .find(|w| w.len() >= 7 && w.chars().all(|c| c.is_ascii_hexdigit() || c == ']'))
        .map(|w| w.trim_matches(|c: char| !c.is_ascii_hexdigit()).to_string());

    Ok(hash)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn canonical_path(entity_id: &str) -> String {
        format!("ledger/concepts/{entity_id}/{entity_id}.md")
    }

    fn fact_mutation(entity_id: &str, fact: &str) -> VaultMutation {
        VaultMutation {
            file_path: canonical_path(entity_id),
            entity_id: entity_id.to_string(),
            kind: MutationKind::FactAdded(fact.to_string()),
        }
    }

    fn archive_mutation(entity_id: &str, fact: &str, reason: &str) -> VaultMutation {
        VaultMutation {
            file_path: canonical_path(entity_id),
            entity_id: entity_id.to_string(),
            kind: MutationKind::FactArchived {
                fact: fact.to_string(),
                reason: reason.to_string(),
            },
        }
    }

    fn rel_mutation(entity_id: &str, rel_type: &str, target: &str) -> VaultMutation {
        VaultMutation {
            file_path: canonical_path(entity_id),
            entity_id: entity_id.to_string(),
            kind: MutationKind::RelationshipAdded {
                rel_type: rel_type.to_string(),
                target: target.to_string(),
            },
        }
    }

    fn create_mutation(entity_id: &str) -> VaultMutation {
        VaultMutation {
            file_path: canonical_path(entity_id),
            entity_id: entity_id.to_string(),
            kind: MutationKind::EntityCreated,
        }
    }

    #[test]
    fn single_fact_added_message() {
        let mutations = vec![fact_mutation("kubernetes", "Monthly cost: $2400")];
        let msg = build_commit_message(&mutations, Some("chat-2026-03-23"));

        assert!(msg.starts_with("memory(kubernetes): added 1 fact [source: chat-2026-03-23]"));
        assert!(msg.contains("Co-Authored-By: Symbiotic AI"));
    }

    #[test]
    fn multiple_facts_same_entity() {
        let mutations = vec![
            fact_mutation("kubernetes", "Cost is high"),
            archive_mutation("kubernetes", "Scales well", "cost analysis"),
            rel_mutation("kubernetes", "replaced_by", "nomad"),
        ];
        let msg = build_commit_message(&mutations, Some("chat-001"));

        assert!(msg.starts_with("memory(kubernetes):"));
        assert!(msg.contains("added 1 fact"));
        assert!(msg.contains("archived 1 fact"));
        assert!(msg.contains("updated 1 relationship"));
    }

    #[test]
    fn multiple_entities_message() {
        let mutations = vec![
            fact_mutation("kubernetes", "Cost is high"),
            create_mutation("nomad"),
            fact_mutation("nomad", "HashiCorp product"),
        ];
        let msg = build_commit_message(&mutations, None);

        assert!(msg.starts_with("memory: update 2 entities"));
        assert!(msg.contains("- kubernetes: added 1 fact"));
        assert!(msg.contains("- nomad: created, added 1 fact"));
    }

    #[test]
    fn entity_created_message() {
        let mutations = vec![create_mutation("nomad")];
        let msg = build_commit_message(&mutations, None);

        assert!(msg.starts_with("memory(nomad): created"));
    }

    #[test]
    fn empty_mutations_returns_empty() {
        let msg = build_commit_message(&[], None);
        assert!(msg.is_empty());
    }

    #[test]
    fn no_mutations_commit_errors() {
        let result = commit_mutations(Path::new("/tmp"), &[], None);
        assert!(matches!(result, Err(VaultGitError::NoMutations)));
    }

    #[test]
    fn plural_facts() {
        let mutations = vec![
            fact_mutation("k8s", "Fact 1"),
            fact_mutation("k8s", "Fact 2"),
            fact_mutation("k8s", "Fact 3"),
        ];
        let msg = build_commit_message(&mutations, None);
        assert!(msg.contains("added 3 facts")); // plural
    }
}
