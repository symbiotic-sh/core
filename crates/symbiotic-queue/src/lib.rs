use std::collections::hash_map::DefaultHasher;
use std::fs;
use std::hash::{Hash, Hasher};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{anyhow, Context, Result};
use symbiotic_core::{harden_dir_permissions, harden_file_permissions};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobStatus {
    Queued,
    Running,
    Failed,
    Done,
    Dlq,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueJob {
    pub job_id: String,
    pub type_name: String,
    pub payload: String,
    pub status: JobStatus,
    pub attempts: u32,
    pub max_attempts: u32,
    pub next_run_at: u64,
    pub idempotency_key: String,
    pub last_error: Option<String>,
    pub lease_owner: Option<String>,
    pub lease_expires_at: Option<u64>,
    pub created_at: u64,
    pub updated_at: u64,
}

#[derive(Debug, Clone)]
pub struct EnqueueRequest {
    pub type_name: String,
    pub payload: String,
    pub idempotency_key: String,
    pub max_attempts: u32,
    pub next_run_at: u64,
    /// When true, re-enqueue even if a terminal duplicate (Done/Dlq) exists.
    pub force: bool,
}

/// What happened when we tried to enqueue a job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnqueueDisposition {
    /// New job was inserted.
    Inserted,
    /// A job with this idempotency key is still active (Queued/Running/Failed).
    ActiveDuplicate { status: JobStatus },
    /// A job with this idempotency key already completed or was dead-lettered.
    /// Caller should decide: re-enqueue (with `force: true`) or skip.
    TerminalDuplicate { status: JobStatus },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnqueueOutcome {
    pub job_id: String,
    pub disposition: EnqueueDisposition,
}

impl EnqueueOutcome {
    /// Convenience: true if a new job was actually inserted.
    pub fn inserted(&self) -> bool {
        matches!(self.disposition, EnqueueDisposition::Inserted)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailOutcome {
    RetryScheduled,
    MovedToDlq,
}

#[derive(Debug, Error)]
pub enum QueueError {
    #[error("job not found: {0}")]
    JobNotFound(String),
    #[error("lease owner mismatch for job: {0}")]
    LeaseOwnerMismatch(String),
    #[error("job is not running: {0}")]
    JobNotRunning(String),
    #[error("invalid queue state: {0}")]
    InvalidState(String),
    #[error("lock poisoned")]
    LockPoisoned,
}

pub trait QueueBackend: Send + Sync {
    fn enqueue(&self, request: EnqueueRequest) -> Result<EnqueueOutcome>;
    fn lease_next(
        &self,
        worker_id: &str,
        now: u64,
        lease_seconds: u64,
        type_filter: Option<&[String]>,
    ) -> Result<Option<QueueJob>>;
    fn heartbeat(&self, job_id: &str, worker_id: &str, now: u64, lease_seconds: u64) -> Result<()>;
    fn ack(&self, job_id: &str, worker_id: &str, now: u64) -> Result<()>;
    fn fail(
        &self,
        job_id: &str,
        worker_id: &str,
        now: u64,
        backoff_seconds: u64,
        error: &str,
    ) -> Result<FailOutcome>;
    fn reclaim_expired_leases(&self, now: u64) -> Result<usize>;
    fn get(&self, job_id: &str) -> Result<Option<QueueJob>>;
    fn list_by_status(&self, status: JobStatus) -> Result<Vec<QueueJob>>;
}

#[derive(Debug)]
pub struct FileQueueStore {
    state_file: PathBuf,
    state: Mutex<QueueState>,
}

#[derive(Debug, Default)]
struct QueueState {
    jobs: Vec<QueueJob>,
}

impl FileQueueStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let state_file = path.as_ref().to_path_buf();
        if let Some(parent) = state_file.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!(
                    "failed to create queue state directory {}",
                    parent.to_string_lossy()
                )
            })?;
            // Best-effort: parent may be a system directory we cannot chmod.
            let _ = harden_dir_permissions(parent, 0o700);
        }

        if !state_file.exists() {
            fs::File::create(&state_file).with_context(|| {
                format!(
                    "failed to create queue state file {}",
                    state_file.to_string_lossy()
                )
            })?;
            harden_file_permissions(&state_file, 0o600).with_context(|| {
                format!(
                    "failed to harden queue state file {}",
                    state_file.to_string_lossy()
                )
            })?;
        }

        let state = load_state(&state_file)?;
        Ok(Self {
            state_file,
            state: Mutex::new(state),
        })
    }
}

