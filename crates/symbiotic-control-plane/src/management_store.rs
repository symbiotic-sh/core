use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::Serialize;
use uuid::Uuid;

use crate::claims::{claims_conflict, ScopeClaim, ScopeClaimStatus};
use crate::leases::{HeartbeatStatus, HeartbeatUpdate, ManagementLeaseConfig};
use crate::work_items::{WorkItem, WorkItemStatus};

pub struct ManagementStore {
    root_path: PathBuf,
    work_items: HashMap<String, WorkItem>,
    claims: HashMap<String, ScopeClaim>,
    lease_config: ManagementLeaseConfig,
}

impl ManagementStore {
    pub fn new(root_path: PathBuf) -> Self {
        Self::with_lease_config(root_path, ManagementLeaseConfig::default())
    }

    pub fn with_lease_config(root_path: PathBuf, lease_config: ManagementLeaseConfig) -> Self {
        Self {
            root_path,
            work_items: HashMap::new(),
            claims: HashMap::new(),
            lease_config,
        }
    }

    pub fn load(&mut self) -> Result<()> {
        self.work_items = load_dir::<WorkItem>(&self.work_items_dir())?;
        self.claims = load_dir::<ScopeClaim>(&self.claims_dir())?;
        Ok(())
    }

    pub fn upsert_work_item(&mut self, work_item: WorkItem) -> Result<()> {
        let id = work_item.id.clone();
        self.work_items.insert(id.clone(), work_item);
        let work_item = self
            .work_items
            .get(&id)
            .expect("work item must exist after insert");
        atomic_write_json(&self.work_items_dir().join(format!("{id}.json")), work_item)
    }

    pub fn get_work_item(&self, id: &str) -> Option<&WorkItem> {
        self.work_items.get(id)
    }

    pub fn work_items_for_initiative(&self, initiative_id: &str) -> Vec<WorkItem> {
        let mut items: Vec<WorkItem> = self
            .work_items
            .values()
            .filter(|item| item.initiative_id.as_deref() == Some(initiative_id))
            .cloned()
            .collect();
        items.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        items
    }

    pub fn work_items_for_parent(&self, parent_work_item_id: &str) -> Vec<WorkItem> {
        let mut items: Vec<WorkItem> = self
            .work_items
            .values()
            .filter(|item| item.parent_work_item_id.as_deref() == Some(parent_work_item_id))
            .cloned()
            .collect();
        items.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        items
    }

    pub fn work_items(&self) -> Vec<WorkItem> {
        let mut items: Vec<WorkItem> = self.work_items.values().cloned().collect();
        items.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        items
    }

    pub fn get_claim(&self, id: &str) -> Option<&ScopeClaim> {
        self.claims.get(id)
    }

    pub fn work_item_count(&self) -> usize {
        self.work_items.len()
    }

    pub fn claim_count(&self) -> usize {
        self.claims.len()
    }

    pub fn active_claim_count(&self) -> usize {
        self.active_claims().count()
    }

    pub fn active_claims(&self) -> impl Iterator<Item = &ScopeClaim> {
        self.claims.values().filter(|claim| claim.is_active())
    }

    pub fn grant_claim(&mut self, claim: ScopeClaim) -> Result<()> {
        if claim.status != ScopeClaimStatus::Active {
            bail!("claim '{}' must be active when granted", claim.id);
        }

        for existing in self.active_claims() {
            if existing.id != claim.id && claims_conflict(existing, &claim) {
                bail!(
                    "claim '{}' conflicts with active claim '{}'",
                    claim.id,
                    existing.id
                );
            }
        }

        let claim_id = claim.id.clone();
        let work_item_id = claim.work_item_id.clone();
        let observed_at = claim.updated_at;
        let claim_path = self.claims_dir().join(format!("{claim_id}.json"));
        self.claims.insert(claim_id.clone(), claim);

        let work_item_snapshot = {
            let work_item = self
                .work_items
                .get_mut(&work_item_id)
                .ok_or_else(|| anyhow::anyhow!("work item for claim '{}' not found", claim_id))?;
            work_item.add_claim(claim_id.clone(), observed_at);
            if work_item.status == WorkItemStatus::ClaimPending {
                work_item.set_status(WorkItemStatus::Claimed, observed_at);
            }
            work_item.clone()
        };

        let claim_snapshot = self
            .claims
            .get(&claim_id)
            .expect("claim must exist after insert")
            .clone();

        atomic_write_json(&claim_path, &claim_snapshot)?;
        atomic_write_json(
            &self
                .work_items_dir()
                .join(format!("{}.json", work_item_snapshot.id)),
            &work_item_snapshot,
        )?;
        Ok(())
    }

