use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use symbiotic_core::now_unix;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueueKind {
    Active,
    Backlog,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DomainTask {
    pub id: String,
    pub title: String,
    pub goal: Option<String>,
    pub stream: Option<String>,
    pub status: String,
    pub priority: u8,
    pub assignee: Option<String>,
    pub created_at: u64,
    pub due_at: Option<u64>,
    pub context: Value,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DomainQueue {
    pub tasks: Vec<DomainTask>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DomainQueueMeta {
    pub domain: String,
    pub updated_at: u64,
    pub total_active: usize,
    pub total_backlog: usize,
}

#[derive(Debug, Error)]
pub enum DomainQueueError {
    #[error("domain not found: {0}")]
    DomainNotFound(String),
    #[error("task not found: {0}")]
    TaskNotFound(String),
}

pub struct DomainQueueStore {
    root: PathBuf,
}

impl DomainQueueStore {
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        fs::create_dir_all(&root)
            .with_context(|| format!("failed to create domains root {}", root.display()))?;
        Ok(Self { root })
    }

    pub fn ensure_domain(&self, domain: &str) -> Result<DomainQueuePaths> {
        let paths = self.paths_for(domain);
        fs::create_dir_all(&paths.queue_dir).with_context(|| {
            format!(
                "failed to create domain queue {}",
                paths.queue_dir.display()
            )
        })?;
        if !paths.active_file.exists() {
            write_queue(&paths.active_file, &DomainQueue::default())?;
        }
        if !paths.backlog_file.exists() {
            write_queue(&paths.backlog_file, &DomainQueue::default())?;
        }
        if !paths.meta_file.exists() {
            let meta = DomainQueueMeta {
                domain: domain.to_string(),
                updated_at: now_unix(),
                total_active: 0,
                total_backlog: 0,
            };
            write_meta(&paths.meta_file, &meta)?;
        }
        Ok(paths)
    }

    pub fn list_domains(&self) -> Result<Vec<String>> {
        let mut out = Vec::new();
        for entry in fs::read_dir(&self.root)
            .with_context(|| format!("failed to read domains root {}", self.root.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                if let Some(name) = path.file_name().and_then(|value| value.to_str()) {
                    out.push(name.to_string());
                }
            }
        }
        out.sort();
        Ok(out)
    }

    pub fn list_tasks(&self, domain: &str, kind: Option<QueueKind>) -> Result<Vec<DomainTask>> {
        let paths = self.ensure_domain(domain)?;
        let mut tasks = Vec::new();
        match kind {
            Some(QueueKind::Active) => tasks.extend(read_queue(&paths.active_file)?.tasks),
            Some(QueueKind::Backlog) => tasks.extend(read_queue(&paths.backlog_file)?.tasks),
            None => {
                tasks.extend(read_queue(&paths.active_file)?.tasks);
                tasks.extend(read_queue(&paths.backlog_file)?.tasks);
            }
        }
        tasks.sort_by(|a, b| (a.priority, a.created_at).cmp(&(b.priority, b.created_at)));
        Ok(tasks)
    }

    pub fn list_all_tasks(&self, kind: Option<QueueKind>) -> Result<Vec<(String, DomainTask)>> {
        let mut out = Vec::new();
        for domain in self.list_domains()? {
            let tasks = self.list_tasks(&domain, kind.clone())?;
            for task in tasks {
                out.push((domain.clone(), task));
            }
        }
        Ok(out)
    }

    pub fn add_task(&self, domain: &str, task: DomainTask, kind: QueueKind) -> Result<DomainTask> {
        let paths = self.ensure_domain(domain)?;
        let mut queue = read_queue(match kind {
            QueueKind::Active => &paths.active_file,
            QueueKind::Backlog => &paths.backlog_file,
        })?;
        queue.tasks.push(task.clone());
        write_queue(
            match kind {
                QueueKind::Active => &paths.active_file,
                QueueKind::Backlog => &paths.backlog_file,
            },
            &queue,
        )?;
        self.refresh_meta(domain)?;
        Ok(task)
    }

    pub fn update_status(&self, domain: &str, task_id: &str, status: &str) -> Result<DomainTask> {
        let paths = self.ensure_domain(domain)?;
        let mut active = read_queue(&paths.active_file)?;
        let mut backlog = read_queue(&paths.backlog_file)?;
        let mut found = None;

        for task in &mut active.tasks {
            if task.id == task_id {
                task.status = status.to_string();
                found = Some(task.clone());
                break;
            }
        }
        if found.is_none() {
            for task in &mut backlog.tasks {
                if task.id == task_id {
                    task.status = status.to_string();
                    found = Some(task.clone());
                    break;
                }
            }
        }

        if let Some(task) = found {
            write_queue(&paths.active_file, &active)?;
            write_queue(&paths.backlog_file, &backlog)?;
            self.refresh_meta(domain)?;
            return Ok(task);
        }
        Err(DomainQueueError::TaskNotFound(task_id.to_string()).into())
    }

    pub fn assign_task(&self, domain: &str, task_id: &str, assignee: &str) -> Result<DomainTask> {
        let paths = self.ensure_domain(domain)?;
        let mut active = read_queue(&paths.active_file)?;
        let mut backlog = read_queue(&paths.backlog_file)?;
        let mut found = None;

        for task in &mut active.tasks {
            if task.id == task_id {
                task.assignee = Some(assignee.to_string());
                found = Some(task.clone());
                break;
            }
        }
        if found.is_none() {
            for task in &mut backlog.tasks {
                if task.id == task_id {
                    task.assignee = Some(assignee.to_string());
                    found = Some(task.clone());
                    break;
                }
            }
        }

        if let Some(task) = found {
            write_queue(&paths.active_file, &active)?;
            write_queue(&paths.backlog_file, &backlog)?;
            self.refresh_meta(domain)?;
            return Ok(task);
        }
        Err(DomainQueueError::TaskNotFound(task_id.to_string()).into())
    }

    pub fn promote_task(&self, domain: &str, task_id: &str) -> Result<DomainTask> {
        let paths = self.ensure_domain(domain)?;
        let mut active = read_queue(&paths.active_file)?;
        let mut backlog = read_queue(&paths.backlog_file)?;
        if let Some(idx) = backlog.tasks.iter().position(|task| task.id == task_id) {
            let task = backlog.tasks.remove(idx);
            active.tasks.push(task.clone());
            write_queue(&paths.active_file, &active)?;
            write_queue(&paths.backlog_file, &backlog)?;
            self.refresh_meta(domain)?;
            return Ok(task);
        }
        Err(DomainQueueError::TaskNotFound(task_id.to_string()).into())
    }

    pub fn refresh_meta(&self, domain: &str) -> Result<()> {
        let paths = self.ensure_domain(domain)?;
        let active = read_queue(&paths.active_file)?;
        let backlog = read_queue(&paths.backlog_file)?;
        let meta = DomainQueueMeta {
            domain: domain.to_string(),
            updated_at: now_unix(),
            total_active: active.tasks.len(),
            total_backlog: backlog.tasks.len(),
        };
        write_meta(&paths.meta_file, &meta)
    }

    pub fn generate_task_id(&self, domain: &str) -> String {
        format!("{}-{}", domain, now_unix())
    }

    fn paths_for(&self, domain: &str) -> DomainQueuePaths {
        let dir = self.root.join(domain).join("queue");
        DomainQueuePaths {
            queue_dir: dir.clone(),
            active_file: dir.join("active.json"),
            backlog_file: dir.join("backlog.json"),
            meta_file: dir.join("meta.json"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct DomainQueuePaths {
    pub queue_dir: PathBuf,
    pub active_file: PathBuf,
    pub backlog_file: PathBuf,
    pub meta_file: PathBuf,
}

fn read_queue(path: &Path) -> Result<DomainQueue> {
    if !path.exists() {
        return Ok(DomainQueue::default());
    }
    let content =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    if content.trim().is_empty() {
        return Ok(DomainQueue::default());
    }
    let queue = serde_json::from_str(&content)
        .with_context(|| format!("invalid queue file {}", path.display()))?;
    Ok(queue)
}

fn write_queue(path: &Path, queue: &DomainQueue) -> Result<()> {
    let content = serde_json::to_string_pretty(queue)?;
    let tmp = path.with_extension("tmp");
    let mut file =
        fs::File::create(&tmp).with_context(|| format!("failed to write {}", tmp.display()))?;
    file.write_all(content.as_bytes())
        .with_context(|| format!("failed to write {}", tmp.display()))?;
    file.flush()
        .with_context(|| format!("failed to flush {}", tmp.display()))?;
    fs::rename(&tmp, path).with_context(|| format!("failed to replace {}", path.display()))?;
    Ok(())
}

fn write_meta(path: &Path, meta: &DomainQueueMeta) -> Result<()> {
    let content = serde_json::to_string_pretty(meta)?;
    let tmp = path.with_extension("tmp");
    let mut file =
        fs::File::create(&tmp).with_context(|| format!("failed to write {}", tmp.display()))?;
    file.write_all(content.as_bytes())
        .with_context(|| format!("failed to write {}", tmp.display()))?;
    file.flush()
        .with_context(|| format!("failed to flush {}", tmp.display()))?;
    fs::rename(&tmp, path).with_context(|| format!("failed to replace {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn unique_suffix() -> String {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        format!("{}_{}_{}", now_unix(), std::process::id(), id)
    }

    #[test]
    fn domain_store_adds_and_lists_tasks() {
        let root = std::env::temp_dir().join(format!("symbiotic_domains_{}", unique_suffix()));
        let store = DomainQueueStore::open(&root).expect("store should open");
        let task = DomainTask {
            id: store.generate_task_id("marketing"),
            title: "Write blog post".to_string(),
            goal: Some("build-business".to_string()),
            stream: Some("marketing".to_string()),
            status: "pending".to_string(),
            priority: 1,
            assignee: None,
            created_at: now_unix(),
            due_at: None,
            context: json!({"keywords": ["ai"]}),
        };
        store
            .add_task("marketing", task.clone(), QueueKind::Active)
            .expect("add should work");
        let tasks = store
            .list_tasks("marketing", None)
            .expect("list should work");
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].title, "Write blog post");
    }
}