impl QueueBackend for FileQueueStore {
    fn enqueue(&self, request: EnqueueRequest) -> Result<EnqueueOutcome> {
        let mut state = self.state.lock().map_err(|_| QueueError::LockPoisoned)?;

        // Check for existing job with same idempotency key.
        if let Some(existing) = state
            .jobs
            .iter()
            .find(|job| job.idempotency_key == request.idempotency_key)
        {
            let status = existing.status;
            let job_id = existing.job_id.clone();

            match status {
                // Active job — always deduplicate.
                JobStatus::Queued | JobStatus::Running | JobStatus::Failed => {
                    return Ok(EnqueueOutcome {
                        job_id,
                        disposition: EnqueueDisposition::ActiveDuplicate { status },
                    });
                }
                // Terminal job — report back, unless force is set.
                JobStatus::Done | JobStatus::Dlq => {
                    if !request.force {
                        return Ok(EnqueueOutcome {
                            job_id,
                            disposition: EnqueueDisposition::TerminalDuplicate { status },
                        });
                    }
                    // force=true: fall through to insert a new job.
                }
            }
        }

        let now = now_unix();
        let job_id = generate_job_id(&request.idempotency_key, state.jobs.len());
        state.jobs.push(QueueJob {
            job_id: job_id.clone(),
            type_name: request.type_name,
            payload: request.payload,
            status: JobStatus::Queued,
            attempts: 0,
            max_attempts: request.max_attempts.max(1),
            next_run_at: request.next_run_at,
            idempotency_key: request.idempotency_key,
            last_error: None,
            lease_owner: None,
            lease_expires_at: None,
            created_at: now,
            updated_at: now,
        });
        persist_state(&self.state_file, &state)?;

        Ok(EnqueueOutcome {
            job_id,
            disposition: EnqueueDisposition::Inserted,
        })
    }

    fn lease_next(
        &self,
        worker_id: &str,
        now: u64,
        lease_seconds: u64,
        type_filter: Option<&[String]>,
    ) -> Result<Option<QueueJob>> {
        let mut state = self.state.lock().map_err(|_| QueueError::LockPoisoned)?;
        reclaim_expired_in_state(&mut state.jobs, now);

        let mut candidates: Vec<(usize, &QueueJob)> = state
            .jobs
            .iter()
            .enumerate()
            .filter(|(_, job)| {
                (job.status == JobStatus::Queued || job.status == JobStatus::Failed)
                    && job.next_run_at <= now
            })
            .filter(|(_, job)| {
                if let Some(filter) = type_filter {
                    filter.iter().any(|kind| kind == &job.type_name)
                } else {
                    true
                }
            })
            .collect();
        candidates.sort_by_key(|(_, job)| (job.next_run_at, job.created_at));

        let Some((idx, _)) = candidates.first().copied() else {
            persist_state(&self.state_file, &state)?;
            return Ok(None);
        };

        let leased_job = {
            let job = state
                .jobs
                .get_mut(idx)
                .ok_or_else(|| anyhow!("queue candidate index out of bounds"))?;
            job.status = JobStatus::Running;
            job.lease_owner = Some(worker_id.to_string());
            job.lease_expires_at = Some(now + lease_seconds.max(1));
            job.updated_at = now;
            job.clone()
        };
        persist_state(&self.state_file, &state)?;
        Ok(Some(leased_job))
    }

    fn heartbeat(&self, job_id: &str, worker_id: &str, now: u64, lease_seconds: u64) -> Result<()> {
        let mut state = self.state.lock().map_err(|_| QueueError::LockPoisoned)?;
        let job = state
            .jobs
            .iter_mut()
            .find(|job| job.job_id == job_id)
            .ok_or_else(|| QueueError::JobNotFound(job_id.to_string()))?;

        if job.status != JobStatus::Running {
            return Err(QueueError::JobNotRunning(job_id.to_string()).into());
        }
        if job.lease_owner.as_deref() != Some(worker_id) {
            return Err(QueueError::LeaseOwnerMismatch(job_id.to_string()).into());
        }

        job.lease_expires_at = Some(now + lease_seconds.max(1));
        job.updated_at = now;
        persist_state(&self.state_file, &state)?;
        Ok(())
    }