    pub fn record_heartbeat(&mut self, heartbeat: HeartbeatUpdate) -> Result<Vec<String>> {
        let mut updated_claim_ids = Vec::new();
        let matching_claim_ids: Vec<String> = self
            .claims
            .values()
            .filter(|claim| {
                claim.is_active()
                    && claim.work_item_id == heartbeat.work_item_id
                    && claim.holder_agent_id == heartbeat.agent_id
            })
            .map(|claim| claim.id.clone())
            .collect();

        let claims_dir = self.claims_dir();
        for claim_id in matching_claim_ids {
            let claim_snapshot = {
                let claim = self
                    .claims
                    .get_mut(&claim_id)
                    .expect("matching claim must still exist");
                claim.lease.renew(&heartbeat, &self.lease_config);
                claim.updated_at = heartbeat.observed_at;
                if heartbeat.status == HeartbeatStatus::Releasing {
                    claim.status = ScopeClaimStatus::Releasing;
                }
                claim.clone()
            };
            atomic_write_json(
                &claims_dir.join(format!("{}.json", claim_snapshot.id)),
                &claim_snapshot,
            )?;
            updated_claim_ids.push(claim_snapshot.id);
        }

        if updated_claim_ids.is_empty() {
            bail!(
                "no active claims found for work item '{}' and agent '{}'",
                heartbeat.work_item_id,
                heartbeat.agent_id
            );
        }

        let work_items_dir = self.work_items_dir();
        if let Some(work_item) = self.work_items.get_mut(&heartbeat.work_item_id) {
            let status = match heartbeat.status {
                HeartbeatStatus::Alive => WorkItemStatus::Running,
                HeartbeatStatus::Blocked => WorkItemStatus::Blocked,
                HeartbeatStatus::AwaitingReview => WorkItemStatus::PendingReview,
                HeartbeatStatus::Releasing => WorkItemStatus::Claimed,
            };
            work_item.set_status(status, heartbeat.observed_at);
            let snapshot = work_item.clone();
            atomic_write_json(
                &work_items_dir.join(format!("{}.json", snapshot.id)),
                &snapshot,
            )?;
        }

        append_json_line(&self.heartbeats_log_path(), &heartbeat)?;
        Ok(updated_claim_ids)
    }

    pub fn expire_stale_claims(&mut self, now_ts: i64) -> Result<Vec<String>> {
        let mut expired_claim_ids = Vec::new();
        let mut affected_work_items = Vec::new();

        let expired_ids: Vec<String> = self
            .claims
            .values()
            .filter(|claim| claim.is_active() && claim.lease.is_expired(now_ts))
            .map(|claim| claim.id.clone())
            .collect();

        let claims_dir = self.claims_dir();
        for claim_id in expired_ids {
            let claim_snapshot = {
                let claim = self
                    .claims
                    .get_mut(&claim_id)
                    .expect("expired claim must still exist");
                claim.status = ScopeClaimStatus::Expired;
                claim.updated_at = now_ts;
                claim.clone()
            };
            atomic_write_json(
                &claims_dir.join(format!("{}.json", claim_snapshot.id)),
                &claim_snapshot,
            )?;
            expired_claim_ids.push(claim_snapshot.id.clone());
            affected_work_items.push(claim_snapshot.work_item_id);
        }

        let work_items_dir = self.work_items_dir();
        for work_item_id in affected_work_items {
            if let Some(work_item) = self.work_items.get_mut(&work_item_id) {
                work_item.set_status(WorkItemStatus::Expired, now_ts);
                let snapshot = work_item.clone();
                atomic_write_json(
                    &work_items_dir.join(format!("{}.json", snapshot.id)),
                    &snapshot,
                )?;
            }
        }

        Ok(expired_claim_ids)
    }

    pub fn revoke_claims_for_work_item(
        &mut self,
        work_item_id: &str,
        now_ts: i64,
    ) -> Result<Vec<String>> {
        let mut revoked_claim_ids = Vec::new();
        let claim_ids: Vec<String> = self
            .claims
            .values()
            .filter(|claim| {
                claim.work_item_id == work_item_id
                    && matches!(
                        claim.status,
                        ScopeClaimStatus::Active | ScopeClaimStatus::Releasing
                    )
            })
            .map(|claim| claim.id.clone())
            .collect();

        let claims_dir = self.claims_dir();
        for claim_id in claim_ids {
            let claim_snapshot = {
                let claim = self
                    .claims
                    .get_mut(&claim_id)
                    .expect("claim selected for revocation must still exist");
                claim.status = ScopeClaimStatus::Revoked;
                claim.updated_at = now_ts;
                claim.clone()
            };
            atomic_write_json(
                &claims_dir.join(format!("{}.json", claim_snapshot.id)),
                &claim_snapshot,
            )?;
            revoked_claim_ids.push(claim_snapshot.id);
        }

        Ok(revoked_claim_ids)
    }

