//! Integration test: Goal -> Domain Queue -> Workflow Progress
//!
//! Verifies goal creation via domain tasks, queue management (active/backlog),
//! status updates, assignment, and promotion between queues.

use serde_json::json;
use symbiotic_domains::{DomainQueueStore, DomainTask, QueueKind};
use tempfile::TempDir;

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn create_goal_with_tasks_and_track_progress() {
    let tmp = TempDir::new().expect("tmpdir");
    let store = DomainQueueStore::open(tmp.path().join("domains")).expect("store");
    let now = now();

    // Step 1: Create a goal's tasks in the "build-product" domain
    let task1 = DomainTask {
        id: store.generate_task_id("build-product"),
        title: "Design API schema".to_string(),
        goal: Some("mvp-launch".to_string()),
        stream: Some("engineering".to_string()),
        status: "pending".to_string(),
        priority: 0,
        assignee: None,
        created_at: now,
        due_at: None,
        context: json!({"milestone": "v0.1"}),
    };

    // Small sleep to ensure unique IDs
    let task2 = DomainTask {
        id: format!("{}-2", store.generate_task_id("build-product")),
        title: "Implement intake pipeline".to_string(),
        goal: Some("mvp-launch".to_string()),
        stream: Some("engineering".to_string()),
        status: "pending".to_string(),
        priority: 1,
        assignee: None,
        created_at: now + 1,
        due_at: None,
        context: json!({"depends_on": "Design API schema"}),
    };

    let task3 = DomainTask {
        id: format!("{}-3", store.generate_task_id("build-product")),
        title: "Write user documentation".to_string(),
        goal: Some("mvp-launch".to_string()),
        stream: Some("docs".to_string()),
        status: "pending".to_string(),
        priority: 2,
        assignee: None,
        created_at: now + 2,
        due_at: None,
        context: json!({}),
    };

    store
        .add_task("build-product", task1.clone(), QueueKind::Active)
        .expect("add task1");
    store
        .add_task("build-product", task2.clone(), QueueKind::Active)
        .expect("add task2");
    store
        .add_task("build-product", task3.clone(), QueueKind::Backlog)
        .expect("add task3 to backlog");

    // Step 2: Verify task listing
    let active = store
        .list_tasks("build-product", Some(QueueKind::Active))
        .expect("list active");
    assert_eq!(active.len(), 2);

    let backlog = store
        .list_tasks("build-product", Some(QueueKind::Backlog))
        .expect("list backlog");
    assert_eq!(backlog.len(), 1);

    let all = store.list_tasks("build-product", None).expect("list all");
    assert_eq!(all.len(), 3);
}

#[test]
fn update_task_status_through_workflow() {
    let tmp = TempDir::new().expect("tmpdir");
    let store = DomainQueueStore::open(tmp.path().join("domains")).expect("store");
    let now = now();

    let task = DomainTask {
        id: "wf-task-1".to_string(),
        title: "Implement feature X".to_string(),
        goal: Some("product-mvp".to_string()),
        stream: Some("engineering".to_string()),
        status: "pending".to_string(),
        priority: 0,
        assignee: None,
        created_at: now,
        due_at: None,
        context: json!({}),
    };

    store
        .add_task("engineering", task.clone(), QueueKind::Active)
        .expect("add");

    // Step 1: Assign to a worker
    let assigned = store
        .assign_task("engineering", "wf-task-1", "agent-alpha")
        .expect("assign");
    assert_eq!(assigned.assignee.as_deref(), Some("agent-alpha"));

    // Step 2: Move to in_progress
    let in_progress = store
        .update_status("engineering", "wf-task-1", "in_progress")
        .expect("update");
    assert_eq!(in_progress.status, "in_progress");

    // Step 3: Complete
    let completed = store
        .update_status("engineering", "wf-task-1", "completed")
        .expect("complete");
    assert_eq!(completed.status, "completed");
}