    fn ack(&self, job_id: &str, worker_id: &str, now: u64) -> Result<()> {
        let mut state = self.state.lock().map_err(|_| QueueError::LockPoisoned)?;
        let job = state
            .jobs
            .iter_mut()
            .find(|job| job.job_id == job_id)
            .ok_or_else(|| QueueError::JobNotFound(job_id.to_string()))?;

        if job.status != JobStatus::Running {
            return Err(QueueError::JobNotRunning(job_id.to_string()).into());
        }
        if job.lease_owner.as_deref() != Some(worker_id) {
            return Err(QueueError::LeaseOwnerMismatch(job_id.to_string()).into());
        }

        job.status = JobStatus::Done;
        job.lease_owner = None;
        job.lease_expires_at = None;
        job.updated_at = now;
        persist_state(&self.state_file, &state)?;
        Ok(())
    }

    fn fail(
        &self,
        job_id: &str,
        worker_id: &str,
        now: u64,
        backoff_seconds: u64,
        error: &str,
    ) -> Result<FailOutcome> {
        let mut state = self.state.lock().map_err(|_| QueueError::LockPoisoned)?;
        let job = state
            .jobs
            .iter_mut()
            .find(|job| job.job_id == job_id)
            .ok_or_else(|| QueueError::JobNotFound(job_id.to_string()))?;

        if job.status != JobStatus::Running {
            return Err(QueueError::JobNotRunning(job_id.to_string()).into());
        }
        if job.lease_owner.as_deref() != Some(worker_id) {
            return Err(QueueError::LeaseOwnerMismatch(job_id.to_string()).into());
        }

        job.attempts = job.attempts.saturating_add(1);
        job.last_error = Some(error.to_string());
        job.updated_at = now;
        job.lease_owner = None;
        job.lease_expires_at = None;

        let outcome = if job.attempts >= job.max_attempts {
            job.status = JobStatus::Dlq;
            FailOutcome::MovedToDlq
        } else {
            job.status = JobStatus::Failed;
            job.next_run_at = now + backoff_seconds;
            FailOutcome::RetryScheduled
        };

        persist_state(&self.state_file, &state)?;
        Ok(outcome)
    }

    fn reclaim_expired_leases(&self, now: u64) -> Result<usize> {
        let mut state = self.state.lock().map_err(|_| QueueError::LockPoisoned)?;
        let reclaimed = reclaim_expired_in_state(&mut state.jobs, now);
        if reclaimed > 0 {
            persist_state(&self.state_file, &state)?;
        }
        Ok(reclaimed)
    }

    fn get(&self, job_id: &str) -> Result<Option<QueueJob>> {
        let state = self.state.lock().map_err(|_| QueueError::LockPoisoned)?;
        Ok(state.jobs.iter().find(|job| job.job_id == job_id).cloned())
    }

    fn list_by_status(&self, status: JobStatus) -> Result<Vec<QueueJob>> {
        let state = self.state.lock().map_err(|_| QueueError::LockPoisoned)?;
        let mut jobs: Vec<QueueJob> = state
            .jobs
            .iter()
            .filter(|job| job.status == status)
            .cloned()
            .collect();
        jobs.sort_by_key(|job| (job.next_run_at, job.created_at));
        Ok(jobs)
    }
}

fn reclaim_expired_in_state(jobs: &mut [QueueJob], now: u64) -> usize {
    let mut reclaimed = 0usize;
    for job in jobs.iter_mut() {
        if job.status == JobStatus::Running
            && job
                .lease_expires_at
                .map(|expires| expires <= now)
                .unwrap_or(false)
        {
            job.status = JobStatus::Queued;
            job.lease_owner = None;
            job.lease_expires_at = None;
            job.updated_at = now;
            reclaimed += 1;
        }
    }
    reclaimed
}

fn persist_state(path: &Path, state: &QueueState) -> Result<()> {
    let tmp_path = path.with_extension("tmp");
    let mut file = fs::File::create(&tmp_path)
        .with_context(|| format!("failed to create temp queue state {}", tmp_path.display()))?;

    for job in &state.jobs {
        writeln!(file, "{}", serialize_job(job))
            .with_context(|| format!("failed writing queue state {}", tmp_path.display()))?;
    }
    file.flush()
        .with_context(|| format!("failed flushing queue state {}", tmp_path.display()))?;
    harden_file_permissions(&tmp_path, 0o600)
        .with_context(|| format!("failed to harden temp queue state {}", tmp_path.display()))?;

    fs::rename(&tmp_path, path).with_context(|| {
        format!(
            "failed to replace queue state file {} from {}",
            path.display(),
            tmp_path.display()
        )
    })?;
    harden_file_permissions(path, 0o600)
        .with_context(|| format!("failed to harden queue state file {}", path.display()))?;
    Ok(())
}