    fn work_items_dir(&self) -> PathBuf {
        self.root_path.join("work-items")
    }

    fn claims_dir(&self) -> PathBuf {
        self.root_path.join("scope-claims")
    }

    fn heartbeats_log_path(&self) -> PathBuf {
        self.root_path.join("heartbeats.log")
    }
}

fn load_dir<T>(dir: &Path) -> Result<HashMap<String, T>>
where
    T: for<'de> serde::Deserialize<'de> + IdentifiedRecord,
{
    let mut records = HashMap::new();
    if !dir.exists() {
        return Ok(records);
    }

    for entry in fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let content = fs::read_to_string(&path)
            .with_context(|| format!("reading record file {}", path.display()))?;
        let record: T = serde_json::from_str(&content)
            .with_context(|| format!("parsing record file {}", path.display()))?;
        records.insert(record.record_id().to_string(), record);
    }

    Ok(records)
}

trait IdentifiedRecord {
    fn record_id(&self) -> &str;
}

impl IdentifiedRecord for WorkItem {
    fn record_id(&self) -> &str {
        &self.id
    }
}

impl IdentifiedRecord for ScopeClaim {
    fn record_id(&self) -> &str {
        &self.id
    }
}

fn atomic_write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let data = serde_json::to_vec_pretty(value)
        .with_context(|| format!("serializing {}", path.display()))?;
    atomic_write(path, &data)
}

fn atomic_write(path: &Path, data: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating parent dir {}", parent.display()))?;
    }

    let tmp_path = path.with_extension(format!("tmp-{}", Uuid::new_v4()));
    fs::write(&tmp_path, data)
        .with_context(|| format!("writing temp file {}", tmp_path.display()))?;
    fs::rename(&tmp_path, path)
        .with_context(|| format!("replacing {} with temp file", path.display()))?;
    Ok(())
}

