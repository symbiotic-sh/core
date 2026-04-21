use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use symbiotic_core::now_unix;

use anyhow::{Context, Result};
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewRecord {
    pub record_id: String,
    pub source_url: Option<String>,
    pub tags: Vec<String>,
    pub created_at: u64,
    pub updated_at: u64,
    pub tldr: String,
}

#[derive(Debug, Clone)]
pub struct ReviewRequest {
    pub record_id: String,
    pub source_url: Option<String>,
    pub tags: Vec<String>,
    pub tldr: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewOutcome {
    pub record_id: String,
    pub inserted: bool,
}

#[derive(Debug, Error)]
pub enum ReviewError {
    #[error("invalid review index state: {0}")]
    InvalidState(String),
    #[error("lock poisoned")]
    LockPoisoned,
}

pub struct FileReviewStore {
    root: PathBuf,
    state: Mutex<ReviewState>,
}

#[derive(Debug, Default)]
struct ReviewState {
    index: Vec<ReviewIndexEntry>,
}

#[derive(Debug, Clone)]
struct ReviewIndexEntry {
    record_id: String,
    source_url: Option<String>,
    tags: Vec<String>,
    created_at: u64,
    updated_at: u64,
}

impl FileReviewStore {
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        fs::create_dir_all(records_dir(&root)).with_context(|| {
            format!(
                "failed to create review records directory {}",
                root.display()
            )
        })?;

        let index_file = index_path(&root);
        if !index_file.exists() {
            fs::File::create(&index_file).with_context(|| {
                format!(
                    "failed to initialize review index file {}",
                    index_file.display()
                )
            })?;
        }

        let state = load_state(&root)?;
        Ok(Self {
            root,
            state: Mutex::new(state),
        })
    }

    pub fn exists(&self, record_id: &str) -> Result<bool> {
        let state = self.state.lock().map_err(|_| ReviewError::LockPoisoned)?;
        Ok(state.index.iter().any(|entry| entry.record_id == record_id))
    }

    pub fn store(&self, request: ReviewRequest) -> Result<ReviewOutcome> {
        let mut state = self.state.lock().map_err(|_| ReviewError::LockPoisoned)?;
        if let Some(existing) = state
            .index
            .iter()
            .find(|entry| entry.record_id == request.record_id)
        {
            return Ok(ReviewOutcome {
                record_id: existing.record_id.clone(),
                inserted: false,
            });
        }

        let now = now_unix();
        let tldr_path = record_path(&self.root, &request.record_id);
        fs::write(&tldr_path, &request.tldr)
            .with_context(|| format!("failed to write review summary {}", tldr_path.display()))?;

        state.index.push(ReviewIndexEntry {
            record_id: request.record_id.clone(),
            source_url: request.source_url,
            tags: normalize_tags(request.tags),
            created_at: now,
            updated_at: now,
        });

        persist_state(&self.root, &state)?;
        Ok(ReviewOutcome {
            record_id: request.record_id,
            inserted: true,
        })
    }

    pub fn get(&self, record_id: &str) -> Result<Option<ReviewRecord>> {
        let state = self.state.lock().map_err(|_| ReviewError::LockPoisoned)?;
        let Some(entry) = state
            .index
            .iter()
            .find(|entry| entry.record_id == record_id)
        else {
            return Ok(None);
        };
        let tldr = fs::read_to_string(record_path(&self.root, record_id))
            .with_context(|| format!("failed to read review summary for record {record_id}"))?;
        Ok(Some(ReviewRecord {
            record_id: entry.record_id.clone(),
            source_url: entry.source_url.clone(),
            tags: entry.tags.clone(),
            created_at: entry.created_at,
            updated_at: entry.updated_at,
            tldr,
        }))
    }
}

#[derive(Debug, Clone)]
pub struct ReviewEngine {
    max_sentences: usize,
    max_chars: usize,
}

impl Default for ReviewEngine {
    fn default() -> Self {
        Self {
            max_sentences: 2,
            max_chars: 320,
        }
    }
}

impl ReviewEngine {
    pub fn summarize(&self, content: &str) -> String {
        let mut cleaned = String::new();
        for line in content.lines() {
            let trimmed = strip_markdown(line);
            if trimmed.is_empty() {
                continue;
            }
            cleaned.push_str(trimmed);
            cleaned.push(' ');
            if cleaned.len() > self.max_chars * 2 {
                break;
            }
        }

        if cleaned.trim().is_empty() {
            cleaned = content.replace('\n', " ");
        }

        let sentences = split_sentences(&cleaned);
        let mut summary = String::new();
        for sentence in sentences.into_iter().take(self.max_sentences) {
            if summary.len() + sentence.len() + 1 > self.max_chars {
                break;
            }
            if !summary.is_empty() {
                summary.push(' ');
            }
            summary.push_str(sentence.trim());
        }

        if summary.trim().is_empty() {
            let fallback = cleaned.trim();
            let clipped = if fallback.len() > self.max_chars {
                &fallback[..self.max_chars]
            } else {
                fallback
            };
            return clipped.trim().to_string();
        }

        summary.trim().to_string()
    }
}