fn load_state(path: &Path) -> Result<QueueState> {
    let content = fs::read_to_string(path)
        .with_context(|| format!("failed reading queue state {}", path.display()))?;
    let mut jobs = Vec::new();
    for (idx, line) in content.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let job = deserialize_job(line).with_context(|| {
            format!("invalid queue state at {} line {}", path.display(), idx + 1)
        })?;
        jobs.push(job);
    }
    Ok(QueueState { jobs })
}

fn serialize_job(job: &QueueJob) -> String {
    [
        escape_field(&job.job_id),
        escape_field(&job.type_name),
        escape_field(&job.payload),
        escape_field(job_status_to_str(job.status)),
        job.attempts.to_string(),
        job.max_attempts.to_string(),
        job.next_run_at.to_string(),
        escape_field(&job.idempotency_key),
        escape_field(job.last_error.as_deref().unwrap_or("")),
        escape_field(job.lease_owner.as_deref().unwrap_or("")),
        job.lease_expires_at.unwrap_or(0).to_string(),
        job.created_at.to_string(),
        job.updated_at.to_string(),
    ]
    .join("\t")
}

fn deserialize_job(line: &str) -> Result<QueueJob> {
    let fields: Vec<&str> = line.split('\t').collect();
    if fields.len() != 13 {
        return Err(
            QueueError::InvalidState(format!("expected 13 fields, got {}", fields.len())).into(),
        );
    }

    let status = str_to_job_status(&unescape_field(fields[3]))?;
    let lease_expires_at_raw: u64 = fields[10]
        .parse()
        .with_context(|| format!("invalid lease_expires_at value {}", fields[10]))?;

    Ok(QueueJob {
        job_id: unescape_field(fields[0]),
        type_name: unescape_field(fields[1]),
        payload: unescape_field(fields[2]),
        status,
        attempts: fields[4]
            .parse()
            .with_context(|| format!("invalid attempts value {}", fields[4]))?,
        max_attempts: fields[5]
            .parse()
            .with_context(|| format!("invalid max_attempts value {}", fields[5]))?,
        next_run_at: fields[6]
            .parse()
            .with_context(|| format!("invalid next_run_at value {}", fields[6]))?,
        idempotency_key: unescape_field(fields[7]),
        last_error: non_empty(unescape_field(fields[8])),
        lease_owner: non_empty(unescape_field(fields[9])),
        lease_expires_at: if lease_expires_at_raw == 0 {
            None
        } else {
            Some(lease_expires_at_raw)
        },
        created_at: fields[11]
            .parse()
            .with_context(|| format!("invalid created_at value {}", fields[11]))?,
        updated_at: fields[12]
            .parse()
            .with_context(|| format!("invalid updated_at value {}", fields[12]))?,
    })
}

fn escape_field(input: &str) -> String {
    input
        .replace('%', "%25")
        .replace('\t', "%09")
        .replace('\n', "%0A")
        .replace('\r', "%0D")
}

fn unescape_field(input: &str) -> String {
    let mut output = String::new();
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '%' {
            let a = chars.next();
            let b = chars.next();
            match (a, b) {
                (Some('2'), Some('5')) => output.push('%'),
                (Some('0'), Some('9')) => output.push('\t'),
                (Some('0'), Some('A')) => output.push('\n'),
                (Some('0'), Some('D')) => output.push('\r'),
                (Some(x), Some(y)) => {
                    output.push('%');
                    output.push(x);
                    output.push(y);
                }
                _ => output.push('%'),
            }
        } else {
            output.push(ch);
        }
    }
    output
}

fn non_empty(input: String) -> Option<String> {
    if input.is_empty() {
        None
    } else {
        Some(input)
    }
}

fn job_status_to_str(status: JobStatus) -> &'static str {
    match status {
        JobStatus::Queued => "queued",
        JobStatus::Running => "running",
        JobStatus::Failed => "failed",
        JobStatus::Done => "done",
        JobStatus::Dlq => "dlq",
    }
}