fn append_json_line<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating parent dir {}", parent.display()))?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("opening log file {}", path.display()))?;
    serde_json::to_writer(&mut file, value)
        .with_context(|| format!("serializing log line for {}", path.display()))?;
    file.write_all(b"\n")
        .with_context(|| format!("writing newline to {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;
    use crate::claims::{CollaborationScope, ScopeMode};
    use crate::leases::Lease;
    use crate::work_items::{AssignmentMode, ReviewMode, WorkItemKind, WorkPriority, WorkUrgency};

    fn test_work_item(id: &str, observed_at: i64) -> WorkItem {
        WorkItem {
            id: id.to_string(),
            project_id: "symbiotic".to_string(),
            initiative_id: None,
            parent_work_item_id: None,
            kind: WorkItemKind::Execution,
            thread_id: None,
            title: "Test work".to_string(),
            summary: "Test summary".to_string(),
            status: WorkItemStatus::ClaimPending,
            priority: WorkPriority::P1,
            urgency: WorkUrgency::Normal,
            assignment_mode: AssignmentMode::SingleOwner,
            requested_scopes: Vec::new(),
            accepted_claim_ids: Vec::new(),
            assignee: None,
            blocked_by: Vec::new(),
            depends_on: Vec::new(),
            review_mode: ReviewMode::NoReview,
            cancellation: None,
            created_at: observed_at,
            updated_at: observed_at,
        }
    }

    fn test_claim(id: &str, work_item_id: &str, path: &str) -> ScopeClaim {
        ScopeClaim {
            id: id.to_string(),
            work_item_id: work_item_id.to_string(),
            holder_agent_id: "agent-1".to_string(),
            scope: CollaborationScope::RepoPath {
                repo_id: "runtime".to_string(),
                path: path.to_string(),
            },
            mode: ScopeMode::ExclusiveWrite,
            status: ScopeClaimStatus::Active,
            lease: Lease::new("agent-1".to_string(), 100, 30, 2),
            granted_at: 100,
            updated_at: 100,
        }
    }

    #[test]
    fn persistence_round_trip_restores_work_items_and_claims() {
        let tmp = TempDir::new().unwrap();
        let mut store = ManagementStore::new(tmp.path().to_path_buf());
        store.upsert_work_item(test_work_item("w1", 100)).unwrap();
        store
            .grant_claim(test_claim("c1", "w1", "services/symbiotic-daemon/src"))
            .unwrap();

        let mut reloaded = ManagementStore::new(tmp.path().to_path_buf());
        reloaded.load().unwrap();

        assert!(reloaded.get_work_item("w1").is_some());
        assert!(reloaded.get_claim("c1").is_some());
    }

    #[test]
    fn grant_claim_rejects_overlapping_exclusive_write() {
        let tmp = TempDir::new().unwrap();
        let mut store = ManagementStore::new(tmp.path().to_path_buf());
        store.upsert_work_item(test_work_item("w1", 100)).unwrap();
        store.upsert_work_item(test_work_item("w2", 100)).unwrap();
        store
            .grant_claim(test_claim("c1", "w1", "services/symbiotic-daemon/src"))
            .unwrap();

        let result = store.grant_claim(test_claim(
            "c2",
            "w2",
            "services/symbiotic-daemon/src/matrix",
        ));
        assert!(result.is_err());
    }

    #[test]
    fn expired_claim_marks_work_item_expired() {
        let tmp = TempDir::new().unwrap();
        let mut store = ManagementStore::new(tmp.path().to_path_buf());
        store.upsert_work_item(test_work_item("w1", 100)).unwrap();
        store
            .grant_claim(test_claim("c1", "w1", "services/symbiotic-daemon/src"))
            .unwrap();

        let expired = store.expire_stale_claims(221).unwrap();
        assert_eq!(expired, vec!["c1".to_string()]);
        assert_eq!(
            store.get_work_item("w1").unwrap().status,
            WorkItemStatus::Expired
        );
        assert_eq!(
            store.get_claim("c1").unwrap().status,
            ScopeClaimStatus::Expired
        );
    }

    #[test]
    fn heartbeat_renews_claim_and_updates_work_item_status() {
        let tmp = TempDir::new().unwrap();
        let mut store = ManagementStore::new(tmp.path().to_path_buf());
        store.upsert_work_item(test_work_item("w1", 100)).unwrap();
        store
            .grant_claim(test_claim("c1", "w1", "services/symbiotic-daemon/src"))
            .unwrap();

        store
            .record_heartbeat(HeartbeatUpdate {
                work_item_id: "w1".to_string(),
                agent_id: "agent-1".to_string(),
                status: HeartbeatStatus::Alive,
                progress_summary: Some("working".to_string()),
                progress_percent: Some(25),
                needs_attention: false,
                observed_at: 150,
            })
            .unwrap();

        assert_eq!(
            store.get_work_item("w1").unwrap().status,
            WorkItemStatus::Running
        );
        assert_eq!(store.get_claim("c1").unwrap().lease.last_heartbeat_at, 150);
    }

    #[test]
    fn revoke_claims_for_work_item_marks_claims_revoked() {
        let tmp = TempDir::new().unwrap();
        let mut store = ManagementStore::new(tmp.path().to_path_buf());
        store.upsert_work_item(test_work_item("w1", 100)).unwrap();
        store
            .grant_claim(test_claim("c1", "w1", "services/symbiotic-daemon/src"))
            .unwrap();

        let revoked = store.revoke_claims_for_work_item("w1", 160).unwrap();

        assert_eq!(revoked, vec!["c1".to_string()]);
        assert_eq!(
            store.get_claim("c1").unwrap().status,
            ScopeClaimStatus::Revoked
        );
    }

    #[test]
    fn work_items_for_parent_returns_only_direct_children() {
        let tmp = TempDir::new().unwrap();
        let mut store = ManagementStore::new(tmp.path().to_path_buf());
        let mut parent = test_work_item("parent", 100);
        parent.status = WorkItemStatus::Running;
        store.upsert_work_item(parent).unwrap();

        let mut child_a = test_work_item("child-a", 101);
        child_a.parent_work_item_id = Some("parent".to_string());
        let mut child_b = test_work_item("child-b", 102);
        child_b.parent_work_item_id = Some("parent".to_string());
        let outsider = test_work_item("outsider", 103);

        store.upsert_work_item(child_a).unwrap();
        store.upsert_work_item(child_b).unwrap();
        store.upsert_work_item(outsider).unwrap();

        let children = store.work_items_for_parent("parent");
        assert_eq!(children.len(), 2);
        assert!(children
            .iter()
            .all(|item| item.parent_work_item_id.as_deref() == Some("parent")));
    }
}
