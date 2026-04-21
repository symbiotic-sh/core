//! Shared Vault path/layout helpers.
//!
//! Centralizes the canonical Markdown layout so the writer, migration tool,
//! indexer, and generated-brief surfaces do not drift from each other.

use std::io;
use std::path::{Path, PathBuf};

use crate::{EntityType, MemorySpace};

const GENERATED_ARTIFACT_SUFFIXES: [&str; 4] =
    [".brief.md", ".timeline.md", ".activity.md", ".decisions.md"];

pub fn canonical_entity_relative_path(
    entity_id: &str,
    entity_type: EntityType,
    space: MemorySpace,
) -> PathBuf {
    match space {
        MemorySpace::Knowledge => PathBuf::from("ledger")
            .join(entity_type.plural_dir())
            .join(entity_id)
            .join(format!("{entity_id}.md")),
        MemorySpace::Identity => PathBuf::from("identity").join(format!("{entity_id}.md")),
        MemorySpace::Operations => PathBuf::from("operations").join(format!("{entity_id}.md")),
    }
}

pub fn generated_brief_relative_path(entity_slug: &str, entity_type: EntityType) -> PathBuf {
    PathBuf::from("ledger")
        .join(entity_type.plural_dir())
        .join(entity_slug)
        .join(format!("{entity_slug}.brief.md"))
}

pub fn is_generated_artifact_path(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    GENERATED_ARTIFACT_SUFFIXES
        .iter()
        .any(|suffix| name.ends_with(suffix))
}

pub fn collect_canonical_markdown_files(
    vault_root: &Path,
) -> Result<Vec<(PathBuf, String)>, io::Error> {
    let mut files = Vec::new();
    let roots = [
        vault_root.join("ledger"),
        vault_root.join("identity"),
        vault_root.join("operations"),
    ];

    for root in roots {
        if !root.exists() {
            continue;
        }
        collect_from_dir(vault_root, &root, &mut files)?;
    }

    files.sort_by(|a, b| a.1.cmp(&b.1));
    files.dedup_by(|a, b| a.1 == b.1);
    Ok(files)
}

pub fn find_canonical_entity_file(
    vault_root: &Path,
    entity_id: &str,
) -> Result<Option<PathBuf>, io::Error> {
    let candidates = collect_canonical_markdown_files(vault_root)?;
    for (abs_path, _rel_path) in candidates {
        let Some(stem) = abs_path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if stem == entity_id {
            return Ok(Some(abs_path));
        }
    }
    Ok(None)
}

fn collect_from_dir(
    vault_root: &Path,
    dir: &Path,
    files: &mut Vec<(PathBuf, String)>,
) -> Result<(), io::Error> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect_from_dir(vault_root, &path, files)?;
            continue;
        }
        if path.extension().is_none_or(|ext| ext != "md") || is_generated_artifact_path(&path) {
            continue;
        }

        let rel = path
            .strip_prefix(vault_root)
            .unwrap_or(&path)
            .to_string_lossy()
            .to_string();
        files.push((path, rel));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_knowledge_entity_path_uses_nested_ledger_layout() {
        let path = canonical_entity_relative_path("rust", EntityType::Tool, MemorySpace::Knowledge);
        assert_eq!(path, PathBuf::from("ledger/tools/rust/rust.md"));
    }

    #[test]
    fn canonical_identity_and_operations_paths_use_top_level_roots() {
        assert_eq!(
            canonical_entity_relative_path(
                "preferences",
                EntityType::Preference,
                MemorySpace::Identity
            ),
            PathBuf::from("identity/preferences.md")
        );
        assert_eq!(
            canonical_entity_relative_path("deploy", EntityType::Task, MemorySpace::Operations),
            PathBuf::from("operations/deploy.md")
        );
    }

    #[test]
    fn generated_artifact_suffixes_are_excluded() {
        assert!(is_generated_artifact_path(Path::new(
            "ledger/tools/rust/rust.brief.md"
        )));
        assert!(is_generated_artifact_path(Path::new(
            "ledger/tools/rust/rust.timeline.md"
        )));
        assert!(!is_generated_artifact_path(Path::new(
            "ledger/tools/rust/rust.md"
        )));
    }

    #[test]
    fn collect_files_includes_canonical_and_skips_briefs() {
        let dir = tempfile::tempdir().unwrap();
        let canonical = dir.path().join("ledger/tools/rust/rust.md");
        let brief = dir.path().join("ledger/tools/rust/rust.brief.md");
        std::fs::create_dir_all(canonical.parent().unwrap()).unwrap();
        std::fs::write(&canonical, "# Rust").unwrap();
        std::fs::write(&brief, "# Rust Brief").unwrap();

        let files = collect_canonical_markdown_files(dir.path()).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].1, "ledger/tools/rust/rust.md");
    }
}
