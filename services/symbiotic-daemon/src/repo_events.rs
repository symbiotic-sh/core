//! Append-only repo lifecycle event markdown emission.
//!
//! Mirrors the goal-event pattern but writes to
//! `operations/projects/{slug}/repos/events/`. The canonical list of event
//! types lives in `docs/design/repo-manifest.md` §Lifecycle Events. Each event
//! is a single markdown file with YAML frontmatter + human-readable body.
//!
//! This module intentionally duplicates the tiny sanitizer / atomic-write
//! helpers that `goal_management.rs` uses rather than coupling the two
//! surfaces via a shared `pub(crate)` API — the goal and repo event file
//! formats may diverge independently.

use std::fs;
use std::path::{Path, PathBuf};

use thiserror::Error;

/// Error surface for `append_repo_event_archive`.
#[derive(Debug, Error)]
pub enum RepoEventError {
    #[error("io error ({ctx}): {source}")]
    Io {
        ctx: String,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid event type: {0}")]
    #[allow(dead_code)]
    InvalidEventType(String),
}

/// Sanitize an arbitrary string into an archive-safe path component.
///
/// Lowercases ASCII alphanumerics, collapses runs of non-alphanumeric
/// characters (including `-` / `_`) into single dashes, trims leading and
/// trailing dashes. Empty input becomes `"item"`.
fn sanitize_archive_component(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut last_dash = false;
    for ch in value.chars() {
        let normalized = if ch.is_ascii_alphanumeric() {
            Some(ch.to_ascii_lowercase())
        } else if matches!(ch, '-' | '_') {
            Some('-')
        } else {
            None
        };
        match normalized {
            Some('-') => {
                if !last_dash && !out.is_empty() {
                    out.push('-');
                    last_dash = true;
                }
            }
            Some(ch) => {
                out.push(ch);
                last_dash = false;
            }
            None => {
                if !last_dash && !out.is_empty() {
                    out.push('-');
                    last_dash = true;
                }
            }
        }
    }
    let out = out.trim_matches('-');
    if out.is_empty() {
        "item".to_string()
    } else {
        out.to_string()
    }
}

/// Write `content` to `path` atomically via a temp-file + rename.
fn write_markdown_atomic(path: &Path, content: &str) -> Result<(), RepoEventError> {
    let parent = path.parent().ok_or_else(|| RepoEventError::Io {
        ctx: format!("missing parent directory for {}", path.display()),
        source: std::io::Error::new(std::io::ErrorKind::InvalidInput, "no parent"),
    })?;
    fs::create_dir_all(parent).map_err(|source| RepoEventError::Io {
        ctx: format!("create_dir_all({})", parent.display()),
        source,
    })?;
    let file_name = path
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| RepoEventError::Io {
            ctx: format!("invalid file name for {}", path.display()),
            source: std::io::Error::new(std::io::ErrorKind::InvalidInput, "bad file name"),
        })?;
    let tmp_path = parent.join(format!("{file_name}.tmp"));
    fs::write(&tmp_path, content).map_err(|source| RepoEventError::Io {
        ctx: format!("write({})", tmp_path.display()),
        source,
    })?;
    fs::rename(&tmp_path, path).map_err(|source| RepoEventError::Io {
        ctx: format!("rename({} -> {})", tmp_path.display(), path.display()),
        source,
    })?;
    Ok(())
}

/// Convert a `project:...` prefixed id into a sanitized archive component.
fn project_archive_component(project_id: &str) -> String {
    let raw = project_id.strip_prefix("project:").unwrap_or(project_id);
    sanitize_archive_component(raw)
}

