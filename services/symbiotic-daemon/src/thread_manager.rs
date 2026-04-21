//! Manages thread lifecycle: creation, archival, routing.
//!
//! Does NOT interact with Matrix directly — it produces `DaemonEvent`s
//! that the existing event loop sends. The `ThreadRegistry` handles
//! persistence of thread_id → room_id mappings.

use crate::events::{DaemonEvent, EventType};
use crate::thread_registry::{ThreadEntry, ThreadRegistry};

/// Manages thread lifecycle: creation, archival, routing.
#[allow(dead_code)]
pub struct ThreadManager {
    registry: ThreadRegistry,
}

#[allow(dead_code)]
impl ThreadManager {
    /// Create a new ThreadManager wrapping the given registry.
    pub fn new(registry: ThreadRegistry) -> Self {
        Self { registry }
    }

    /// Create a new thread. Returns the `routing.created` event to emit.
    ///
    /// The `slug` is used to form the thread_id (e.g., "thread-{slug}").
    /// The `room_id` is the Matrix room backing this thread.
    pub fn create_thread(
        &mut self,
        slug: &str,
        title: &str,
        room_id: &str,
        now: u64,
    ) -> DaemonEvent {
        let thread_id = format!("thread-{slug}");

        self.registry.register(ThreadEntry {
            thread_id: thread_id.clone(),
            room_id: room_id.to_string(),
            title: title.to_string(),
            created_at: now,
            last_activity: now,
            archived: false,
        });

        DaemonEvent {
            event_type: EventType::RoutingCreated,
            status: "completed".to_string(),
            job_id: None,
            detail: format!("thread_id={thread_id} room_id={room_id} title={title}"),
            goal_room: None,
            goal_template: None,
            goal_run_id: None,
            goal_id: None,
            intake_run_id: None,
            url: None,
            title: Some(title.to_string()),
            sensitivity: None,
            quick_replies: None,
            thread_id: Some(thread_id),
        }
    }

    /// Look up which room a thread_id maps to.
    pub fn room_for_thread(&self, thread_id: &str) -> Option<&str> {
        self.registry.room_id_for(thread_id)
    }

    /// Look up which thread a room_id maps to.
    pub fn thread_for_room(&self, room_id: &str) -> Option<&ThreadEntry> {
        self.registry.thread_for_room(room_id)
    }

    /// Mark a thread as archived. Returns a `routing.archived` event if the
    /// thread exists, or `None` if the thread_id was not found.
    pub fn archive_thread(&mut self, thread_id: &str) -> Option<DaemonEvent> {
        let entry = self.registry.lookup(thread_id)?;
        let title = entry.title.clone();

        self.registry.archive(thread_id);

        Some(DaemonEvent {
            event_type: EventType::RoutingArchived,
            status: "completed".to_string(),
            job_id: None,
            detail: format!("thread_id={thread_id} archived=true"),
            goal_room: None,
            goal_template: None,
            goal_run_id: None,
            goal_id: None,
            intake_run_id: None,
            url: None,
            title: Some(title),
            sensitivity: None,
            quick_replies: None,
            thread_id: Some(thread_id.to_string()),
        })
    }

    /// Update last activity timestamp for a thread.
    pub fn touch(&mut self, thread_id: &str, now: u64) {
        self.registry.update_activity(thread_id, now);
    }

    /// Get all active (non-archived) threads.
    pub fn active_threads(&self) -> Vec<&ThreadEntry> {
        self.registry.list_active()
    }

    /// Generate a thread title from the first message.
    /// Rule-based: extracts key nouns/verbs, caps at 40 chars.
    /// TODO: Replace with cheap LLM call (Haiku-tier) in Phase 2.
    pub fn generate_title(message: &str) -> String {
        let trimmed = message.trim();

        if trimmed.is_empty() {
            return String::new();
        }

        // If message is short enough, use it directly (cleaned up)
        if trimmed.len() <= 40 {
            return Self::clean_title(trimmed);
        }

        // Take first sentence or first 40 chars
        let first_sentence = trimmed
            .split(&['.', '!', '?', '\n'][..])
            .next()
            .unwrap_or(trimmed);

        if first_sentence.len() <= 40 {
            return Self::clean_title(first_sentence);
        }

        // Truncate at word boundary
        let truncated: String = first_sentence.chars().take(40).collect();
        match truncated.rfind(' ') {
            Some(pos) if pos > 10 => format!("{}...", &truncated[..pos]),
            _ => format!("{}...", truncated),
        }
    }

