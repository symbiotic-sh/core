//! Vault Linter — validates the strict canonical entity-record contract.
//!
//! The linter enforces the current Vault-as-Truth workflow:
//! - canonical records parse through the standard vault parser
//! - top-level sections use `## Facts`, `## Relationships`, `## History`
//! - semantic history lives under `## History`
//! - generated artifacts such as `*.brief.md` are not canonical lint targets

use crate::vault_layout::is_generated_artifact_path;
use crate::vault_parser;
use std::path::Path;

/// Linting results.
#[derive(Debug, Clone, Default)]
pub struct LintReport {
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
}

impl LintReport {
    pub fn is_valid(&self) -> bool {
        self.errors.is_empty()
    }
}

/// Run all lint checks on a file's content.
pub fn lint_content(content: &str) -> LintReport {
    lint_content_at_path(content, None)
}

/// Run all lint checks on content plus an optional target path.
pub fn lint_content_at_path(content: &str, path: Option<&Path>) -> LintReport {
    let mut report = LintReport::default();

    let parsed = match vault_parser::parse_entity_file(content) {
        Ok(parsed) => {
            if parsed
                .memories
                .iter()
                .all(|memory| memory.status != crate::MemoryStatus::Active)
            {
                report
                    .warnings
                    .push("Entity has no active facts.".to_string());
            }
            if parsed.relationships.is_empty() {
                report
                    .warnings
                    .push("Entity has no relationships (isolated node).".to_string());
            }
            Some(parsed)
        }
        Err(error) => {
            report.errors.push(format!("Parsing failed: {error}"));
            None
        }
    };

    let sections = top_level_sections(content);
    require_section(&mut report, &sections, "Facts");
    require_section(&mut report, &sections, "Relationships");
    require_section(&mut report, &sections, "History");
    if sections.iter().any(|(name, _)| name == "Archived") {
        report.errors.push(
            "Legacy '## Archived' section is not allowed; move archived facts under \
             '## History' -> '### Archived Facts'."
                .to_string(),
        );
    }
    require_section_order(
        &mut report,
        &sections,
        &["Facts", "Relationships", "History"],
    );

    lint_history_structure(content, &mut report);
    lint_absolute_paths(content, &mut report);

    if let (Some(path), Some(parsed)) = (path, parsed.as_ref()) {
        lint_path_contract(path, &parsed.entity.id, &mut report);
    }

    report
}

/// Lint a file on disk.
pub fn lint_file(path: &Path) -> std::io::Result<LintReport> {
    let content = std::fs::read_to_string(path)?;
    Ok(lint_content_at_path(&content, Some(path)))
}

fn top_level_sections(content: &str) -> Vec<(String, usize)> {
    content
        .lines()
        .enumerate()
        .filter_map(|(index, line)| {
            line.trim()
                .strip_prefix("## ")
                .map(|name| (name.trim().to_string(), index + 1))
        })
        .collect()
}

fn require_section(report: &mut LintReport, sections: &[(String, usize)], name: &str) {
    if !sections
        .iter()
        .any(|(section_name, _)| section_name == name)
    {
        report.errors.push(format!("Missing '## {name}' section."));
    }
}

fn require_section_order(report: &mut LintReport, sections: &[(String, usize)], names: &[&str]) {
    let mut previous_line = None;
    for name in names {
        let Some((_, line)) = sections
            .iter()
            .find(|(section_name, _)| section_name == *name)
        else {
            return;
        };
        if let Some(previous_line) = previous_line {
            if *line <= previous_line {
                report.errors.push(
                    "Section order must be '## Facts' -> '## Relationships' -> '## History'."
                        .to_string(),
                );
                return;
            }
        }
        previous_line = Some(*line);
    }
}

