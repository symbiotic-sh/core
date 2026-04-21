pub mod device;
pub mod persistence;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum AgentTrustLevel {
    ReadOnly = 0,
    ArchiveWrite = 1,
    CredentialAccess = 2,
    ExternalAct = 3,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilityToken {
    pub token_id: String,
    pub subject: String,
    pub trust_level: AgentTrustLevel,
    pub scopes: HashSet<String>,
    pub expires_at: u64,
    pub one_time: bool,
    pub consumed: bool,
    /// Goal namespace this token is scoped to.
    /// - `Some("trading")` -- can only access `data/vault/trading/credentials/`
    /// - `None` -- can only access `data/vault/global/credentials/`
    #[serde(default)]
    pub goal_scope: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccessRequest {
    pub subject: String,
    pub required_level: AgentTrustLevel,
    pub scope: String,
    /// Goal namespace the request targets.
    /// Must match the token's `goal_scope` for access to be granted.
    #[serde(default)]
    pub goal_scope: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccessDecision {
    pub allowed: bool,
    pub reason: String,
}

#[derive(Debug, Error)]
pub enum BrokerError {
    #[error("token not found: {0}")]
    TokenNotFound(String),
    #[error("token subject mismatch for token: {0}")]
    SubjectMismatch(String),
    #[error("token expired: {0}")]
    TokenExpired(String),
    #[error("token already consumed: {0}")]
    TokenConsumed(String),
    #[error("insufficient trust level")]
    InsufficientTrust,
    #[error("scope not permitted: {0}")]
    ScopeDenied(String),
    #[error("goal scope mismatch: token has {token_scope:?}, request has {request_scope:?}")]
    GoalScopeMismatch {
        token_scope: Option<String>,
        request_scope: Option<String>,
    },
}

#[derive(Debug, Default)]
pub struct AccessBroker {
    tokens: HashMap<String, CapabilityToken>,
}

impl AccessBroker {
    pub fn new() -> Self {
        Self {
            tokens: HashMap::new(),
        }
    }

    pub fn issue_token(&mut self, token: CapabilityToken) {
        self.tokens.insert(token.token_id.clone(), token);
    }

    pub fn from_tokens(tokens: Vec<CapabilityToken>) -> Self {
        let mut map = HashMap::new();
        for token in tokens {
            map.insert(token.token_id.clone(), token);
        }
        Self { tokens: map }
    }

    pub fn tokens(&self) -> Vec<CapabilityToken> {
        let mut out = self.tokens.values().cloned().collect::<Vec<_>>();
        out.sort_by(|a, b| a.token_id.cmp(&b.token_id));
        out
    }

    /// Get a reference to a token by ID.
    pub fn get_token(&self, token_id: &str) -> Option<&CapabilityToken> {
        self.tokens.get(token_id)
    }

    /// Remove a token by ID. Returns true if it existed.
    pub fn remove_token(&mut self, token_id: &str) -> bool {
        self.tokens.remove(token_id).is_some()
    }

    pub fn evaluate(
        &mut self,
        token_id: &str,
        request: &AccessRequest,
        now: u64,
    ) -> Result<AccessDecision> {
        let token = self
            .tokens
            .get_mut(token_id)
            .ok_or_else(|| BrokerError::TokenNotFound(token_id.to_string()))?;

        if token.subject != request.subject {
            return Err(BrokerError::SubjectMismatch(token_id.to_string()).into());
        }
        if token.expires_at <= now {
            return Err(BrokerError::TokenExpired(token_id.to_string()).into());
        }
        if token.one_time && token.consumed {
            return Err(BrokerError::TokenConsumed(token_id.to_string()).into());
        }
        if token.trust_level < request.required_level {
            return Err(BrokerError::InsufficientTrust.into());
        }
        if !token.scopes.contains(&request.scope) {
            return Err(BrokerError::ScopeDenied(request.scope.clone()).into());
        }
        // Goal scope enforcement: token and request must target the same namespace.
        if token.goal_scope != request.goal_scope {
            return Err(BrokerError::GoalScopeMismatch {
                token_scope: token.goal_scope.clone(),
                request_scope: request.goal_scope.clone(),
            }
            .into());
        }

        if token.one_time {
            token.consumed = true;
        }

        Ok(AccessDecision {
            allowed: true,
            reason: "granted".to_string(),
        })
    }
}

// Re-export for backward compatibility with external callers.
pub use symbiotic_core::now_unix;

#[cfg(test)]
mod tests {
    use super::*;

    fn token(expires_at: u64, one_time: bool) -> CapabilityToken {
        CapabilityToken {
            token_id: "t1".to_string(),
            subject: "agent-a".to_string(),
            trust_level: AgentTrustLevel::ExternalAct,
            scopes: [
                "action.browser.login".to_string(),
                "archive.write".to_string(),
            ]
            .into_iter()
            .collect(),
            expires_at,
            one_time,
            consumed: false,
            goal_scope: None,
        }
    }

    #[test]
    fn grants_access_when_token_meets_requirements() {
        let now = now_unix();
        let mut broker = AccessBroker::new();
        broker.issue_token(token(now + 3600, false));

        let decision = broker
            .evaluate(
                "t1",
                &AccessRequest {
                    subject: "agent-a".to_string(),
                    required_level: AgentTrustLevel::ArchiveWrite,
                    scope: "archive.write".to_string(),
                    goal_scope: None,
                },
                now,
            )
            .expect("should grant");
        assert!(decision.allowed);
    }

    #[test]
    fn denies_expired_token() {
        let now = now_unix();
        let mut broker = AccessBroker::new();
        broker.issue_token(token(now - 1, false));

        let err = broker
            .evaluate(
                "t1",
                &AccessRequest {
                    subject: "agent-a".to_string(),
                    required_level: AgentTrustLevel::ArchiveWrite,
                    scope: "archive.write".to_string(),
                    goal_scope: None,
                },
                now,
            )
            .expect_err("should deny");
        assert!(err.to_string().contains("expired"));
    }

    #[test]
    fn denies_when_scope_missing() {
        let now = now_unix();
        let mut broker = AccessBroker::new();
        broker.issue_token(token(now + 3600, false));

        let err = broker
            .evaluate(
                "t1",
                &AccessRequest {
                    subject: "agent-a".to_string(),
                    required_level: AgentTrustLevel::ReadOnly,
                    scope: "credential.read".to_string(),
                    goal_scope: None,
                },
                now,
            )
            .expect_err("should deny");
        assert!(err.to_string().contains("scope not permitted"));
    }

    #[test]
    fn one_time_token_is_consumed() {
        let now = now_unix();
        let mut broker = AccessBroker::new();
        broker.issue_token(token(now + 3600, true));

        let first = broker
            .evaluate(
                "t1",
                &AccessRequest {
                    subject: "agent-a".to_string(),
                    required_level: AgentTrustLevel::ReadOnly,
                    scope: "archive.write".to_string(),
                    goal_scope: None,
                },
                now,
            )
            .expect("first should grant");
        assert!(first.allowed);

        let second = broker
            .evaluate(
                "t1",
                &AccessRequest {
                    subject: "agent-a".to_string(),
                    required_level: AgentTrustLevel::ReadOnly,
                    scope: "archive.write".to_string(),
                    goal_scope: None,
                },
                now,
            )
            .expect_err("second should fail");
        assert!(second.to_string().contains("already consumed"));
    }

    #[test]
    fn denies_subject_mismatch() {
        let now = now_unix();
        let mut broker = AccessBroker::new();
        broker.issue_token(CapabilityToken {
            token_id: "t-subj".to_string(),
            subject: "agent-1".to_string(),
            trust_level: AgentTrustLevel::ExternalAct,
            scopes: ["archive.write".to_string()].into_iter().collect(),
            expires_at: now + 3600,
            one_time: false,
            consumed: false,
            goal_scope: None,
        });

        let err = broker
            .evaluate(
                "t-subj",
                &AccessRequest {
                    subject: "agent-2".to_string(),
                    required_level: AgentTrustLevel::ReadOnly,
                    scope: "archive.write".to_string(),
                    goal_scope: None,
                },
                now,
            )
            .expect_err("should deny subject mismatch");
        let broker_err = err
            .downcast_ref::<BrokerError>()
            .expect("should be BrokerError");
        assert!(
            matches!(broker_err, BrokerError::SubjectMismatch(_)),
            "expected SubjectMismatch, got {broker_err:?}"
        );
    }

    #[test]
    fn denies_insufficient_trust_level() {
        let now = now_unix();
        let mut broker = AccessBroker::new();
        broker.issue_token(CapabilityToken {
            token_id: "t-trust".to_string(),
            subject: "agent-a".to_string(),
            trust_level: AgentTrustLevel::ReadOnly,
            scopes: ["archive.write".to_string()].into_iter().collect(),
            expires_at: now + 3600,
            one_time: false,
            consumed: false,
            goal_scope: None,
        });

        let err = broker
            .evaluate(
                "t-trust",
                &AccessRequest {
                    subject: "agent-a".to_string(),
                    required_level: AgentTrustLevel::CredentialAccess,
                    scope: "archive.write".to_string(),
                    goal_scope: None,
                },
                now,
            )
            .expect_err("should deny insufficient trust");
        let broker_err = err
            .downcast_ref::<BrokerError>()
            .expect("should be BrokerError");
        assert!(
            matches!(broker_err, BrokerError::InsufficientTrust),
            "expected InsufficientTrust, got {broker_err:?}"
        );
    }

    #[test]
    fn broker_roundtrips_tokens_collection() {
        let now = now_unix();
        let first = token(now + 100, false);
        let second = CapabilityToken {
            token_id: "t2".to_string(),
            subject: "agent-b".to_string(),
            trust_level: AgentTrustLevel::ReadOnly,
            scopes: ["archive.read".to_string()].into_iter().collect(),
            expires_at: now + 200,
            one_time: false,
            consumed: false,
            goal_scope: None,
        };

        let broker = AccessBroker::from_tokens(vec![first.clone(), second.clone()]);
        let tokens = broker.tokens();
        assert_eq!(tokens.len(), 2);
        assert_eq!(tokens[0].token_id, "t1");
        assert_eq!(tokens[1].token_id, "t2");
    }

    // --- Goal scope enforcement tests ---

    #[test]
    fn goal_scoped_token_grants_matching_goal_request() {
        let now = now_unix();
        let mut broker = AccessBroker::new();
        broker.issue_token(CapabilityToken {
            token_id: "t-goal".to_string(),
            subject: "agent-a".to_string(),
            trust_level: AgentTrustLevel::CredentialAccess,
            scopes: ["credential.read".to_string()].into_iter().collect(),
            expires_at: now + 3600,
            one_time: false,
            consumed: false,
            goal_scope: Some("trading".to_string()),
        });

        let decision = broker
            .evaluate(
                "t-goal",
                &AccessRequest {
                    subject: "agent-a".to_string(),
                    required_level: AgentTrustLevel::CredentialAccess,
                    scope: "credential.read".to_string(),
                    goal_scope: Some("trading".to_string()),
                },
                now,
            )
            .expect("matching goal scope should grant");
        assert!(decision.allowed);
    }

    #[test]
    fn goal_scoped_token_denies_different_goal_request() {
        let now = now_unix();
        let mut broker = AccessBroker::new();
        broker.issue_token(CapabilityToken {
            token_id: "t-goal-a".to_string(),
            subject: "agent-a".to_string(),
            trust_level: AgentTrustLevel::CredentialAccess,
            scopes: ["credential.read".to_string()].into_iter().collect(),
            expires_at: now + 3600,
            one_time: false,
            consumed: false,
            goal_scope: Some("trading".to_string()),
        });

        let err = broker
            .evaluate(
                "t-goal-a",
                &AccessRequest {
                    subject: "agent-a".to_string(),
                    required_level: AgentTrustLevel::CredentialAccess,
                    scope: "credential.read".to_string(),
                    goal_scope: Some("email".to_string()),
                },
                now,
            )
            .expect_err("trading token must not access email scope");
        let broker_err = err
            .downcast_ref::<BrokerError>()
            .expect("should be BrokerError");
        assert!(
            matches!(broker_err, BrokerError::GoalScopeMismatch { .. }),
            "expected GoalScopeMismatch, got {broker_err:?}"
        );
    }

    #[test]
    fn goal_scoped_token_cannot_access_global_credentials() {
        let now = now_unix();
        let mut broker = AccessBroker::new();
        broker.issue_token(CapabilityToken {
            token_id: "t-goal-only".to_string(),
            subject: "agent-a".to_string(),
            trust_level: AgentTrustLevel::CredentialAccess,
            scopes: ["credential.read".to_string()].into_iter().collect(),
            expires_at: now + 3600,
            one_time: false,
            consumed: false,
            goal_scope: Some("trading".to_string()),
        });

        let err = broker
            .evaluate(
                "t-goal-only",
                &AccessRequest {
                    subject: "agent-a".to_string(),
                    required_level: AgentTrustLevel::CredentialAccess,
                    scope: "credential.read".to_string(),
                    goal_scope: None, // global
                },
                now,
            )
            .expect_err("goal-scoped token must not access global credentials");
        let broker_err = err
            .downcast_ref::<BrokerError>()
            .expect("should be BrokerError");
        assert!(
            matches!(broker_err, BrokerError::GoalScopeMismatch { .. }),
            "expected GoalScopeMismatch, got {broker_err:?}"
        );
    }

    #[test]
    fn global_token_cannot_access_goal_scoped_credentials() {
        let now = now_unix();
        let mut broker = AccessBroker::new();
        broker.issue_token(CapabilityToken {
            token_id: "t-global".to_string(),
            subject: "agent-a".to_string(),
            trust_level: AgentTrustLevel::CredentialAccess,
            scopes: ["credential.read".to_string()].into_iter().collect(),
            expires_at: now + 3600,
            one_time: false,
            consumed: false,
            goal_scope: None,
        });

        let err = broker
            .evaluate(
                "t-global",
                &AccessRequest {
                    subject: "agent-a".to_string(),
                    required_level: AgentTrustLevel::CredentialAccess,
                    scope: "credential.read".to_string(),
                    goal_scope: Some("trading".to_string()),
                },
                now,
            )
            .expect_err("global token must not access goal-scoped credentials");
        let broker_err = err
            .downcast_ref::<BrokerError>()
            .expect("should be BrokerError");
        assert!(
            matches!(broker_err, BrokerError::GoalScopeMismatch { .. }),
            "expected GoalScopeMismatch, got {broker_err:?}"
        );
    }

    #[test]
    fn goal_scope_none_defaults_to_global() {
        let now = now_unix();
        let mut broker = AccessBroker::new();
        // Token without goal_scope (None = global)
        broker.issue_token(CapabilityToken {
            token_id: "t-default".to_string(),
            subject: "agent-a".to_string(),
            trust_level: AgentTrustLevel::CredentialAccess,
            scopes: ["credential.read".to_string()].into_iter().collect(),
            expires_at: now + 3600,
            one_time: false,
            consumed: false,
            goal_scope: None,
        });

        // Request also with None (global) should succeed
        let decision = broker
            .evaluate(
                "t-default",
                &AccessRequest {
                    subject: "agent-a".to_string(),
                    required_level: AgentTrustLevel::CredentialAccess,
                    scope: "credential.read".to_string(),
                    goal_scope: None,
                },
                now,
            )
            .expect("both None should match as global");
        assert!(decision.allowed);
    }

    #[test]
    fn goal_scope_serialization_backward_compat() {
        // Token JSON without goal_scope field should deserialize with None
        let json = r#"{
            "token_id": "t-old",
            "subject": "agent-a",
            "trust_level": "ExternalAct",
            "scopes": ["archive.read"],
            "expires_at": 9999999999,
            "one_time": false,
            "consumed": false
        }"#;
        let token: CapabilityToken =
            serde_json::from_str(json).expect("should deserialize without goal_scope");
        assert_eq!(token.goal_scope, None);
    }
}