    /// Derive a URL-safe slug from a user's description.
    ///
    /// Lowercase, alphanumeric + dashes, max 20 chars, with a 4-hex timestamp
    /// suffix for uniqueness. Used as the Matrix room alias local part
    /// (e.g., `thread-build-a-website-a1f3`).
    pub fn generate_slug(description: &str) -> String {
        let title = Self::generate_title(description);
        let raw: String = title
            .to_lowercase()
            .chars()
            .map(|c| if c.is_alphanumeric() { c } else { '-' })
            .collect();
        // Collapse consecutive dashes
        let mut slug = String::new();
        for c in raw.chars() {
            if c == '-' && slug.ends_with('-') {
                continue;
            }
            slug.push(c);
        }
        let slug = slug.trim_matches('-');
        let slug = &slug[..slug.len().min(20)];
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        format!("{}-{:04x}", slug, ts % 0xFFFF)
    }

    /// Clean a raw string into a title: strip command prefixes, trim, capitalize.
    fn clean_title(s: &str) -> String {
        // Remove leading command-like prefixes
        let s = s.trim_start_matches(['/', '!', '>']);
        let s = s.trim();
        // Capitalize first letter
        let mut chars = s.chars();
        match chars.next() {
            None => String::new(),
            Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
        }
    }

    /// Split a thread: create a child thread with extracted context.
    /// Returns (routing.split event for parent, routing.created event for stream).
    pub fn split_thread(
        &mut self,
        parent_thread_id: &str,
        child_slug: &str,
        child_title: &str,
        child_room_id: &str,
        context_summary: &str,
        now: u64,
    ) -> Option<(DaemonEvent, DaemonEvent)> {
        // Verify parent exists
        self.registry.lookup(parent_thread_id)?;

        let child_thread_id = format!("thread-{child_slug}");

        // Register child thread
        self.registry.register(ThreadEntry {
            thread_id: child_thread_id.clone(),
            room_id: child_room_id.to_string(),
            title: child_title.to_string(),
            created_at: now,
            last_activity: now,
            archived: false,
        });

        // Split event for parent thread
        let split_event = DaemonEvent {
            event_type: EventType::RoutingSplit,
            status: "completed".to_string(),
            job_id: None,
            detail: format!(
                "parent={parent_thread_id} child={child_thread_id} context={context_summary}"
            ),
            goal_room: None,
            goal_template: None,
            goal_run_id: None,
            goal_id: None,
            intake_run_id: None,
            url: None,
            title: Some(child_title.to_string()),
            sensitivity: None,
            quick_replies: None,
            thread_id: Some(parent_thread_id.to_string()),
        };

        // Created event for stream
        let created_event = DaemonEvent {
            event_type: EventType::RoutingCreated,
            status: "completed".to_string(),
            job_id: None,
            detail: format!(
                "thread_id={child_thread_id} room_id={child_room_id} title={child_title} split_from={parent_thread_id}"
            ),
            goal_room: None,
            goal_template: None,
            goal_run_id: None,
            goal_id: None,
            intake_run_id: None,
            url: None,
            title: Some(child_title.to_string()),
            sensitivity: None,
            quick_replies: None,
            thread_id: Some(child_thread_id),
        };

        Some((split_event, created_event))
    }

    /// Merge source thread into target thread.
    /// Archives the source and returns a routing.moved event.
    pub fn merge_threads(
        &mut self,
        source_thread_id: &str,
        target_thread_id: &str,
        now: u64,
    ) -> Option<DaemonEvent> {
        // Both threads must exist
        let source = self.registry.lookup(source_thread_id)?;
        let source_title = source.title.clone();
        let target = self.registry.lookup(target_thread_id)?;
        let target_title = target.title.clone();

        // Archive the source thread
        self.registry.archive(source_thread_id);
        // Touch the target to update activity
        self.registry.update_activity(target_thread_id, now);

        Some(DaemonEvent {
            event_type: EventType::RoutingMoved,
            status: "completed".to_string(),
            job_id: None,
            detail: format!(
                "merged {source_thread_id} ({source_title}) into {target_thread_id} ({target_title})"
            ),
            goal_room: None,
            goal_template: None,
            goal_run_id: None,
            goal_id: None,
            intake_run_id: None,
            url: None,
            title: Some(target_title),
            sensitivity: None,
            quick_replies: None,
            thread_id: Some(target_thread_id.to_string()),
        })
    }

    /// Find threads idle longer than `idle_threshold_secs` and archive them.
    /// Returns list of archive events.
    pub fn auto_archive(&mut self, now: u64, idle_threshold_secs: u64) -> Vec<DaemonEvent> {
        let idle_threads: Vec<String> = self
            .registry
            .list_active()
            .iter()
            .filter(|t| now.saturating_sub(t.last_activity) > idle_threshold_secs)
            .map(|t| t.thread_id.clone())
            .collect();

        idle_threads
            .iter()
            .filter_map(|tid| self.archive_thread(tid))
            .collect()
    }