#[test]
fn promote_task_from_backlog_to_active() {
    let tmp = TempDir::new().expect("tmpdir");
    let store = DomainQueueStore::open(tmp.path().join("domains")).expect("store");
    let now = now();

    let task = DomainTask {
        id: "promote-1".to_string(),
        title: "Nice-to-have feature".to_string(),
        goal: Some("polish".to_string()),
        stream: None,
        status: "pending".to_string(),
        priority: 5,
        assignee: None,
        created_at: now,
        due_at: None,
        context: json!({}),
    };

    store
        .add_task("product", task, QueueKind::Backlog)
        .expect("add to backlog");

    // Verify it's in backlog
    let backlog = store
        .list_tasks("product", Some(QueueKind::Backlog))
        .expect("list backlog");
    assert_eq!(backlog.len(), 1);

    let active = store
        .list_tasks("product", Some(QueueKind::Active))
        .expect("list active");
    assert_eq!(active.len(), 0);

    // Promote
    let promoted = store.promote_task("product", "promote-1").expect("promote");
    assert_eq!(promoted.id, "promote-1");

    // Verify moved
    let backlog = store
        .list_tasks("product", Some(QueueKind::Backlog))
        .expect("list backlog");
    assert_eq!(backlog.len(), 0);

    let active = store
        .list_tasks("product", Some(QueueKind::Active))
        .expect("list active");
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].id, "promote-1");
}

#[test]
fn multi_domain_task_listing() {
    let tmp = TempDir::new().expect("tmpdir");
    let store = DomainQueueStore::open(tmp.path().join("domains")).expect("store");
    let now = now();

    let engineering_task = DomainTask {
        id: "eng-1".to_string(),
        title: "Build API".to_string(),
        goal: Some("mvp".to_string()),
        stream: Some("engineering".to_string()),
        status: "pending".to_string(),
        priority: 0,
        assignee: None,
        created_at: now,
        due_at: None,
        context: json!({}),
    };

    let marketing_task = DomainTask {
        id: "mkt-1".to_string(),
        title: "Write launch blog post".to_string(),
        goal: Some("mvp".to_string()),
        stream: Some("marketing".to_string()),
        status: "pending".to_string(),
        priority: 0,
        assignee: None,
        created_at: now,
        due_at: None,
        context: json!({}),
    };

    store
        .add_task("engineering", engineering_task, QueueKind::Active)
        .expect("add eng");
    store
        .add_task("marketing", marketing_task, QueueKind::Active)
        .expect("add mkt");

    // List all domains
    let domains = store.list_domains().expect("list domains");
    assert!(domains.contains(&"engineering".to_string()));
    assert!(domains.contains(&"marketing".to_string()));

    // List all tasks across domains
    let all = store.list_all_tasks(None).expect("list all");
    assert_eq!(all.len(), 2);

    // Verify domain labels
    let domain_names: Vec<&str> = all.iter().map(|(d, _)| d.as_str()).collect();
    assert!(domain_names.contains(&"engineering"));
    assert!(domain_names.contains(&"marketing"));
}

#[test]
fn domain_queue_meta_tracks_counts() {
    let tmp = TempDir::new().expect("tmpdir");
    let store = DomainQueueStore::open(tmp.path().join("domains")).expect("store");
    let now = now();

    // Add tasks to different queues
    store
        .add_task(
            "ops",
            DomainTask {
                id: "ops-1".to_string(),
                title: "Deploy v1".to_string(),
                goal: None,
                stream: None,
                status: "pending".to_string(),
                priority: 0,
                assignee: None,
                created_at: now,
                due_at: None,
                context: json!({}),
            },
            QueueKind::Active,
        )
        .expect("add active");

    store
        .add_task(
            "ops",
            DomainTask {
                id: "ops-2".to_string(),
                title: "Plan v2".to_string(),
                goal: None,
                stream: None,
                status: "pending".to_string(),
                priority: 0,
                assignee: None,
                created_at: now,
                due_at: None,
                context: json!({}),
            },
            QueueKind::Backlog,
        )
        .expect("add backlog");

    // Read meta file to verify counts
    let meta_path = tmp
        .path()
        .join("domains")
        .join("ops")
        .join("queue")
        .join("meta.json");
    let meta_content = std::fs::read_to_string(&meta_path).expect("read meta");
    let meta: serde_json::Value = serde_json::from_str(&meta_content).expect("parse meta");
    assert_eq!(meta["total_active"], 1);
    assert_eq!(meta["total_backlog"], 1);
    assert_eq!(meta["domain"], "ops");
}

#[test]
fn task_not_found_returns_error() {
    let tmp = TempDir::new().expect("tmpdir");
    let store = DomainQueueStore::open(tmp.path().join("domains")).expect("store");

    // Ensure domain exists
    store.ensure_domain("empty").expect("ensure");

    let err = store
        .update_status("empty", "nonexistent", "done")
        .expect_err("should fail");
    assert!(err.to_string().contains("task not found"), "error: {err}");
}