fn lint_history_structure(content: &str, report: &mut LintReport) {
    let mut in_history = false;
    let mut history_subsection: Option<&str> = None;

    for (index, raw_line) in content.lines().enumerate() {
        let line = raw_line.trim();
        if let Some(section_name) = line.strip_prefix("## ") {
            in_history = section_name.trim() == "History";
            history_subsection = None;
            continue;
        }
        if let Some(subsection_name) = line.strip_prefix("### ") {
            if in_history {
                history_subsection = Some(subsection_name.trim());
            } else {
                report.errors.push(format!(
                    "History-style subsection outside '## History' on line {}.",
                    index + 1
                ));
            }
            continue;
        }
        if !line.starts_with("- ") {
            continue;
        }

        let is_archived_fact = line.contains("~~") || line.contains("[archived:");
        let is_relationship_change =
            line.starts_with("- removed:") || line.starts_with("- replaced:");

        if is_archived_fact && history_subsection != Some("Archived Facts") {
            report.errors.push(format!(
                "Archived fact line must live under '## History' -> '### Archived Facts' (line {}).",
                index + 1
            ));
        }
        if is_relationship_change && history_subsection != Some("Relationship Changes") {
            report.errors.push(format!(
                "Relationship change line must live under '## History' -> '### Relationship Changes' (line {}).",
                index + 1
            ));
        }
    }
}

fn lint_absolute_paths(content: &str, report: &mut LintReport) {
    for (index, line) in content.lines().enumerate() {
        if line.contains("/Users/")
            || line.contains("C:\\")
            || (line.contains('/') && line.contains(':') && !line.contains("://"))
        {
            report.warnings.push(format!(
                "Potential absolute path found on line {}: {}",
                index + 1,
                line.trim()
            ));
        }
    }
}

fn lint_path_contract(path: &Path, entity_id: &str, report: &mut LintReport) {
    if is_generated_artifact_path(path) {
        report
            .errors
            .push("Generated artifact files are not canonical lint targets.".to_string());
        return;
    }

    let normalized = path.to_string_lossy().replace('\\', "/");
    let file_name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("");
    if file_name != format!("{entity_id}.md") {
        report.errors.push(format!(
            "ID mismatch: canonical filename must be '{entity_id}.md' but found '{file_name}'."
        ));
    }

    if normalized.contains("/ledger/") {
        let expected_suffix = format!("/{entity_id}/{entity_id}.md");
        if !normalized.ends_with(&expected_suffix) {
            report.errors.push(format!(
                "Ledger records must use per-entity folders ending with '{}'.",
                expected_suffix.trim_start_matches('/')
            ));
        }
    } else if !(normalized.contains("/identity/") || normalized.contains("/operations/")) {
        report.errors.push(
            "Canonical entity records must live under ledger/, identity/, or operations/."
                .to_string(),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_lint_valid_content() {
        let content = r#"---
id: test
type: tool
space: knowledge
created: 2026-01-01T00:00:00Z
updated: 2026-01-01T00:00:00Z
---
# Test
## Facts
- A fact [source: manual]
## Relationships
- used_with: [[other]]
## History
### Archived Facts
- ~~Old fact~~ [source: manual] [archived: 2026-01-02, reason: outdated]
"#;
        let report = lint_content(content);
        assert!(report.is_valid(), "{:?}", report.errors);
    }

    #[test]
    fn test_lint_legacy_archived_section_errors() {
        let content = r#"---
id: test
type: tool
space: knowledge
created: 2026-01-01T00:00:00Z
updated: 2026-01-01T00:00:00Z
---
# Test
## Facts
- A fact [source: manual]
## Archived
- ~~Old fact~~ [archived: 2026-01-02, reason: outdated]
## Relationships
- used_with: [[other]]
## History
"#;
        let report = lint_content(content);
        assert!(!report.is_valid());
        assert!(report
            .errors
            .iter()
            .any(|error| error.contains("Legacy '## Archived'")));
    }

    #[test]
    fn test_lint_generated_artifact_path_errors() {
        let content = r#"---
id: rust
type: tool
space: knowledge
created: 2026-01-01T00:00:00Z
updated: 2026-01-01T00:00:00Z
---
# Rust
## Facts
- A fact [source: manual]
## Relationships
- used_with: [[cargo]]
## History
"#;
        let report =
            lint_content_at_path(content, Some(Path::new("ledger/tools/rust/rust.brief.md")));
        assert!(!report.is_valid());
        assert!(report
            .errors
            .iter()
            .any(|error| error.contains("not canonical lint targets")));
    }
}