/// Convert an event-type snake_case token to a Title Case title string.
/// `"repo_mirror_pull_completed"` -> `"Repo Mirror Pull Completed"`.
fn event_type_title(event_type: &str) -> String {
    event_type
        .split('_')
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(first) => {
                    let mut word = first.to_uppercase().collect::<String>();
                    word.push_str(chars.as_str());
                    word
                }
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Append an append-only repo lifecycle event markdown doc under
/// `{archive_root}/operations/projects/{project_slug}/repos/events/`.
///
/// `event_type` examples: `"repo_attached"`, `"repo_state_changed"`,
/// `"repo_mirror_pull_completed"`. Canonical list in
/// `docs/design/repo-manifest.md` §Lifecycle Events.
///
/// `observed_at` is Unix seconds. `detail` is a short slug-safe suffix
/// (typically the repo slug). `payload_yaml_fragment` is raw YAML lines (no
/// `---` delimiters) appended after the canonical header fields.
/// `body_markdown` is the human-readable prose below the frontmatter.
///
/// Returns the absolute path of the written event file.
pub fn append_repo_event_archive(
    archive_root: &Path,
    project_id: &str,
    repo_id: &str,
    event_type: &str,
    observed_at: i64,
    detail: &str,
    payload_yaml_fragment: &str,
    body_markdown: &str,
) -> Result<PathBuf, RepoEventError> {
    let project_slug = project_archive_component(project_id);
    let events_dir = archive_root
        .join("operations")
        .join("projects")
        .join(project_slug)
        .join("repos")
        .join("events");
    fs::create_dir_all(&events_dir).map_err(|source| RepoEventError::Io {
        ctx: format!("create_dir_all({})", events_dir.display()),
        source,
    })?;
    let event_path = events_dir.join(format!(
        "{}-{}-{}.md",
        observed_at,
        sanitize_archive_component(event_type),
        sanitize_archive_component(detail)
    ));
    let title = event_type_title(event_type);
    let mut content = String::new();
    content.push_str("---\n");
    content.push_str(&format!("repo_id: \"{repo_id}\"\n"));
    content.push_str(&format!("event_type: \"{event_type}\"\n"));
    content.push_str(&format!("observed_at: {observed_at}\n"));
    if !payload_yaml_fragment.is_empty() {
        content.push_str(payload_yaml_fragment);
        if !payload_yaml_fragment.ends_with('\n') {
            content.push('\n');
        }
    }
    content.push_str("---\n\n");
    content.push_str(&format!("# {title}\n\n"));
    content.push_str(body_markdown);
    if !body_markdown.ends_with('\n') {
        content.push('\n');
    }
    write_markdown_atomic(&event_path, &content)?;
    Ok(event_path)
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn find_event_files(events_dir: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        if let Ok(iter) = fs::read_dir(events_dir) {
            for entry in iter.flatten() {
                let path = entry.path();
                if path.is_file()
                    && path
                        .extension()
                        .and_then(|ext| ext.to_str())
                        .map(|ext| ext == "md")
                        .unwrap_or(false)
                {
                    out.push(path);
                }
            }
        }
        out.sort();
        out
    }

    // 1
    #[test]
    fn append_event_creates_events_dir_and_file() {
        let tmp = TempDir::new().unwrap();
        let archive_root = tmp.path();
        let path = append_repo_event_archive(
            archive_root,
            "project:test",
            "repo:test",
            "repo_mirror_pull_completed",
            1_700_000_000,
            "test",
            "head_after: null\nrefs_advanced: false\ninitialized: true\n",
            "Body.\n",
        )
        .expect("append");
        assert!(
            path.exists(),
            "event file should exist at {}",
            path.display()
        );
        let events_dir = archive_root
            .join("operations")
            .join("projects")
            .join("test")
            .join("repos")
            .join("events");
        assert!(events_dir.is_dir(), "events dir exists");
    }

    // 2
    #[test]
    fn event_file_has_canonical_frontmatter() {
        let tmp = TempDir::new().unwrap();
        let path = append_repo_event_archive(
            tmp.path(),
            "project:test",
            "repo:test",
            "repo_mirror_pull_completed",
            1_700_000_000,
            "test",
            "head_after: null\nrefs_advanced: false\ninitialized: true\n",
            "Body text.\n",
        )
        .expect("append");
        let contents = fs::read_to_string(&path).unwrap();
        assert!(
            contents.contains("repo_id: \"repo:test\""),
            "repo_id present: {contents}"
        );
        assert!(
            contents.contains("event_type: \"repo_mirror_pull_completed\""),
            "event_type present: {contents}"
        );
        assert!(
            contents.contains("observed_at: 1700000000"),
            "observed_at present: {contents}"
        );
    }

    // 3
    #[test]
    fn sanitized_components_in_filename() {
        let tmp = TempDir::new().unwrap();
        let path = append_repo_event_archive(
            tmp.path(),
            "project:weird name!",
            "repo:weird",
            "repo_mirror_pull_completed",
            1_700_000_000,
            "test",
            "head_after: null\nrefs_advanced: false\ninitialized: true\n",
            "Body.\n",
        )
        .expect("append");
        let path_str = path.to_string_lossy().to_string();
        assert!(
            path_str.contains("/projects/weird-name/"),
            "sanitized project slug in path: {path_str}"
        );
        let filename = path.file_name().unwrap().to_string_lossy().to_string();
        assert!(
            filename.contains("repo-mirror-pull-completed"),
            "sanitized event type in filename: {filename}"
        );
    }

    // 4
    #[test]
    fn payload_fragment_appears_inline() {
        let tmp = TempDir::new().unwrap();
        let path = append_repo_event_archive(
            tmp.path(),
            "project:test",
            "repo:test",
            "repo_mirror_pull_completed",
            1_700_000_000,
            "test",
            "foo: true\nbar: 42\n",
            "Body.\n",
        )
        .expect("append");
        let contents = fs::read_to_string(&path).unwrap();
        assert!(
            contents.contains("foo: true"),
            "foo line present: {contents}"
        );
        assert!(contents.contains("bar: 42"), "bar line present: {contents}");
        // Ensure they are inside the frontmatter region (before the trailing ---).
        let closing = contents.find("---\n\n").expect("closing fence");
        let head = &contents[..closing];
        assert!(head.contains("foo: true"));
        assert!(head.contains("bar: 42"));
    }

    // 5
    #[test]
    fn body_markdown_below_frontmatter() {
        let tmp = TempDir::new().unwrap();
        let path = append_repo_event_archive(
            tmp.path(),
            "project:test",
            "repo:test",
            "repo_mirror_pull_completed",
            1_700_000_000,
            "test",
            "head_after: null\nrefs_advanced: false\ninitialized: true\n",
            "Body paragraph text.\n",
        )
        .expect("append");
        let contents = fs::read_to_string(&path).unwrap();
        // Body must appear after a `---\n\n` fence.
        let idx = contents.find("---\n\n").expect("closing fence");
        let body = &contents[idx + "---\n\n".len()..];
        assert!(
            body.contains("Body paragraph text."),
            "body text appears below frontmatter: {body}"
        );
    }

    // 6
    #[test]
    fn second_event_with_same_type_uses_different_filename() {
        let tmp = TempDir::new().unwrap();
        let path_a = append_repo_event_archive(
            tmp.path(),
            "project:test",
            "repo:test",
            "repo_mirror_pull_completed",
            1_700_000_000,
            "test",
            "head_after: null\nrefs_advanced: false\ninitialized: true\n",
            "first\n",
        )
        .expect("append a");
        let path_b = append_repo_event_archive(
            tmp.path(),
            "project:test",
            "repo:test",
            "repo_mirror_pull_completed",
            1_700_000_001,
            "test",
            "head_after: null\nrefs_advanced: false\ninitialized: true\n",
            "second\n",
        )
        .expect("append b");
        assert_ne!(path_a, path_b, "distinct observed_at -> distinct filenames");
        let events_dir = tmp
            .path()
            .join("operations")
            .join("projects")
            .join("test")
            .join("repos")
            .join("events");
        let files = find_event_files(&events_dir);
        assert_eq!(files.len(), 2, "two event files written: {files:?}");
    }
}