    /// Persist registry to disk.
    pub fn save(&self) -> Result<(), std::io::Error> {
        self.registry.save()
    }

    /// Load registry from disk.
    pub fn load(&mut self) -> Result<(), std::io::Error> {
        self.registry.load()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::thread_registry::ThreadRegistry;

    fn make_manager() -> (tempfile::TempDir, ThreadManager) {
        let dir = tempfile::tempdir().unwrap();
        let registry = ThreadRegistry::new(dir.path());
        let manager = ThreadManager::new(registry);
        (dir, manager)
    }

    #[test]
    fn create_thread_returns_routing_created_event() {
        let (_dir, mut mgr) = make_manager();
        let event = mgr.create_thread("test-slug", "My Thread", "!room:example.com", 1000);

        assert_eq!(event.event_type, EventType::RoutingCreated);
        assert_eq!(event.status, "completed");
        assert_eq!(event.thread_id, Some("thread-test-slug".to_string()));
        assert!(event.detail.contains("thread-test-slug"));
    }

    #[test]
    fn room_for_thread_after_create() {
        let (_dir, mut mgr) = make_manager();
        mgr.create_thread("abc", "Title", "!room:ex.com", 1000);

        assert_eq!(mgr.room_for_thread("thread-abc"), Some("!room:ex.com"));
        assert!(mgr.room_for_thread("thread-xyz").is_none());
    }

    #[test]
    fn thread_for_room_after_create() {
        let (_dir, mut mgr) = make_manager();
        mgr.create_thread("abc", "Title", "!room:ex.com", 1000);

        let entry = mgr.thread_for_room("!room:ex.com").unwrap();
        assert_eq!(entry.thread_id, "thread-abc");
    }

    #[test]
    fn archive_thread_returns_event() {
        let (_dir, mut mgr) = make_manager();
        mgr.create_thread("abc", "Title", "!room:ex.com", 1000);

        let event = mgr.archive_thread("thread-abc").unwrap();
        assert_eq!(event.event_type, EventType::RoutingArchived);
        assert_eq!(event.thread_id, Some("thread-abc".to_string()));

        // After archival, active_threads should be empty.
        assert!(mgr.active_threads().is_empty());
    }

    #[test]
    fn archive_unknown_thread_returns_none() {
        let (_dir, mut mgr) = make_manager();
        assert!(mgr.archive_thread("thread-nope").is_none());
    }

    #[test]
    fn touch_updates_activity() {
        let (_dir, mut mgr) = make_manager();
        mgr.create_thread("abc", "Title", "!room:ex.com", 1000);
        mgr.touch("thread-abc", 2000);

        let entry = mgr.thread_for_room("!room:ex.com").unwrap();
        assert_eq!(entry.last_activity, 2000);
    }

    #[test]
    fn save_and_load_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        {
            let registry = ThreadRegistry::new(dir.path());
            let mut mgr = ThreadManager::new(registry);
            mgr.create_thread("abc", "Title", "!room:ex.com", 1000);
            mgr.save().unwrap();
        }
        {
            let registry = ThreadRegistry::new(dir.path());
            let mut mgr = ThreadManager::new(registry);
            mgr.load().unwrap();
            assert_eq!(mgr.room_for_thread("thread-abc"), Some("!room:ex.com"));
        }
    }

    // --- Title generation tests ---

    #[test]
    fn title_short_message_used_directly() {
        let title = ThreadManager::generate_title("fix the login bug");
        assert_eq!(title, "Fix the login bug");
    }

    #[test]
    fn title_strips_command_prefix() {
        let title = ThreadManager::generate_title("/deploy the service");
        assert_eq!(title, "Deploy the service");
    }

    #[test]
    fn title_strips_bang_prefix() {
        let title = ThreadManager::generate_title("!restart all workers");
        assert_eq!(title, "Restart all workers");
    }

    #[test]
    fn title_truncates_at_first_sentence() {
        let msg = "Investigate the memory leak. It has been causing OOM kills in production for the last week.";
        let title = ThreadManager::generate_title(msg);
        assert_eq!(title, "Investigate the memory leak");
    }

    #[test]
    fn title_truncates_long_sentence_at_word_boundary() {
        let msg = "This is a very long first sentence that exceeds forty characters by quite a significant margin";
        let title = ThreadManager::generate_title(msg);
        assert!(title.len() <= 44); // 40 chars + "..."
        assert!(title.ends_with("..."));
    }

    #[test]
    fn title_empty_message_returns_empty() {
        let title = ThreadManager::generate_title("");
        assert_eq!(title, "");
    }

