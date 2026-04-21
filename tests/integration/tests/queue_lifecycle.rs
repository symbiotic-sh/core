//! Integration test: Queue -> Job Execution -> Completion
//!
//! Verifies the full queue lifecycle: enqueue, lease, ack/fail,
//! retry behavior with backoff, and dead-letter queue (DLQ) handling.

use symbiotic_queue::{EnqueueRequest, FailOutcome, FileQueueStore, JobStatus, QueueBackend};
use tempfile::TempDir;

fn now() -> u64 {
    symbiotic_queue::now_unix()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn full_lifecycle_enqueue_lease_ack() {
    let tmp = TempDir::new().expect("tmpdir");
    let queue = FileQueueStore::open(tmp.path().join("queue.log")).expect("queue");
    let now = now();

    // Step 1: Enqueue a job
    let outcome = queue
        .enqueue(EnqueueRequest {
            type_name: "ingest.fetch".to_string(),
            payload: "url=https://example.com/article".to_string(),
            idempotency_key: "fetch-1".to_string(),
            max_attempts: 3,
            next_run_at: now,
            force: false,
        })
        .expect("enqueue");
    assert!(outcome.inserted());
    let job_id = outcome.job_id.clone();

    // Step 2: Lease the job (simulating a worker picking it up)
    let leased = queue
        .lease_next("worker-1", now, 60, None)
        .expect("lease")
        .expect("job should be available");
    assert_eq!(leased.job_id, job_id);
    assert_eq!(leased.status, JobStatus::Running);

    // Step 3: Acknowledge completion
    queue.ack(&job_id, "worker-1", now + 5).expect("ack");

    let done = queue.get(&job_id).expect("get").expect("exists");
    assert_eq!(done.status, JobStatus::Done);
    assert!(done.lease_owner.is_none());
}

#[test]
fn retry_on_transient_failure() {
    let tmp = TempDir::new().expect("tmpdir");
    let queue = FileQueueStore::open(tmp.path().join("queue.log")).expect("queue");
    let now = now();

    let outcome = queue
        .enqueue(EnqueueRequest {
            type_name: "ingest.fetch".to_string(),
            payload: "url=https://flaky.example".to_string(),
            idempotency_key: "flaky-1".to_string(),
            max_attempts: 3,
            next_run_at: now,
            force: false,
        })
        .expect("enqueue");
    let job_id = outcome.job_id;

    // First attempt: lease and fail
    let _leased = queue
        .lease_next("worker-1", now, 60, None)
        .expect("lease")
        .expect("available");

    let fail_result = queue
        .fail(&job_id, "worker-1", now + 1, 30, "timeout connecting")
        .expect("fail");
    assert_eq!(fail_result, FailOutcome::RetryScheduled);

    // Verify job stays in Failed state until backoff elapses
    let job = queue.get(&job_id).expect("get").expect("exists");
    assert_eq!(job.status, JobStatus::Failed);
    assert_eq!(job.attempts, 1);
    assert!(job.last_error.as_deref() == Some("timeout connecting"));

    // Job should not be leasable before backoff expires
    let not_ready = queue
        .lease_next("worker-1", now + 10, 60, None)
        .expect("lease");
    assert!(
        not_ready.is_none(),
        "job should not be available before backoff"
    );

    // Job should be leasable after backoff
    let retried = queue
        .lease_next("worker-1", now + 31, 60, None)
        .expect("lease")
        .expect("available after backoff");
    assert_eq!(retried.job_id, job_id);
    assert_eq!(retried.attempts, 1);
}

#[test]
fn permanent_failure_moves_to_dlq() {
    let tmp = TempDir::new().expect("tmpdir");
    let queue = FileQueueStore::open(tmp.path().join("queue.log")).expect("queue");
    let now = now();

    let outcome = queue
        .enqueue(EnqueueRequest {
            type_name: "ingest.fetch".to_string(),
            payload: "url=https://dead.example".to_string(),
            idempotency_key: "dead-1".to_string(),
            max_attempts: 2,
            next_run_at: now,
            force: false,
        })
        .expect("enqueue");
    let job_id = outcome.job_id;

    // Attempt 1: fail
    let _l1 = queue
        .lease_next("worker-1", now, 60, None)
        .expect("lease")
        .expect("available");
    let r1 = queue
        .fail(&job_id, "worker-1", now + 1, 10, "404 not found")
        .expect("fail");
    assert_eq!(r1, FailOutcome::RetryScheduled);

    // Attempt 2: fail again -> should move to DLQ
    let _l2 = queue
        .lease_next("worker-1", now + 11, 60, None)
        .expect("lease")
        .expect("available for retry");
    let r2 = queue
        .fail(&job_id, "worker-1", now + 12, 10, "still 404")
        .expect("fail");
    assert_eq!(r2, FailOutcome::MovedToDlq);

    // Verify DLQ status
    let dlq_job = queue.get(&job_id).expect("get").expect("exists");
    assert_eq!(dlq_job.status, JobStatus::Dlq);
    assert_eq!(dlq_job.attempts, 2);
    assert!(dlq_job.last_error.as_deref() == Some("still 404"));

    // DLQ job should not be leasable
    let nothing = queue
        .lease_next("worker-1", now + 100, 60, None)
        .expect("lease");
    assert!(nothing.is_none(), "DLQ jobs should not be leasable");
}

#[test]
fn idempotent_enqueue_returns_same_job() {
    let tmp = TempDir::new().expect("tmpdir");
    let queue = FileQueueStore::open(tmp.path().join("queue.log")).expect("queue");
    let now = now();

    let first = queue
        .enqueue(EnqueueRequest {
            type_name: "ingest.fetch".to_string(),
            payload: "url=https://example.com".to_string(),
            idempotency_key: "idem-1".to_string(),
            max_attempts: 3,
            next_run_at: now,
            force: false,
        })
        .expect("first enqueue");

    let second = queue
        .enqueue(EnqueueRequest {
            type_name: "ingest.fetch".to_string(),
            payload: "url=https://example.com".to_string(),
            idempotency_key: "idem-1".to_string(),
            max_attempts: 3,
            next_run_at: now,
            force: false,
        })
        .expect("second enqueue");

    assert!(first.inserted());
    assert!(!second.inserted());
    assert_eq!(first.job_id, second.job_id);
}

#[test]
fn type_filter_restricts_leased_jobs() {
    let tmp = TempDir::new().expect("tmpdir");
    let queue = FileQueueStore::open(tmp.path().join("queue.log")).expect("queue");
    let now = now();

    queue
        .enqueue(EnqueueRequest {
            type_name: "ingest.fetch".to_string(),
            payload: "url=https://example.com".to_string(),
            idempotency_key: "type-1".to_string(),
            max_attempts: 1,
            next_run_at: now,
            force: false,
        })
        .expect("enqueue fetch");

    queue
        .enqueue(EnqueueRequest {
            type_name: "archive.review".to_string(),
            payload: "record=abc".to_string(),
            idempotency_key: "type-2".to_string(),
            max_attempts: 1,
            next_run_at: now,
            force: false,
        })
        .expect("enqueue review");

    // Only lease review jobs
    let filter = vec!["archive.review".to_string()];
    let leased = queue
        .lease_next("worker-1", now, 60, Some(&filter))
        .expect("lease")
        .expect("available");
    assert_eq!(leased.type_name, "archive.review");
}

#[test]
fn expired_lease_is_reclaimed() {
    let tmp = TempDir::new().expect("tmpdir");
    let queue = FileQueueStore::open(tmp.path().join("queue.log")).expect("queue");
    let now = now();

    let outcome = queue
        .enqueue(EnqueueRequest {
            type_name: "ingest.fetch".to_string(),
            payload: "url=https://example.com".to_string(),
            idempotency_key: "reclaim-1".to_string(),
            max_attempts: 3,
            next_run_at: now,
            force: false,
        })
        .expect("enqueue");
    let job_id = outcome.job_id;

    // Worker 1 leases with 30-second lease
    let _leased = queue
        .lease_next("worker-1", now, 30, None)
        .expect("lease")
        .expect("available");

    // Worker 1 dies without acking. After lease expires, reclaim.
    let reclaimed = queue.reclaim_expired_leases(now + 31).expect("reclaim");
    assert_eq!(reclaimed, 1);

    // Worker 2 can now pick up the job
    let retaken = queue
        .lease_next("worker-2", now + 32, 60, None)
        .expect("lease")
        .expect("available after reclaim");
    assert_eq!(retaken.job_id, job_id);
    assert_eq!(retaken.lease_owner, Some("worker-2".to_string()));
}

#[test]
fn heartbeat_extends_lease() {
    let tmp = TempDir::new().expect("tmpdir");
    let queue = FileQueueStore::open(tmp.path().join("queue.log")).expect("queue");
    let now = now();

    let outcome = queue
        .enqueue(EnqueueRequest {
            type_name: "long.task".to_string(),
            payload: "heavy work".to_string(),
            idempotency_key: "hb-1".to_string(),
            max_attempts: 1,
            next_run_at: now,
            force: false,
        })
        .expect("enqueue");
    let job_id = outcome.job_id;

    // Lease with 30-second lease
    let _leased = queue
        .lease_next("worker-1", now, 30, None)
        .expect("lease")
        .expect("available");

    // Heartbeat at t+25, extending lease by another 30 seconds
    queue
        .heartbeat(&job_id, "worker-1", now + 25, 30)
        .expect("heartbeat");

    // At t+35, the original lease would have expired, but heartbeat extended it
    let reclaimed = queue.reclaim_expired_leases(now + 35).expect("reclaim");
    assert_eq!(reclaimed, 0, "lease should be extended by heartbeat");

    // At t+56, the heartbeat-extended lease has expired
    let reclaimed = queue.reclaim_expired_leases(now + 56).expect("reclaim");
    assert_eq!(reclaimed, 1, "lease should have expired by now");
}

#[test]
fn queue_state_persists_across_reopen() {
    let tmp = TempDir::new().expect("tmpdir");
    let path = tmp.path().join("persist.log");
    let now = now();

    // Enqueue and close
    {
        let queue = FileQueueStore::open(&path).expect("open");
        queue
            .enqueue(EnqueueRequest {
                type_name: "persist.test".to_string(),
                payload: "data".to_string(),
                idempotency_key: "persist-1".to_string(),
                max_attempts: 1,
                next_run_at: now,
                force: false,
            })
            .expect("enqueue");
    }

    // Reopen and verify
    {
        let queue = FileQueueStore::open(&path).expect("reopen");
        let queued = queue.list_by_status(JobStatus::Queued).expect("list");
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].type_name, "persist.test");
    }
}

#[test]
fn list_by_status_filters_correctly() {
    let tmp = TempDir::new().expect("tmpdir");
    let queue = FileQueueStore::open(tmp.path().join("queue.log")).expect("queue");
    let now = now();

    // Create 3 jobs
    for i in 0..3 {
        queue
            .enqueue(EnqueueRequest {
                type_name: "test.job".to_string(),
                payload: format!("job-{i}"),
                idempotency_key: format!("status-{i}"),
                max_attempts: 1,
                next_run_at: now,
                force: false,
            })
            .expect("enqueue");
    }

    // Lease one (makes it Running)
    let _leased = queue
        .lease_next("worker-1", now, 60, None)
        .expect("lease")
        .expect("available");

    let queued = queue
        .list_by_status(JobStatus::Queued)
        .expect("list queued");
    let running = queue
        .list_by_status(JobStatus::Running)
        .expect("list running");

    assert_eq!(queued.len(), 2);
    assert_eq!(running.len(), 1);
}
