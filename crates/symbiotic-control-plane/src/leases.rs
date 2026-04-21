use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Lease {
    pub holder_agent_id: String,
    pub lease_started_at: i64,
    pub last_heartbeat_at: i64,
    pub expires_at: i64,
    pub heartbeat_interval_secs: u64,
    pub max_missed_heartbeats: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HeartbeatUpdate {
    pub work_item_id: String,
    pub agent_id: String,
    pub status: HeartbeatStatus,
    pub progress_summary: Option<String>,
    pub progress_percent: Option<u8>,
    pub needs_attention: bool,
    pub observed_at: i64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HeartbeatStatus {
    Alive,
    Blocked,
    AwaitingReview,
    Releasing,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct ManagementLeaseConfig {
    pub default_heartbeat_interval_secs: u64,
    pub default_lease_duration_secs: u64,
    pub default_max_missed_heartbeats: u32,
    pub blocked_grace_secs: u64,
}

impl Default for ManagementLeaseConfig {
    fn default() -> Self {
        Self {
            default_heartbeat_interval_secs: 30,
            default_lease_duration_secs: 120,
            default_max_missed_heartbeats: 2,
            blocked_grace_secs: 300,
        }
    }
}

impl Lease {
    pub fn new(
        holder_agent_id: String,
        lease_started_at: i64,
        heartbeat_interval_secs: u64,
        max_missed_heartbeats: u32,
    ) -> Self {
        let lease_duration_secs =
            heartbeat_interval_secs.saturating_mul(max_missed_heartbeats as u64 + 2);
        Self {
            holder_agent_id,
            lease_started_at,
            last_heartbeat_at: lease_started_at,
            expires_at: lease_started_at + lease_duration_secs as i64,
            heartbeat_interval_secs,
            max_missed_heartbeats,
        }
    }

    pub fn renew(&mut self, heartbeat: &HeartbeatUpdate, config: &ManagementLeaseConfig) {
        self.last_heartbeat_at = heartbeat.observed_at;
        let extension_secs = match heartbeat.status {
            HeartbeatStatus::Blocked => config.blocked_grace_secs,
            _ => config.default_lease_duration_secs,
        };
        self.expires_at = heartbeat.observed_at + extension_secs as i64;
    }

    pub fn is_expired(&self, now_ts: i64) -> bool {
        now_ts > self.expires_at
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocked_heartbeat_uses_blocked_grace_window() {
        let config = ManagementLeaseConfig::default();
        let mut lease = Lease::new("agent-1".to_string(), 100, 30, 2);
        let heartbeat = HeartbeatUpdate {
            work_item_id: "w1".to_string(),
            agent_id: "agent-1".to_string(),
            status: HeartbeatStatus::Blocked,
            progress_summary: Some("waiting".to_string()),
            progress_percent: Some(50),
            needs_attention: true,
            observed_at: 180,
        };
        lease.renew(&heartbeat, &config);
        assert_eq!(lease.expires_at, 180 + config.blocked_grace_secs as i64);
    }

    #[test]
    fn lease_expires_after_deadline() {
        let lease = Lease::new("agent-1".to_string(), 100, 30, 2);
        assert!(!lease.is_expired(220));
        assert!(lease.is_expired(221));
    }
}
