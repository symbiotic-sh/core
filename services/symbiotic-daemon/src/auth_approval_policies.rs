use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use credential_gateway::auth_engine::AuthExecutionAttestation;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthApprovalPolicy {
    pub policy_id: String,
    pub target: String,
    pub scopes: Vec<String>,
    pub auth_profile_id: String,
    pub auth_profile_sha256: String,
    pub created_at: u64,
    pub updated_at: u64,
    pub expires_at: u64,
    pub revoked_at: Option<u64>,
    pub created_from_request_id: String,
    pub created_by: String,
    pub purpose: String,
}

impl AuthApprovalPolicy {
    pub fn is_active(&self, now: u64) -> bool {
        self.revoked_at.is_none() && now < self.expires_at
    }
}

#[derive(Debug, Clone)]
pub struct AuthApprovalPolicyRequest {
    pub target: String,
    pub scopes: Vec<String>,
    pub attestation: AuthExecutionAttestation,
    pub created_from_request_id: String,
    pub created_by: String,
    pub purpose: String,
    pub ttl_secs: u64,
}

pub struct AuthApprovalPolicyStore {
    root: PathBuf,
    policies: HashMap<String, AuthApprovalPolicy>,
}

impl AuthApprovalPolicyStore {
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        fs::create_dir_all(&root).with_context(|| {
            format!(
                "failed to create auth approval policy directory {}",
                root.display()
            )
        })?;
        let mut policies = HashMap::new();
        for entry in fs::read_dir(&root).with_context(|| {
            format!(
                "failed to read auth approval policy directory {}",
                root.display()
            )
        })? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) != Some("json") {
                continue;
            }
            let body = fs::read_to_string(&path)
                .with_context(|| format!("failed to read policy file {}", path.display()))?;
            let policy: AuthApprovalPolicy = serde_json::from_str(&body)
                .with_context(|| format!("failed to parse policy file {}", path.display()))?;
            policies.insert(policy.policy_id.clone(), policy);
        }
        Ok(Self { root, policies })
    }

    pub fn create(
        &mut self,
        request: AuthApprovalPolicyRequest,
        now: u64,
    ) -> Result<AuthApprovalPolicy> {
        if request.ttl_secs == 0 {
            return Err(anyhow!("remember_for_secs must be > 0"));
        }
        let target = request.target.to_ascii_lowercase();
        let scopes = normalize_scopes(&request.scopes);
        let policy_id = new_policy_id(
            &target,
            &scopes,
            &request.attestation.profile_id,
            &request.attestation.script_sha256,
            now,
        );
        let policy = AuthApprovalPolicy {
            policy_id,
            target,
            scopes,
            auth_profile_id: request.attestation.profile_id,
            auth_profile_sha256: request.attestation.script_sha256,
            created_at: now,
            updated_at: now,
            expires_at: now + request.ttl_secs,
            revoked_at: None,
            created_from_request_id: request.created_from_request_id,
            created_by: request.created_by,
            purpose: request.purpose,
        };
        self.persist(&policy)?;
        self.policies
            .insert(policy.policy_id.clone(), policy.clone());
        Ok(policy)
    }

    pub fn find_matching(
        &self,
        target: &str,
        scopes: &[String],
        attestation: &AuthExecutionAttestation,
        now: u64,
    ) -> Option<AuthApprovalPolicy> {
        let target = target.to_ascii_lowercase();
        let scopes = normalize_scopes(scopes);
        self.policies
            .values()
            .find(|policy| {
                policy.is_active(now)
                    && policy.target == target
                    && policy.scopes == scopes
                    && policy.auth_profile_id == attestation.profile_id
                    && policy.auth_profile_sha256 == attestation.script_sha256
            })
            .cloned()
    }

    pub fn revoke(&mut self, policy_id: &str, now: u64) -> Result<AuthApprovalPolicy> {
        let mut policy = self
            .policies
            .get(policy_id)
            .cloned()
            .ok_or_else(|| anyhow!("auth approval policy not found: {policy_id}"))?;
        policy.revoked_at = Some(now);
        policy.updated_at = now;
        self.persist(&policy)?;
        self.policies
            .insert(policy.policy_id.clone(), policy.clone());
        Ok(policy)
    }

    pub fn list(&self, include_inactive: bool, now: u64) -> Vec<AuthApprovalPolicy> {
        let mut policies = self
            .policies
            .values()
            .filter(|policy| include_inactive || policy.is_active(now))
            .cloned()
            .collect::<Vec<_>>();
        policies.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        policies
    }

    fn persist(&self, policy: &AuthApprovalPolicy) -> Result<()> {
        let path = self.path_for(&policy.policy_id);
        let body = serde_json::to_string_pretty(policy)?;
        fs::write(&path, body)
            .with_context(|| format!("failed to write auth approval policy {}", path.display()))
    }

    fn path_for(&self, policy_id: &str) -> PathBuf {
        self.root.join(format!("{policy_id}.json"))
    }
}