fn split_sentences(content: &str) -> Vec<String> {
    let mut sentences = Vec::new();
    let mut current = String::new();
    for ch in content.chars() {
        current.push(ch);
        if matches!(ch, '.' | '!' | '?') {
            if !current.trim().is_empty() {
                sentences.push(current.trim().to_string());
            }
            current.clear();
        }
    }
    if !current.trim().is_empty() {
        sentences.push(current.trim().to_string());
    }
    sentences
}

fn strip_markdown(line: &str) -> &str {
    line.trim()
        .trim_start_matches('#')
        .trim_start_matches('>')
        .trim_start_matches('-')
        .trim_start_matches('*')
        .trim()
}

fn normalize_tags(tags: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = tags
        .into_iter()
        .map(|tag| tag.trim().to_ascii_lowercase())
        .filter(|tag| !tag.is_empty())
        .collect();
    out.sort();
    out.dedup();
    out
}

fn records_dir(root: &Path) -> PathBuf {
    root.join("records")
}

fn record_path(root: &Path, record_id: &str) -> PathBuf {
    records_dir(root).join(format!("{record_id}.md"))
}

fn index_path(root: &Path) -> PathBuf {
    root.join("index.tsv")
}

fn persist_state(root: &Path, state: &ReviewState) -> Result<()> {
    let index_file = index_path(root);
    let tmp_file = index_file.with_extension("tmp");
    let mut f = fs::File::create(&tmp_file)
        .with_context(|| format!("failed to create temp index file {}", tmp_file.display()))?;
    for entry in &state.index {
        writeln!(f, "{}", serialize_index_entry(entry))
            .with_context(|| format!("failed writing temp index {}", tmp_file.display()))?;
    }
    f.flush()
        .with_context(|| format!("failed flushing temp index {}", tmp_file.display()))?;
    fs::rename(&tmp_file, &index_file).with_context(|| {
        format!(
            "failed replacing review index {} with {}",
            index_file.display(),
            tmp_file.display()
        )
    })?;
    Ok(())
}

fn load_state(root: &Path) -> Result<ReviewState> {
    let index_file = index_path(root);
    let mut index = Vec::new();
    let content = fs::read_to_string(&index_file)
        .with_context(|| format!("failed to read index file {}", index_file.display()))?;
    for (idx, line) in content.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let entry = deserialize_index_entry(line).with_context(|| {
            format!(
                "invalid review index at {} line {}",
                index_file.display(),
                idx + 1
            )
        })?;
        index.push(entry);
    }
    Ok(ReviewState { index })
}

fn serialize_index_entry(entry: &ReviewIndexEntry) -> String {
    [
        escape(&entry.record_id),
        escape(entry.source_url.as_deref().unwrap_or("")),
        escape(&entry.tags.join(",")),
        entry.created_at.to_string(),
        entry.updated_at.to_string(),
    ]
    .join("\t")
}

fn deserialize_index_entry(line: &str) -> Result<ReviewIndexEntry> {
    let fields: Vec<&str> = line.split('\t').collect();
    if fields.len() != 5 {
        return Err(
            ReviewError::InvalidState(format!("expected 5 fields, got {}", fields.len())).into(),
        );
    }
    let tags = unescape(fields[2])
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
        .collect();
    Ok(ReviewIndexEntry {
        record_id: unescape(fields[0]),
        source_url: match unescape(fields[1]).trim() {
            "" => None,
            value => Some(value.to_string()),
        },
        tags,
        created_at: fields[3].parse().unwrap_or_default(),
        updated_at: fields[4].parse().unwrap_or_default(),
    })
}

fn escape(input: &str) -> String {
    input
        .replace('\\', "\\\\")
        .replace('\t', "\\t")
        .replace('\n', "\\n")
}

fn unescape(input: &str) -> String {
    let mut out = String::new();
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            match chars.peek() {
                Some('t') => {
                    chars.next();
                    out.push('\t');
                }
                Some('n') => {
                    chars.next();
                    out.push('\n');
                }
                Some('\\') => {
                    chars.next();
                    out.push('\\');
                }
                _ => out.push(ch),
            }
        } else {
            out.push(ch);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn review_store_persists_summary() {
        let root = PathBuf::from("target/tmp-review-store");
        let _ = fs::remove_dir_all(&root);
        let store = FileReviewStore::open(&root).expect("store opens");
        let outcome = store
            .store(ReviewRequest {
                record_id: "rec-1".to_string(),
                source_url: Some("https://example.com".to_string()),
                tags: vec!["tag".to_string(), "Tag".to_string()],
                tldr: "summary".to_string(),
            })
            .expect("store should succeed");
        assert!(outcome.inserted);
        let record = store.get("rec-1").expect("get should succeed");
        assert_eq!(record.unwrap().tldr, "summary");
    }

    #[test]
    fn review_engine_extracts_summary() {
        let engine = ReviewEngine::default();
        let summary = engine.summarize("# Title\nThis is a first sentence. Second sentence here!");
        assert!(summary.contains("first sentence"));
    }
}