fn str_to_job_status(value: &str) -> Result<JobStatus> {
    match value {
        "queued" => Ok(JobStatus::Queued),
        "running" => Ok(JobStatus::Running),
        "failed" => Ok(JobStatus::Failed),
        "done" => Ok(JobStatus::Done),
        "dlq" => Ok(JobStatus::Dlq),
        other => Err(QueueError::InvalidState(format!("unknown job status {other}")).into()),
    }
}

fn generate_job_id(idempotency_key: &str, counter: usize) -> String {
    let mut hasher = DefaultHasher::new();
    idempotency_key.hash(&mut hasher);
    counter.hash(&mut hasher);
    now_unix().hash(&mut hasher);
    format!("job_{:x}", hasher.finish())
}

// Re-export for backward compatibility with external callers.
pub use symbiotic_core::now_unix;

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn test_queue_file(name: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir()
            .join(format!(
                "symbiotic_queue_{name}_{}_{}_{}",
                now_unix(),
                std::process::id(),
                id
            ))
            .join("queue.log")
    }

    #[test]
    fn enqueue_is_idempotent() {
        let path = test_queue_file("idempotent");
        let queue = FileQueueStore::open(&path).expect("queue open");
        let now = now_unix();

        let first = queue
            .enqueue(EnqueueRequest {
                type_name: "ingest.fetch".to_string(),
                payload: "url=https://example.com".to_string(),
                idempotency_key: "same-key".to_string(),
                max_attempts: 3,
                next_run_at: now,
                force: false,
            })
            .expect("enqueue should work");
        let second = queue
            .enqueue(EnqueueRequest {
                type_name: "ingest.fetch".to_string(),
                payload: "url=https://example.com".to_string(),
                idempotency_key: "same-key".to_string(),
                max_attempts: 3,
                next_run_at: now,
                force: false,
            })
            .expect("enqueue should work");

        assert!(first.inserted());
        assert_eq!(
            second.disposition,
            EnqueueDisposition::ActiveDuplicate {
                status: JobStatus::Queued
            }
        );
        assert_eq!(first.job_id, second.job_id);
    }

    #[test]
    fn lease_and_ack_complete_flow() {
        let path = test_queue_file("lease_ack");
        let queue = FileQueueStore::open(&path).expect("queue open");
        let now = now_unix();

        let enqueued = queue
            .enqueue(EnqueueRequest {
                type_name: "archive.review.enqueue".to_string(),
                payload: "record=abc".to_string(),
                idempotency_key: "review-abc".to_string(),
                max_attempts: 3,
                next_run_at: now,
                force: false,
            })
            .expect("enqueue should work");

        let leased = queue
            .lease_next("worker-1", now, 60, None)
            .expect("lease should work")
            .expect("job should exist");
        assert_eq!(leased.status, JobStatus::Running);

        queue
            .ack(&enqueued.job_id, "worker-1", now + 1)
            .expect("ack should work");

        let done = queue
            .get(&enqueued.job_id)
            .expect("get should work")
            .expect("job exists");
        assert_eq!(done.status, JobStatus::Done);
    }

    #[test]
    fn failure_retries_then_moves_to_dlq() {
        let path = test_queue_file("dlq");
        let queue = FileQueueStore::open(&path).expect("queue open");
        let now = now_unix();

        let job = queue
            .enqueue(EnqueueRequest {
                type_name: "ingest.fetch".to_string(),
                payload: "url=https://bad.example".to_string(),
                idempotency_key: "bad-url".to_string(),
                max_attempts: 2,
                next_run_at: now,
                force: false,
            })
            .expect("enqueue should work");

        // Lease and fail the first attempt
        let _leased = queue
            .lease_next("worker-1", now, 60, None)
            .expect("lease should work")
            .expect("job should exist");
        let outcome1 = queue
            .fail(&job.job_id, "worker-1", now + 1, 30, "network")
            .expect("fail should work");
        assert_eq!(outcome1, FailOutcome::RetryScheduled);

        // Job should be in Failed state (observable), not immediately Queued
        let failed_job = queue
            .get(&job.job_id)
            .expect("get should work")
            .expect("job exists");
        assert_eq!(failed_job.status, JobStatus::Failed);
        assert_eq!(failed_job.attempts, 1);

        // Before backoff elapses, lease_next should not return the failed job
        let too_early = queue
            .lease_next("worker-1", now + 10, 60, None)
            .expect("lease should work");
        assert!(
            too_early.is_none(),
            "failed job should not be leasable before backoff elapses"
        );

        // After backoff elapses, lease_next picks up the failed job
        let retried = queue
            .lease_next("worker-1", now + 31, 60, None)
            .expect("lease should work")
            .expect("job should be retried after backoff");
        assert_eq!(retried.attempts, 1);
        assert_eq!(retried.status, JobStatus::Running);

        // Second failure exhausts max_attempts -> DLQ
        let outcome2 = queue
            .fail(&job.job_id, "worker-1", now + 32, 30, "network")
            .expect("fail should work");
        assert_eq!(outcome2, FailOutcome::MovedToDlq);

        let dlq = queue
            .get(&job.job_id)
            .expect("get should work")
            .expect("job exists");
        assert_eq!(dlq.status, JobStatus::Dlq);
        assert_eq!(dlq.attempts, 2);
    }

    #[test]
    fn failed_state_is_observable_and_listed() {
        let path = test_queue_file("failed_observable");
        let queue = FileQueueStore::open(&path).expect("queue open");
        let now = now_unix();

        let job = queue
            .enqueue(EnqueueRequest {
                type_name: "ingest.fetch".to_string(),
                payload: "url=https://flaky.example".to_string(),
                idempotency_key: "flaky-key".to_string(),
                max_attempts: 3,
                next_run_at: now,
                force: false,
            })
            .expect("enqueue should work");

        // Lease and fail
        let _leased = queue
            .lease_next("worker-1", now, 60, None)
            .expect("lease should work")
            .expect("job should exist");
        queue
            .fail(&job.job_id, "worker-1", now + 1, 60, "timeout")
            .expect("fail should work");

        // list_by_status(Failed) should return the job
        let failed_jobs = queue
            .list_by_status(JobStatus::Failed)
            .expect("list should work");
        assert_eq!(failed_jobs.len(), 1);
        assert_eq!(failed_jobs[0].job_id, job.job_id);
        assert_eq!(failed_jobs[0].last_error.as_deref(), Some("timeout"));

        // list_by_status(Queued) should be empty (job is Failed, not Queued)
        let queued_jobs = queue
            .list_by_status(JobStatus::Queued)
            .expect("list should work");
        assert!(
            queued_jobs.is_empty(),
            "failed job should not appear in queued list"
        );
    }

    #[test]
    fn failed_job_persists_and_retries_after_reopen() {
        let path = test_queue_file("failed_persist");
        let now = now_unix();
        let job_id;

        // Enqueue, lease, and fail in one session
        {
            let queue = FileQueueStore::open(&path).expect("queue open");
            let job = queue
                .enqueue(EnqueueRequest {
                    type_name: "ingest.fetch".to_string(),
                    payload: "url=https://reopen.example".to_string(),
                    idempotency_key: "reopen-key".to_string(),
                    max_attempts: 3,
                    next_run_at: now,
                    force: false,
                })
                .expect("enqueue should work");
            job_id = job.job_id.clone();

            let _leased = queue
                .lease_next("worker-1", now, 60, None)
                .expect("lease should work")
                .expect("job should exist");
            queue
                .fail(&job.job_id, "worker-1", now + 1, 30, "transient error")
                .expect("fail should work");
        }

        // Reopen and verify job is still Failed, then retry
        {
            let queue = FileQueueStore::open(&path).expect("queue open again");

            let job = queue
                .get(&job_id)
                .expect("get should work")
                .expect("job exists");
            assert_eq!(job.status, JobStatus::Failed);

            // After backoff, job should be leasable
            let retried = queue
                .lease_next("worker-2", now + 31, 60, None)
                .expect("lease should work")
                .expect("failed job should be leasable after backoff");
            assert_eq!(retried.job_id, job_id);
            assert_eq!(retried.status, JobStatus::Running);
        }
    }

    #[test]
    fn terminal_duplicate_reported_then_force_re_enqueues() {
        let path = test_queue_file("re_enqueue_done");
        let queue = FileQueueStore::open(&path).expect("queue open");
        let now = now_unix();

        // Enqueue, lease, and ack (Done)
        let first = queue
            .enqueue(EnqueueRequest {
                type_name: "workflow.run".to_string(),
                payload: "goal=abc".to_string(),
                idempotency_key: "goal-abc".to_string(),
                max_attempts: 1,
                next_run_at: now,
                force: false,
            })
            .expect("enqueue should work");
        assert!(first.inserted());

        let _leased = queue
            .lease_next("worker-1", now, 60, None)
            .expect("lease should work")
            .expect("job should exist");
        queue
            .ack(&first.job_id, "worker-1", now + 1)
            .expect("ack should work");

        // Re-enqueue without force — should get TerminalDuplicate
        let dup = queue
            .enqueue(EnqueueRequest {
                type_name: "workflow.run".to_string(),
                payload: "goal=abc-phase2".to_string(),
                idempotency_key: "goal-abc".to_string(),
                max_attempts: 1,
                next_run_at: now + 2,
                force: false,
            })
            .expect("enqueue should work");
        assert_eq!(
            dup.disposition,
            EnqueueDisposition::TerminalDuplicate {
                status: JobStatus::Done
            }
        );
        assert_eq!(dup.job_id, first.job_id, "returns the old job_id");

        // Re-enqueue WITH force — should insert a new job
        let forced = queue
            .enqueue(EnqueueRequest {
                type_name: "workflow.run".to_string(),
                payload: "goal=abc-phase2".to_string(),
                idempotency_key: "goal-abc".to_string(),
                max_attempts: 1,
                next_run_at: now + 2,
                force: true,
            })
            .expect("force re-enqueue should work");
        assert!(forced.inserted());
        assert_ne!(forced.job_id, first.job_id);
    }

    #[test]
    fn terminal_duplicate_reported_for_dlq() {
        let path = test_queue_file("re_enqueue_dlq");
        let queue = FileQueueStore::open(&path).expect("queue open");
        let now = now_unix();

        // Enqueue with max_attempts=1, lease, and fail -> DLQ
        let first = queue
            .enqueue(EnqueueRequest {
                type_name: "workflow.run".to_string(),
                payload: "goal=xyz".to_string(),
                idempotency_key: "goal-xyz".to_string(),
                max_attempts: 1,
                next_run_at: now,
                force: false,
            })
            .expect("enqueue should work");

        let _leased = queue
            .lease_next("worker-1", now, 60, None)
            .expect("lease should work")
            .expect("job should exist");
        let outcome = queue
            .fail(&first.job_id, "worker-1", now + 1, 30, "fatal")
            .expect("fail should work");
        assert_eq!(outcome, FailOutcome::MovedToDlq);

        // Without force — TerminalDuplicate
        let dup = queue
            .enqueue(EnqueueRequest {
                type_name: "workflow.run".to_string(),
                payload: "goal=xyz-retry".to_string(),
                idempotency_key: "goal-xyz".to_string(),
                max_attempts: 1,
                next_run_at: now + 2,
                force: false,
            })
            .expect("enqueue should work");
        assert_eq!(
            dup.disposition,
            EnqueueDisposition::TerminalDuplicate {
                status: JobStatus::Dlq
            }
        );

        // With force — inserts
        let forced = queue
            .enqueue(EnqueueRequest {
                type_name: "workflow.run".to_string(),
                payload: "goal=xyz-retry".to_string(),
                idempotency_key: "goal-xyz".to_string(),
                max_attempts: 1,
                next_run_at: now + 2,
                force: true,
            })
            .expect("force re-enqueue should work");
        assert!(forced.inserted());
        assert_ne!(forced.job_id, first.job_id);
    }

    #[test]
    fn queue_state_persists_across_reopen() {
        let path = test_queue_file("persist");
        let now = now_unix();

        {
            let queue = FileQueueStore::open(&path).expect("queue open");
            let _job = queue
                .enqueue(EnqueueRequest {
                    type_name: "ingest.fetch".to_string(),
                    payload: "url=https://persist.example".to_string(),
                    idempotency_key: "persist-key".to_string(),
                    max_attempts: 3,
                    next_run_at: now,
                    force: false,
                })
                .expect("enqueue should work");
        }

        {
            let queue = FileQueueStore::open(&path).expect("queue open again");
            let queued = queue
                .list_by_status(JobStatus::Queued)
                .expect("list should work");
            assert_eq!(queued.len(), 1);
            assert_eq!(queued[0].idempotency_key, "persist-key");
        }
    }
}
