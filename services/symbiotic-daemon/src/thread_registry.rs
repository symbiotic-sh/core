//! Persistent mapping of thread_id → Matrix room_id.
//!
//! The registry is stored as JSON at `{data_dir}/threads/registry.json`.
//! Uses atomic write (tmp file → rename) for crash safety.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Maps thread IDs to Matrix room IDs.
/// Persisted as JSON at `{data_dir}/threads/registry.json`.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ThreadRegistry {
    data_dir: PathBuf,
    map: HashMap<String, ThreadEntry>,
}

/// A single thread entry in the registry.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ThreadEntry {
    pub thread_id: String,
    pub room_id: String,
    pub title: String,
    pub created_at: u64,
    pub last_activity: u64,
    pub archived: bool,
}

#[allow(dead_code)]
impl ThreadRegistry {
    /// Create a new empty registry backed by the given data directory.
    pub fn new(data_dir: &Path) -> Self {
        Self {
            data_dir: data_dir.to_path_buf(),
            map: HashMap::new(),
        }
    }

    /// Path to the persisted registry JSON file.
    fn registry_path(&self) -> PathBuf {
        self.data_dir.join("threads").join("registry.json")
    }

    /// Load the registry from disk. Creates the directory if missing.
    pub fn load(&mut self) -> Result<(), std::io::Error> {
        let path = self.registry_path();
        if !path.exists() {
            return Ok(());
        }
        let data = std::fs::read_to_string(&path)?;
        let entries: Vec<ThreadEntry> = serde_json::from_str(&data)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
        self.map.clear();
        for entry in entries {
            self.map.insert(entry.thread_id.clone(), entry);
        }
        Ok(())
    }

    /// Persist the registry to disk using atomic write (tmp → rename).
    pub fn save(&self) -> Result<(), std::io::Error> {
        let path = self.registry_path();
        let dir = path.parent().expect("registry path must have parent");
        std::fs::create_dir_all(dir)?;

        let entries: Vec<&ThreadEntry> = self.map.values().collect();
        let json = serde_json::to_string_pretty(&entries)
            .map_err(|e| std::io::Error::other(e.to_string()))?;

        // Atomic write: write to tmp file, then rename.
        let tmp_path = path.with_extension("json.tmp");
        std::fs::write(&tmp_path, json.as_bytes())?;
        std::fs::rename(&tmp_path, &path)?;
        Ok(())
    }

    /// Register a new thread entry (or overwrite an existing one).
    pub fn register(&mut self, entry: ThreadEntry) {
        self.map.insert(entry.thread_id.clone(), entry);
    }

    /// Look up a thread entry by thread_id.
    pub fn lookup(&self, thread_id: &str) -> Option<&ThreadEntry> {
        self.map.get(thread_id)
    }

    /// Return the room_id for a given thread_id.
    pub fn room_id_for(&self, thread_id: &str) -> Option<&str> {
        self.map.get(thread_id).map(|e| e.room_id.as_str())
    }

    /// Find the thread entry associated with a given room_id.
    pub fn thread_for_room(&self, room_id: &str) -> Option<&ThreadEntry> {
        self.map.values().find(|e| e.room_id == room_id)
    }

    /// Update the last_activity timestamp for a thread.
    pub fn update_activity(&mut self, thread_id: &str, ts: u64) {
        if let Some(entry) = self.map.get_mut(thread_id) {
            entry.last_activity = ts;
        }
    }

    /// Mark a thread as archived.
    pub fn archive(&mut self, thread_id: &str) {
        if let Some(entry) = self.map.get_mut(thread_id) {
            entry.archived = true;
        }
    }

    /// Return all non-archived thread entries.
    pub fn list_active(&self) -> Vec<&ThreadEntry> {
        self.map.values().filter(|e| !e.archived).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_and_lookup() {
        let dir = tempfile::tempdir().unwrap();
        let mut reg = ThreadRegistry::new(dir.path());

        reg.register(ThreadEntry {
            thread_id: "thread-1".to_string(),
            room_id: "!abc:example.com".to_string(),
            title: "Test thread".to_string(),
            created_at: 1000,
            last_activity: 1000,
            archived: false,
        });

        assert!(reg.lookup("thread-1").is_some());
        assert_eq!(reg.room_id_for("thread-1"), Some("!abc:example.com"));
        assert!(reg.lookup("thread-2").is_none());
    }

    #[test]
    fn thread_for_room() {
        let dir = tempfile::tempdir().unwrap();
        let mut reg = ThreadRegistry::new(dir.path());

        reg.register(ThreadEntry {
            thread_id: "thread-1".to_string(),
            room_id: "!abc:example.com".to_string(),
            title: "Test thread".to_string(),
            created_at: 1000,
            last_activity: 1000,
            archived: false,
        });

        let entry = reg.thread_for_room("!abc:example.com").unwrap();
        assert_eq!(entry.thread_id, "thread-1");
        assert!(reg.thread_for_room("!xyz:example.com").is_none());
    }

    #[test]
    fn save_and_load() {
        let dir = tempfile::tempdir().unwrap();
        let mut reg = ThreadRegistry::new(dir.path());

        reg.register(ThreadEntry {
            thread_id: "thread-1".to_string(),
            room_id: "!abc:example.com".to_string(),
            title: "Test thread".to_string(),
            created_at: 1000,
            last_activity: 1000,
            archived: false,
        });
        reg.save().unwrap();

        let mut reg2 = ThreadRegistry::new(dir.path());
        reg2.load().unwrap();
        assert_eq!(reg2.room_id_for("thread-1"), Some("!abc:example.com"));
    }

    #[test]
    fn archive_and_list_active() {
        let dir = tempfile::tempdir().unwrap();
        let mut reg = ThreadRegistry::new(dir.path());

        reg.register(ThreadEntry {
            thread_id: "thread-1".to_string(),
            room_id: "!abc:example.com".to_string(),
            title: "Active".to_string(),
            created_at: 1000,
            last_activity: 1000,
            archived: false,
        });
        reg.register(ThreadEntry {
            thread_id: "thread-2".to_string(),
            room_id: "!def:example.com".to_string(),
            title: "Archived".to_string(),
            created_at: 1000,
            last_activity: 1000,
            archived: false,
        });

        assert_eq!(reg.list_active().len(), 2);

        reg.archive("thread-2");
        let active = reg.list_active();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].thread_id, "thread-1");
    }

    #[test]
    fn update_activity() {
        let dir = tempfile::tempdir().unwrap();
        let mut reg = ThreadRegistry::new(dir.path());

        reg.register(ThreadEntry {
            thread_id: "thread-1".to_string(),
            room_id: "!abc:example.com".to_string(),
            title: "Test".to_string(),
            created_at: 1000,
            last_activity: 1000,
            archived: false,
        });

        reg.update_activity("thread-1", 2000);
        assert_eq!(reg.lookup("thread-1").unwrap().last_activity, 2000);
    }

    #[test]
    fn load_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        let mut reg = ThreadRegistry::new(dir.path());
        // Should succeed even when no file exists.
        reg.load().unwrap();
        assert!(reg.list_active().is_empty());
    }
}