fn normalize_scopes(scopes: &[String]) -> Vec<String> {
    let set = scopes
        .iter()
        .map(|scope| scope.trim().to_ascii_lowercase())
        .filter(|scope| !scope.is_empty())
        .collect::<BTreeSet<_>>();
    set.into_iter().collect()
}

fn new_policy_id(
    target: &str,
    scopes: &[String],
    auth_profile_id: &str,
    auth_profile_sha256: &str,
    now: u64,
) -> String {
    format!(
        "authpol_{:x}",
        crate::events::simple_hash(&format!(
            "{target}|{}|{auth_profile_id}|{auth_profile_sha256}|{now}",
            scopes.join(",")
        ))
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_attestation() -> AuthExecutionAttestation {
        AuthExecutionAttestation {
            requested_domain: "github.com".to_string(),
            profile_id: "github.com".to_string(),
            match_kind: credential_gateway::auth_engine::AuthProfileMatchKind::Exact,
            script_kind: credential_gateway::auth_engine::AuthProfileScriptKind::Shell,
            script_sha256: "a".repeat(64),
        }
    }

    #[test]
    fn create_and_match_policy() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut store = AuthApprovalPolicyStore::open(dir.path()).expect("open store");
        let policy = store
            .create(
                AuthApprovalPolicyRequest {
                    target: "GitHub.com".to_string(),
                    scopes: vec!["web.login".to_string(), "web.login".to_string()],
                    attestation: sample_attestation(),
                    created_from_request_id: "authreq_1".to_string(),
                    created_by: "@user:test".to_string(),
                    purpose: "Open GitHub settings".to_string(),
                    ttl_secs: 3600,
                },
                100,
            )
            .expect("create policy");

        let matched = store
            .find_matching(
                "github.com",
                &["web.login".to_string()],
                &sample_attestation(),
                200,
            )
            .expect("policy should match");
        assert_eq!(matched.policy_id, policy.policy_id);
        assert_eq!(matched.target, "github.com");
        assert_eq!(matched.scopes, vec!["web.login"]);
    }

    #[test]
    fn revoked_policy_no_longer_matches() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut store = AuthApprovalPolicyStore::open(dir.path()).expect("open store");
        let policy = store
            .create(
                AuthApprovalPolicyRequest {
                    target: "github.com".to_string(),
                    scopes: vec!["web.login".to_string()],
                    attestation: sample_attestation(),
                    created_from_request_id: "authreq_1".to_string(),
                    created_by: "@user:test".to_string(),
                    purpose: "Open GitHub settings".to_string(),
                    ttl_secs: 3600,
                },
                100,
            )
            .expect("create policy");
        store.revoke(&policy.policy_id, 150).expect("revoke policy");

        assert!(store
            .find_matching(
                "github.com",
                &["web.login".to_string()],
                &sample_attestation(),
                200,
            )
            .is_none());
    }
}