    #[test]
    fn title_whitespace_only_returns_empty() {
        let title = ThreadManager::generate_title("   \n\t  ");
        assert_eq!(title, "");
    }

    #[test]
    fn title_capitalizes_first_letter() {
        let title = ThreadManager::generate_title("deploy services");
        assert_eq!(title, "Deploy services");
    }

    // --- Split thread tests ---

    #[test]
    fn split_thread_creates_child_and_returns_events() {
        let (_dir, mut mgr) = make_manager();
        mgr.create_thread("parent", "Parent Thread", "!parent:ex.com", 1000);

        let result = mgr.split_thread(
            "thread-parent",
            "child",
            "Child Thread",
            "!child:ex.com",
            "Extracted discussion about X",
            2000,
        );

        assert!(result.is_some());
        let (split_event, created_event) = result.unwrap();

        // Split event targets parent
        assert_eq!(split_event.event_type, EventType::RoutingSplit);
        assert_eq!(split_event.thread_id, Some("thread-parent".to_string()));
        assert!(split_event.detail.contains("thread-child"));

        // Created event targets child
        assert_eq!(created_event.event_type, EventType::RoutingCreated);
        assert_eq!(created_event.thread_id, Some("thread-child".to_string()));
        assert!(created_event.detail.contains("split_from=thread-parent"));

        // Child thread should be registered
        assert_eq!(mgr.room_for_thread("thread-child"), Some("!child:ex.com"));
    }

    #[test]
    fn split_thread_with_unknown_parent_returns_none() {
        let (_dir, mut mgr) = make_manager();
        let result = mgr.split_thread(
            "thread-nonexistent",
            "child",
            "Child",
            "!room:ex.com",
            "context",
            1000,
        );
        assert!(result.is_none());
    }

    // --- Merge thread tests ---

    #[test]
    fn merge_threads_archives_source() {
        let (_dir, mut mgr) = make_manager();
        mgr.create_thread("source", "Source Thread", "!src:ex.com", 1000);
        mgr.create_thread("target", "Target Thread", "!tgt:ex.com", 1000);

        let event = mgr.merge_threads("thread-source", "thread-target", 2000);
        assert!(event.is_some());

        let event = event.unwrap();
        assert_eq!(event.event_type, EventType::RoutingMoved);
        assert_eq!(event.thread_id, Some("thread-target".to_string()));
        assert!(event.detail.contains("merged"));

        // Source should be archived, target still active
        let active = mgr.active_threads();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].thread_id, "thread-target");
        // Target activity should be updated
        assert_eq!(active[0].last_activity, 2000);
    }

    #[test]
    fn merge_unknown_source_returns_none() {
        let (_dir, mut mgr) = make_manager();
        mgr.create_thread("target", "Target", "!tgt:ex.com", 1000);
        assert!(mgr
            .merge_threads("thread-nope", "thread-target", 2000)
            .is_none());
    }

    #[test]
    fn merge_unknown_target_returns_none() {
        let (_dir, mut mgr) = make_manager();
        mgr.create_thread("source", "Source", "!src:ex.com", 1000);
        assert!(mgr
            .merge_threads("thread-source", "thread-nope", 2000)
            .is_none());
    }

    // --- Auto-archive tests ---

    #[test]
    fn auto_archive_archives_idle_threads() {
        let (_dir, mut mgr) = make_manager();
        mgr.create_thread("active", "Active Thread", "!a:ex.com", 1000);
        mgr.create_thread("idle", "Idle Thread", "!b:ex.com", 500);

        // At now=2000 with threshold=1000:
        // "active" last_activity=1000, idle for 1000s — NOT exceeding threshold
        // "idle" last_activity=500, idle for 1500s — exceeds threshold
        let events = mgr.auto_archive(2000, 1000);

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, EventType::RoutingArchived);
        assert_eq!(events[0].thread_id, Some("thread-idle".to_string()));

        // "active" should still be active
        let active = mgr.active_threads();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].thread_id, "thread-active");
    }

    #[test]
    fn auto_archive_no_idle_threads_returns_empty() {
        let (_dir, mut mgr) = make_manager();
        mgr.create_thread("a", "Thread A", "!a:ex.com", 1000);
        mgr.create_thread("b", "Thread B", "!b:ex.com", 900);

        let events = mgr.auto_archive(1500, 1000);
        assert!(events.is_empty());
    }

    #[test]
    fn auto_archive_all_idle() {
        let (_dir, mut mgr) = make_manager();
        mgr.create_thread("a", "Thread A", "!a:ex.com", 100);
        mgr.create_thread("b", "Thread B", "!b:ex.com", 200);

        let events = mgr.auto_archive(5000, 1000);
        assert_eq!(events.len(), 2);
        assert!(mgr.active_threads().is_empty());
    }
}
