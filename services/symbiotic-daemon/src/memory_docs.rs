//! Thread Memory Document generator.
//!
//! Produces living Markdown summaries for each thread, stored in
//! `knowledge-base/threads/{slug}.md`. These documents are regenerated
//! from active facts in the Neural Graph whenever the thread's fact set
//! changes (goal completion, new decisions, periodic schedule).
//!
//! See `docs/design/memory-system.md` Layer 3 — Thread Memory Documents.

use std::fmt::Write as FmtWrite;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};

// Re-exported domain types from symbiotic-memory.
// These will resolve once the crate dependency is wired in Cargo.toml.
use symbiotic_memory::staleness::{StalenessChecker, StalenessResult};
use symbiotic_memory::types::{Entity, Memory, MemoryStatus};

/// Result of generating a Thread Memory Document.
#[derive(Debug, Clone)]
pub struct DocGenResult {
    /// Path to the written Markdown file.
    pub path: PathBuf,
    /// SHA-256 hex hash of the Markdown content (for cache validation).
    pub content_hash: String,
}

/// Compute a SHA-256 hex hash of the given content.
pub fn compute_content_hash(content: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// Summary of a goal associated with a thread.
#[derive(Debug, Clone)]
pub struct GoalSummary {
    /// Human-readable goal title.
    pub title: String,
    /// Current status (e.g. "active", "completed", "paused").
    pub status: String,
    /// Progress percentage (0–100).
    pub progress: u8,
    /// When the goal was last updated (ISO-8601 date string).
    pub updated_at: String,
}

/// A single entry in the decision history table.
#[derive(Debug, Clone)]
pub struct DecisionHistoryEntry {
    /// ISO-8601 date string.
    pub date: String,
    /// The decision text.
    pub decision: String,
    /// ID of a superseded decision, if any.
    pub supersedes: Option<String>,
}

/// Generates Thread Memory Documents as Markdown files.
///
/// One document per thread at `{kb_root}/threads/{slug}.md`.
/// These are living summaries regenerated from the Neural Graph — not
/// manually edited (though users may edit them in Obsidian).
pub struct ThreadMemoryDocGenerator {
    kb_root: PathBuf,
}

impl ThreadMemoryDocGenerator {
    /// Create a new generator targeting the given knowledge-base root.
    pub fn new(kb_root: impl AsRef<Path>) -> Self {
        Self {
            kb_root: kb_root.as_ref().to_path_buf(),
        }
    }

    /// Generate a Thread Memory Doc for a given thread.
    ///
    /// Returns the path to the written Markdown file.
    ///
    /// # Arguments
    /// * `thread_id` — internal thread identifier (e.g. "thread-saas-product")
    /// * `thread_title` — human-readable title
    /// * `messages_summary` — AI-generated summary of the conversation
    /// * `entities` — entities referenced in this thread
    /// * `decisions` — memories classified as decisions
    /// * `findings` — memories classified as findings
    /// * `goals` — active/completed goals for this thread
    /// * `decision_history` — chronological decision history with superseding info
    #[allow(clippy::too_many_arguments)]
    pub fn generate(
        &self,
        thread_id: &str,
        thread_title: &str,
        messages_summary: &str,
        entities: &[Entity],
        decisions: &[Memory],
        findings: &[Memory],
        goals: &[GoalSummary],
        decision_history: &[DecisionHistoryEntry],
    ) -> Result<DocGenResult> {
        let slug = slug_from_thread_id(thread_id);
        let threads_dir = self.kb_root.join("threads");
        fs::create_dir_all(&threads_dir).with_context(|| {
            format!(
                "failed to create threads directory {}",
                threads_dir.display()
            )
        })?;

        let mut doc = String::with_capacity(4096);

        // --- YAML frontmatter ---
        writeln!(doc, "---").unwrap();
        writeln!(doc, "type: thread-memory").unwrap();
        writeln!(doc, "thread_id: {thread_id}").unwrap();
        writeln!(doc, "---").unwrap();
        writeln!(doc).unwrap();

        // --- Title ---
        writeln!(doc, "# {thread_title}").unwrap();
        writeln!(doc).unwrap();

        // --- Summary ---
        writeln!(doc, "## Summary").unwrap();
        writeln!(doc).unwrap();
        writeln!(doc, "{messages_summary}").unwrap();
        writeln!(doc).unwrap();

        // --- Key Decisions ---
        if !decisions.is_empty() {
            let active_decisions: Vec<&Memory> = decisions
                .iter()
                .filter(|d| d.status == MemoryStatus::Active)
                .collect();
            if !active_decisions.is_empty() {
                writeln!(doc, "## Key Decisions").unwrap();
                writeln!(doc).unwrap();
                for decision in active_decisions {
                    let date = format_date(&decision.valid_from);
                    writeln!(doc, "- {} ({})", decision.fact, date).unwrap();
                }
                writeln!(doc).unwrap();
            }
        }

        // --- Findings ---
        if !findings.is_empty() {
            writeln!(doc, "## Findings").unwrap();
            writeln!(doc).unwrap();
            for finding in findings {
                let source = entity_wikilink_for_memory(finding, entities);
                let date = format_date(&finding.valid_from);
                writeln!(doc, "- {} ({}, {})", finding.fact, source, date).unwrap();
            }
            writeln!(doc).unwrap();
        }

        // --- Entities ---
        if !entities.is_empty() {
            writeln!(doc, "## Entities").unwrap();
            writeln!(doc).unwrap();
            for entity in entities {
                let description = entity_short_description(entity);
                writeln!(doc, "- [[{}]] — {}", entity.name, description).unwrap();
            }
            writeln!(doc).unwrap();
        }

        // --- Active Goals ---
        if !goals.is_empty() {
            writeln!(doc, "## Active Goals").unwrap();
            writeln!(doc).unwrap();
            for goal in goals {
                writeln!(
                    doc,
                    "- {} ({}, {}%)",
                    goal.title, goal.status, goal.progress
                )
                .unwrap();
            }
            writeln!(doc).unwrap();
        }

        // --- Decision History ---
        if !decision_history.is_empty() {
            writeln!(doc, "## Decision History").unwrap();
            writeln!(doc).unwrap();
            for entry in decision_history {
                match entry.supersedes.as_deref() {
                    Some(previous) if !previous.trim().is_empty() => {
                        writeln!(
                            doc,
                            "- {} — {} (supersedes: {})",
                            entry.date, entry.decision, previous
                        )
                        .unwrap();
                    }
                    _ => {
                        writeln!(doc, "- {} — {}", entry.date, entry.decision).unwrap();
                    }
                }
            }
            writeln!(doc).unwrap();
        }

        // --- Staleness Warnings ---
        // Combine decisions + findings and run staleness scan.
        let all_facts: Vec<&Memory> = decisions.iter().chain(findings.iter()).collect();
        if !all_facts.is_empty() {
            let checker = StalenessChecker::default();
            let all_owned: Vec<Memory> = all_facts.iter().map(|m| (*m).clone()).collect();
            if let Some(section) = format_staleness_warnings(&all_owned, &checker) {
                writeln!(doc, "{section}").unwrap();
            }
        }

        // --- Compute content hash and insert into frontmatter ---
        let content_hash = compute_content_hash(&doc);

        // Insert content_hash into the YAML frontmatter (before the closing `---`).
        // The frontmatter starts with "---\n" and the closing "---\n" is on line 4.
        // We insert `content_hash: {hex}` just before the closing `---`.
        let doc = if let Some(second_sep) = doc
            .find("---\n")
            .and_then(|first| doc[first + 4..].find("---\n").map(|pos| first + 4 + pos))
        {
            let mut with_hash = String::with_capacity(doc.len() + 80);
            with_hash.push_str(&doc[..second_sep]);
            writeln!(with_hash, "content_hash: {content_hash}").unwrap();
            with_hash.push_str(&doc[second_sep..]);
            with_hash
        } else {
            doc
        };

        // --- Write file ---
        let output_path = threads_dir.join(format!("{slug}.md"));
        fs::write(&output_path, &doc).with_context(|| {
            format!(
                "failed to write thread memory doc {}",
                output_path.display()
            )
        })?;

        Ok(DocGenResult {
            path: output_path,
            content_hash,
        })
    }

    /// Check if a Thread Memory Doc needs regeneration.
    ///
    /// Regeneration is triggered when:
    /// - The doc has never been generated (`last_generated` is `None`)
    /// - The doc is older than `max_age_secs` seconds
    ///
    /// The caller is responsible for also triggering regeneration on
    /// fact changes (new decisions, goal completions, etc.).
    pub fn needs_regeneration(
        &self,
        _thread_id: &str,
        last_generated: Option<u64>,
        now: u64,
        max_age_secs: u64,
    ) -> bool {
        let last = match last_generated {
            Some(ts) => ts,
            None => return true,
        };

        // Regenerate if the doc is stale beyond max age.
        now.saturating_sub(last) > max_age_secs
    }

    /// Return the expected file path for a given thread.
    pub fn doc_path(&self, thread_id: &str) -> PathBuf {
        let slug = slug_from_thread_id(thread_id);
        self.kb_root.join("threads").join(format!("{slug}.md"))
    }

    /// Check whether a doc already exists on disk.
    pub fn doc_exists(&self, thread_id: &str) -> bool {
        self.doc_path(thread_id).exists()
    }
}

/// Build a `RoutedMatrixEnvelope` for a `memory_doc.updated` event.
///
/// This is emitted to the thread's Matrix room whenever a Thread Memory Doc
/// is generated or regenerated, so the app knows to re-fetch the doc.
///
/// # Arguments
/// * `thread_id` — the thread identifier (e.g. "thread-saas-product")
/// * `content_hash` — SHA-256 hex hash of the generated Markdown content
/// * `doc_path` — filesystem path to the doc (relative or absolute)
/// * `now` — current Unix timestamp
/// * `target_room` — Matrix room ID to route the event to
pub fn build_memory_doc_updated_envelope(
    thread_id: &str,
    content_hash: &str,
    doc_path: &std::path::Path,
    now: u64,
    target_room: &str,
) -> crate::events::RoutedMatrixEnvelope {
    use symbiotic_core::protocol::{Kind, Status};
    use symbiotic_matrix::events::MatrixEventEnvelope;

    let body = format!("Thread memory doc updated for {thread_id}");
    let envelope = MatrixEventEnvelope::new(Kind::State, Status::Success, now, &body)
        .with_thread(thread_id)
        .with_detail_field("thread_id", thread_id)
        .with_detail_field("content_hash", content_hash)
        .with_detail_field("updated_at", now)
        .with_detail_field("path", doc_path.display().to_string())
        .with_detail_field("event_type", "memory_doc.updated");

    crate::events::RoutedMatrixEnvelope {
        room_id: target_room.to_string(),
        envelope,
    }
}

// --- Staleness formatting ---

/// Format staleness warnings for a set of memories.
///
/// Returns `None` if all facts are fresh (no warnings needed), or
/// `Some(markdown_section)` containing a `## Staleness Warnings` section
/// with one bullet per stale or suspect fact.
///
/// Age is computed from `valid_from` relative to the current system time.
pub fn format_staleness_warnings(facts: &[Memory], checker: &StalenessChecker) -> Option<String> {
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    format_staleness_warnings_at(facts, checker, now_secs)
}

/// Testable variant of [`format_staleness_warnings`] that accepts an explicit
/// "now" timestamp (epoch seconds) instead of reading the system clock.
pub fn format_staleness_warnings_at(
    facts: &[Memory],
    checker: &StalenessChecker,
    now_secs: u64,
) -> Option<String> {
    let age_fn = |mem: &Memory| -> u64 {
        // Parse ISO-8601 date from valid_from; fall back to 0 days if unparseable.
        parse_date_to_epoch_secs(&mem.valid_from)
            .map(|epoch| now_secs.saturating_sub(epoch) / 86400)
            .unwrap_or(0)
    };

    let stale_ids = checker.scan(facts, age_fn);

    if stale_ids.is_empty() {
        return None;
    }

    // Build a lookup from id -> fact text and id -> stale reason.
    let fact_by_id: std::collections::HashMap<&str, &Memory> =
        facts.iter().map(|m| (m.id.as_str(), m)).collect();

    let mut section = String::new();
    writeln!(section, "## Staleness Warnings").unwrap();
    writeln!(section).unwrap();

    for id in &stale_ids {
        if let Some(mem) = fact_by_id.get(id.as_str()) {
            let age = age_fn(mem);
            let result = checker.check_staleness(mem, age);
            let reason = match result {
                StalenessResult::Stale { reason } => reason,
                _ => "unknown".to_string(),
            };
            let date = format_date(&mem.valid_from);
            writeln!(
                section,
                "- **{}** — Stale since {}: {}",
                mem.fact, date, reason
            )
            .unwrap();
        }
    }

    Some(section)
}

/// Parse an ISO-8601 date string to epoch seconds.
/// Handles both `YYYY-MM-DD` and `YYYY-MM-DDThh:mm:ssZ` formats.
fn parse_date_to_epoch_secs(date_str: &str) -> Option<u64> {
    // Try full ISO-8601 with time first
    if date_str.len() >= 19 {
        // "2026-03-15T10:30:00Z" — parse year/month/day/hour/min/sec
        let year: i64 = date_str.get(0..4)?.parse().ok()?;
        let month: u64 = date_str.get(5..7)?.parse().ok()?;
        let day: u64 = date_str.get(8..10)?.parse().ok()?;
        let hour: u64 = date_str.get(11..13)?.parse().ok()?;
        let min: u64 = date_str.get(14..16)?.parse().ok()?;
        let sec: u64 = date_str.get(17..19)?.parse().ok()?;
        return Some(simple_epoch(year, month, day, hour, min, sec));
    }
    // Try date-only: "2026-03-15"
    if date_str.len() >= 10 {
        let year: i64 = date_str.get(0..4)?.parse().ok()?;
        let month: u64 = date_str.get(5..7)?.parse().ok()?;
        let day: u64 = date_str.get(8..10)?.parse().ok()?;
        return Some(simple_epoch(year, month, day, 0, 0, 0));
    }
    None
}

/// Approximate epoch seconds from date components.
/// Not leap-second-perfect, but sufficient for staleness day-level calculations.
fn simple_epoch(year: i64, month: u64, day: u64, hour: u64, min: u64, sec: u64) -> u64 {
    // Days from months (approximate, non-leap)
    const DAYS_BEFORE_MONTH: [u64; 13] = [0, 0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334];
    let m = month.clamp(1, 12) as usize;
    let y = year as u64;
    // Days since epoch (1970-01-01), simplified
    let years_since_1970 = y.saturating_sub(1970);
    let leap_years = (y.saturating_sub(1969)) / 4 - (y.saturating_sub(1901)) / 100
        + (y.saturating_sub(1601)) / 400;
    let day_of_year = DAYS_BEFORE_MONTH[m] + day.saturating_sub(1);
    let total_days = years_since_1970 * 365 + leap_years + day_of_year;
    total_days * 86400 + hour * 3600 + min * 60 + sec
}

// --- Helpers ---

/// Derive a filename slug from a thread_id.
/// "thread-saas-product" → "saas-product"
fn slug_from_thread_id(thread_id: &str) -> String {
    thread_id
        .strip_prefix("thread-")
        .unwrap_or(thread_id)
        .to_string()
}

/// Format a date string for display. If already short (YYYY-MM-DD),
/// return as-is. Otherwise trim to the first 10 chars.
fn format_date(date_str: &str) -> &str {
    if date_str.len() >= 10 {
        &date_str[..10]
    } else {
        date_str
    }
}

/// Find the entity name wikilink for a memory's entity_id.
fn entity_wikilink_for_memory(memory: &Memory, entities: &[Entity]) -> String {
    entities
        .iter()
        .find(|e| e.id == memory.entity_id)
        .map(|e| format!("source: [[{}]]", e.name))
        .unwrap_or_else(|| "source: unknown".to_string())
}

/// Generate a short description from an entity's type and attributes.
fn entity_short_description(entity: &Entity) -> String {
    // Try to extract a "description" or "role" from attributes.
    if let Some(desc) = entity
        .attributes
        .get("description")
        .and_then(|v| v.as_str())
    {
        return desc.to_string();
    }
    if let Some(role) = entity.attributes.get("role").and_then(|v| v.as_str()) {
        return role.to_string();
    }
    // Fallback to entity type.
    entity.entity_type.singular_label().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use symbiotic_memory::staleness::StalenessConfig;
    use symbiotic_memory::types::{
        AllowedModels, EntityStatus, EntityType, FactDisposition, FactType, MemoryStatus,
        Sensitivity,
    };

    fn make_entity(id: &str, name: &str, entity_type: EntityType) -> Entity {
        Entity {
            id: id.to_string(),
            entity_type,
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

    #[test]
    fn slug_from_thread_id_strips_prefix() {
        assert_eq!(slug_from_thread_id("thread-saas-product"), "saas-product");
        assert_eq!(slug_from_thread_id("no-prefix"), "no-prefix");
    }

    #[test]
    fn format_date_truncates() {
        assert_eq!(format_date("2026-03-15T10:30:00Z"), "2026-03-15");
        assert_eq!(format_date("2026-03-15"), "2026-03-15");
        assert_eq!(format_date("short"), "short");
    }

    #[test]
    fn generate_produces_valid_markdown() {
        let dir = tempfile::tempdir().unwrap();
        let gen = ThreadMemoryDocGenerator::new(dir.path());

        let entities = vec![
            make_entity("vue-001", "Vue", EntityType::Tool),
            make_entity("stripe-001", "Stripe", EntityType::Tool),
        ];
        let decisions = vec![make_memory(
            "m1",
            "vue-001",
            "Use Vue over React for frontend",
            "2026-03-14",
        )];
        let findings = vec![make_memory(
            "m2",
            "stripe-001",
            "Stripe charges 2.9% + 30c per transaction",
            "2026-03-12",
        )];
        let goals = vec![GoalSummary {
            title: "Build landing page".to_string(),
            status: "active".to_string(),
            progress: 40,
            updated_at: "2026-03-15".to_string(),
        }];
        let history = vec![DecisionHistoryEntry {
            date: "2026-03-14".to_string(),
            decision: "Vue + Nuxt selected".to_string(),
            supersedes: Some("React + Next.js".to_string()),
        }];

        let result = gen
            .generate(
                "thread-saas-product",
                "SaaS Product",
                "Discussion about building the SaaS analytics platform.",
                &entities,
                &decisions,
                &findings,
                &goals,
                &history,
            )
            .unwrap();

        assert!(result.path.exists());
        assert!(result.path.ends_with("saas-product.md"));
        assert!(!result.content_hash.is_empty());

        let content = fs::read_to_string(&result.path).unwrap();

        // Verify structural sections
        assert!(content.contains("# SaaS Product"));
        assert!(content.contains("## Summary"));
        assert!(content.contains("## Key Decisions"));
        assert!(content.contains("## Findings"));
        assert!(content.contains("## Entities"));
        assert!(content.contains("## Active Goals"));
        assert!(content.contains("## Decision History"));

        // Verify content
        assert!(content.contains("Use Vue over React for frontend"));
        assert!(content.contains("[[Vue]]"));
        assert!(content.contains("[[Stripe]]"));
        assert!(content.contains("Build landing page (active, 40%)"));
        assert!(content.contains("2026-03-14 — Vue + Nuxt selected"));
        assert!(content.contains("supersedes: React + Next.js"));

        // Verify frontmatter
        assert!(content.contains("type: thread-memory"));
        assert!(content.contains("thread_id: thread-saas-product"));
        assert!(content.contains(&format!("content_hash: {}", result.content_hash)));
    }

    #[test]
    fn generate_with_empty_sections() {
        let dir = tempfile::tempdir().unwrap();
        let gen = ThreadMemoryDocGenerator::new(dir.path());

        let result = gen
            .generate(
                "thread-empty",
                "Empty Thread",
                "No activity yet.",
                &[],
                &[],
                &[],
                &[],
                &[],
            )
            .unwrap();

        let content = fs::read_to_string(&result.path).unwrap();

        assert!(content.contains("# Empty Thread"));
        assert!(content.contains("## Summary"));
        assert!(content.contains("No activity yet."));
        // Empty sections should be omitted
        assert!(!content.contains("## Key Decisions"));
        assert!(!content.contains("## Findings"));
        assert!(!content.contains("## Entities"));
        assert!(!content.contains("## Active Goals"));
        assert!(!content.contains("## Decision History"));
    }

    #[test]
    fn needs_regeneration_when_never_generated() {
        let dir = tempfile::tempdir().unwrap();
        let gen = ThreadMemoryDocGenerator::new(dir.path());
        assert!(gen.needs_regeneration("thread-x", None, 1000, 3600));
    }

    #[test]
    fn needs_regeneration_when_stale() {
        let dir = tempfile::tempdir().unwrap();
        let gen = ThreadMemoryDocGenerator::new(dir.path());
        // Generated 2 hours ago, max age is 1 hour
        assert!(gen.needs_regeneration("thread-x", Some(1000), 8200, 3600));
    }

    #[test]
    fn no_regeneration_when_fresh() {
        let dir = tempfile::tempdir().unwrap();
        let gen = ThreadMemoryDocGenerator::new(dir.path());
        // Generated 30 min ago, max age is 1 hour
        assert!(!gen.needs_regeneration("thread-x", Some(1000), 2800, 3600));
    }

    #[test]
    fn doc_path_is_correct() {
        let gen = ThreadMemoryDocGenerator::new("/kb");
        assert_eq!(
            gen.doc_path("thread-saas-product"),
            PathBuf::from("/kb/threads/saas-product.md")
        );
    }

    #[test]
    fn superseded_decisions_are_omitted() {
        let dir = tempfile::tempdir().unwrap();
        let gen = ThreadMemoryDocGenerator::new(dir.path());

        let mut decision = make_memory("m1", "vue-001", "Use React for frontend", "2026-03-10");
        decision.status = MemoryStatus::Superseded;

        let result = gen
            .generate(
                "thread-test",
                "Test Thread",
                "Summary.",
                &[],
                &[decision],
                &[],
                &[],
                &[],
            )
            .unwrap();

        let content = fs::read_to_string(&result.path).unwrap();
        assert!(!content.contains("Use React for frontend"));
        assert!(!content.contains("SUPERSEDED"));
        assert!(!content.contains("## Key Decisions"));
    }

    #[test]
    fn generate_hash_is_deterministic() {
        let dir = tempfile::tempdir().unwrap();
        let gen = ThreadMemoryDocGenerator::new(dir.path());

        let result1 = gen
            .generate(
                "thread-det",
                "Det Thread",
                "Same content.",
                &[],
                &[],
                &[],
                &[],
                &[],
            )
            .unwrap();
        // Re-generate with the same input — hash must be identical.
        let result2 = gen
            .generate(
                "thread-det",
                "Det Thread",
                "Same content.",
                &[],
                &[],
                &[],
                &[],
                &[],
            )
            .unwrap();

        assert_eq!(result1.content_hash, result2.content_hash);
        assert!(!result1.content_hash.is_empty());
        // SHA-256 hex is 64 characters
        assert_eq!(result1.content_hash.len(), 64);
    }

    #[test]
    fn generate_hash_changes_with_content() {
        let dir = tempfile::tempdir().unwrap();
        let gen = ThreadMemoryDocGenerator::new(dir.path());

        let result1 = gen
            .generate(
                "thread-a",
                "Thread A",
                "Content version 1.",
                &[],
                &[],
                &[],
                &[],
                &[],
            )
            .unwrap();
        let result2 = gen
            .generate(
                "thread-a",
                "Thread A",
                "Content version 2.",
                &[],
                &[],
                &[],
                &[],
                &[],
            )
            .unwrap();

        assert_ne!(result1.content_hash, result2.content_hash);
    }

    #[test]
    fn build_memory_doc_updated_envelope_has_correct_fields() {
        use std::path::PathBuf;

        let envelope = build_memory_doc_updated_envelope(
            "thread-test",
            "abc123def456",
            &PathBuf::from("knowledge-base/threads/test.md"),
            1700000000,
            "#goals:localhost",
        );

        assert_eq!(envelope.room_id, "#goals:localhost");
        assert_eq!(envelope.envelope.sym.t.as_deref(), Some("thread-test"));
        assert!(envelope.envelope.body.contains("thread-test"));

        let detail = envelope
            .envelope
            .sym
            .d
            .as_ref()
            .expect("detail should exist");
        assert_eq!(
            detail.get("thread_id").and_then(|v| v.as_str()),
            Some("thread-test")
        );
        assert_eq!(
            detail.get("content_hash").and_then(|v| v.as_str()),
            Some("abc123def456")
        );
        assert_eq!(
            detail.get("updated_at").and_then(|v| v.as_u64()),
            Some(1700000000)
        );
        assert_eq!(
            detail.get("event_type").and_then(|v| v.as_str()),
            Some("memory_doc.updated")
        );
        assert!(detail.get("path").is_some());
    }

    // --- Staleness warning tests ---

    /// Helper to build a memory with explicit fact_type and depends_on.
    fn make_typed_memory(
        id: &str,
        fact: &str,
        date: &str,
        fact_type: Option<FactType>,
        depends_on: Vec<String>,
    ) -> Memory {
        Memory {
            id: id.to_string(),
            entity_id: "ent-1".to_string(),
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
            fact_type,
            authored_by: None,
            supersedes: None,
            depends_on,
            fsrs: None,
        }
    }

    #[test]
    fn staleness_warnings_none_when_all_fresh() {
        // Facts dated recently — everything fresh.
        let checker = StalenessChecker::default();
        let facts = vec![
            make_typed_memory(
                "f1",
                "Server uses 4GB RAM",
                "2026-03-22",
                Some(FactType::Finding),
                vec![],
            ),
            make_typed_memory(
                "d1",
                "Use PostgreSQL",
                "2026-03-20",
                Some(FactType::Decision),
                vec![],
            ),
        ];
        // "now" = 2026-03-23 => facts are 1-3 days old, well within cadences
        let now_secs = simple_epoch(2026, 3, 23, 0, 0, 0);
        let result = format_staleness_warnings_at(&facts, &checker, now_secs);
        assert!(
            result.is_none(),
            "expected no warnings for fresh facts, got: {:?}",
            result
        );
    }

    #[test]
    fn staleness_warnings_one_stale_fact() {
        // A Finding with cadence 90 days, but the fact is from >90 days ago.
        let checker = StalenessChecker::new(StalenessConfig {
            finding_cadence_days: 90,
            ..StalenessConfig::default()
        });
        let facts = vec![make_typed_memory(
            "f1",
            "API rate limit is 100/min",
            "2025-12-01T00:00:00Z",
            Some(FactType::Finding),
            vec![],
        )];
        // "now" = 2026-03-23 => ~112 days after 2025-12-01 (>90d cadence)
        let now_secs = simple_epoch(2026, 3, 23, 0, 0, 0);
        let result = format_staleness_warnings_at(&facts, &checker, now_secs);
        assert!(result.is_some(), "expected staleness warnings");
        let section = result.unwrap();
        assert!(
            section.contains("## Staleness Warnings"),
            "missing section header"
        );
        assert!(
            section.contains("API rate limit is 100/min"),
            "missing fact text"
        );
        assert!(section.contains("Stale since"), "missing 'Stale since'");
    }

    #[test]
    fn staleness_warnings_only_directly_stale() {
        // Cascading propagation removed — only directly stale facts are reported.
        // Fact A is stale by age, Fact B depends on A but is fresh by its own age.
        let checker = StalenessChecker::new(StalenessConfig {
            finding_cadence_days: 90,
            decision_cadence_days: 180,
            ..StalenessConfig::default()
        });
        let facts = vec![
            make_typed_memory(
                "f-old",
                "Competitor charges $10/month",
                "2025-10-01T00:00:00Z",
                Some(FactType::Finding),
                vec![],
            ),
            make_typed_memory(
                "d-dep",
                "Price our product at $8/month",
                "2025-12-01T00:00:00Z",
                Some(FactType::Decision),
                vec!["f-old".to_string()],
            ),
        ];
        // "now" = 2026-03-23 => f-old is ~173 days old (>90d => stale)
        // d-dep is ~112 days old (<180d => fresh by its own cadence)
        let now_secs = simple_epoch(2026, 3, 23, 0, 0, 0);
        let result = format_staleness_warnings_at(&facts, &checker, now_secs);
        assert!(result.is_some(), "expected staleness warnings");
        let section = result.unwrap();
        assert!(
            section.contains("Competitor charges $10/month"),
            "missing stale fact"
        );
        // d-dep should NOT appear — it's fresh by its own cadence
        assert!(
            !section.contains("Price our product at $8/month"),
            "fresh fact should not appear in warnings"
        );
    }

    #[test]
    fn format_staleness_warnings_output_matches_expected_markdown() {
        let checker = StalenessChecker::new(StalenessConfig {
            finding_cadence_days: 30,
            ..StalenessConfig::default()
        });
        let facts = vec![make_typed_memory(
            "f1",
            "Old fact",
            "2025-01-01T00:00:00Z",
            Some(FactType::Finding),
            vec![],
        )];
        let now_secs = simple_epoch(2026, 3, 23, 0, 0, 0);
        let result = format_staleness_warnings_at(&facts, &checker, now_secs);
        let section = result.expect("should have warnings");
        // Verify markdown structure
        assert!(
            section.starts_with("## Staleness Warnings\n"),
            "section should start with header"
        );
        assert!(
            section.contains("- **Old fact** — Stale since 2025-01-01:"),
            "should have bullet with fact and date"
        );
        // Verify the reason mentions cadence
        assert!(section.contains("cadence"), "reason should mention cadence");
    }
}
