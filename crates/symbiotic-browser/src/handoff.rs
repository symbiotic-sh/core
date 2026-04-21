//! Login handoff state machine for cloud-to-script credential flow.
//!
//! When a browser session expires, the system hands off authentication
//! to the Auth Script Engine or user. Cloud AI never handles credentials.
//!
//! State machine: Detecting -> HandoffRequested -> BrowserLaunched ->
//! AwaitingLogin -> Validating -> SessionRestored

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// The current state of a login handoff.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HandoffState {
    /// Cloud AI has detected an authentication failure.
    Detecting,
    /// Handoff has been requested; waiting for Nucleus to launch browser.
    HandoffRequested,
    /// Browser has been launched for user/Auth Script Engine login.
    BrowserLaunched,
    /// Waiting for user or Auth Script Engine to complete login.
    AwaitingLogin,
    /// Validating that the login was successful.
    Validating,
    /// Session has been restored; cloud AI can resume.
    SessionRestored,
    /// Handoff failed after exhausting retries.
    Failed,
}

/// Who will perform the login.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoginActor {
    /// Auth Script Engine drives the browser through login via deterministic scripts.
    AuthScript,
    /// User logs in directly without AI assistance.
    User,
}

/// Events that drive the handoff state machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HandoffEvent {
    /// Cloud AI confirmed the session is expired.
    SessionExpired,
    /// Nucleus launched a headed browser window.
    BrowserOpened,
    /// Login process has started (user or Auth Script Engine is active).
    LoginStarted,
    /// Login appears to be complete (check for success indicators).
    LoginAttempted,
    /// Session validation passed.
    ValidationPassed,
    /// Session validation failed (can retry).
    ValidationFailed,
    /// Handoff has timed out or exhausted retries.
    TimedOut,
}

/// Errors specific to the handoff state machine.
#[derive(Debug, Error)]
pub enum HandoffError {
    #[error("invalid transition from {from:?} on event {event:?}")]
    InvalidTransition {
        from: HandoffState,
        event: HandoffEvent,
    },
    #[error("handoff failed: {reason}")]
    Failed { reason: String },
}

/// Tracks a single login handoff process.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoginHandoff {
    /// The profile ID this handoff is for.
    pub profile_id: String,
    /// The domain requiring login.
    pub domain: String,
    /// Current state.
    pub state: HandoffState,
    /// Who is performing the login.
    pub actor: LoginActor,
    /// Number of validation attempts.
    pub validation_attempts: u32,
    /// Maximum validation attempts before failure.
    pub max_validation_attempts: u32,
    /// Timestamp when handoff was initiated.
    pub started_at: u64,
    /// Timeout in seconds for the entire handoff.
    pub timeout_secs: u64,
}

impl LoginHandoff {
    /// Create a new handoff in the Detecting state.
    pub fn new(profile_id: String, domain: String, actor: LoginActor, now: u64) -> Self {
        Self {
            profile_id,
            domain,
            state: HandoffState::Detecting,
            actor,
            validation_attempts: 0,
            max_validation_attempts: 3,
            started_at: now,
            timeout_secs: 300, // 5 minutes default
        }
    }

    /// Apply an event to advance the state machine.
    pub fn apply(&mut self, event: HandoffEvent) -> Result<&HandoffState, HandoffError> {
        let new_state = match (&self.state, &event) {
            (HandoffState::Detecting, HandoffEvent::SessionExpired) => {
                HandoffState::HandoffRequested
            }
            (HandoffState::HandoffRequested, HandoffEvent::BrowserOpened) => {
                HandoffState::BrowserLaunched
            }
            (HandoffState::BrowserLaunched, HandoffEvent::LoginStarted) => {
                HandoffState::AwaitingLogin
            }
            (HandoffState::AwaitingLogin, HandoffEvent::LoginAttempted) => HandoffState::Validating,
            (HandoffState::Validating, HandoffEvent::ValidationPassed) => {
                HandoffState::SessionRestored
            }
            (HandoffState::Validating, HandoffEvent::ValidationFailed) => {
                self.validation_attempts += 1;
                if self.validation_attempts >= self.max_validation_attempts {
                    HandoffState::Failed
                } else {
                    HandoffState::AwaitingLogin
                }
            }
            // Any state can transition to Failed on timeout
            (_, HandoffEvent::TimedOut) => HandoffState::Failed,
            (from, event) => {
                return Err(HandoffError::InvalidTransition {
                    from: from.clone(),
                    event: event.clone(),
                });
            }
        };
        self.state = new_state;
        Ok(&self.state)
    }

    /// Check if the handoff has timed out given the current time.
    pub fn is_timed_out(&self, now: u64) -> bool {
        now.saturating_sub(self.started_at) >= self.timeout_secs
    }

    /// Check if the handoff is in a terminal state.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self.state,
            HandoffState::SessionRestored | HandoffState::Failed
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_handoff() -> LoginHandoff {
        LoginHandoff::new(
            "x-primary".to_string(),
            "x.com".to_string(),
            LoginActor::User,
            1700000000,
        )
    }

    #[test]
    fn handoff_state_serde_roundtrip() {
        let states = [
            HandoffState::Detecting,
            HandoffState::HandoffRequested,
            HandoffState::BrowserLaunched,
            HandoffState::AwaitingLogin,
            HandoffState::Validating,
            HandoffState::SessionRestored,
            HandoffState::Failed,
        ];
        for state in &states {
            let json = serde_json::to_string(state).expect("serialize");
            let parsed: HandoffState = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(&parsed, state);
        }
    }

    #[test]
    fn handoff_happy_path() {
        let mut h = make_handoff();
        assert_eq!(h.state, HandoffState::Detecting);

        h.apply(HandoffEvent::SessionExpired).unwrap();
        assert_eq!(h.state, HandoffState::HandoffRequested);

        h.apply(HandoffEvent::BrowserOpened).unwrap();
        assert_eq!(h.state, HandoffState::BrowserLaunched);

        h.apply(HandoffEvent::LoginStarted).unwrap();
        assert_eq!(h.state, HandoffState::AwaitingLogin);

        h.apply(HandoffEvent::LoginAttempted).unwrap();
        assert_eq!(h.state, HandoffState::Validating);

        h.apply(HandoffEvent::ValidationPassed).unwrap();
        assert_eq!(h.state, HandoffState::SessionRestored);

        assert!(h.is_terminal());
    }

    #[test]
    fn handoff_validation_retry_then_success() {
        let mut h = make_handoff();
        h.apply(HandoffEvent::SessionExpired).unwrap();
        h.apply(HandoffEvent::BrowserOpened).unwrap();
        h.apply(HandoffEvent::LoginStarted).unwrap();
        h.apply(HandoffEvent::LoginAttempted).unwrap();

        // First validation fails, goes back to AwaitingLogin
        h.apply(HandoffEvent::ValidationFailed).unwrap();
        assert_eq!(h.state, HandoffState::AwaitingLogin);
        assert_eq!(h.validation_attempts, 1);

        // User tries again
        h.apply(HandoffEvent::LoginAttempted).unwrap();
        h.apply(HandoffEvent::ValidationPassed).unwrap();
        assert_eq!(h.state, HandoffState::SessionRestored);
    }

    #[test]
    fn handoff_validation_exhausted_transitions_to_failed() {
        let mut h = make_handoff();
        h.max_validation_attempts = 2;
        h.apply(HandoffEvent::SessionExpired).unwrap();
        h.apply(HandoffEvent::BrowserOpened).unwrap();
        h.apply(HandoffEvent::LoginStarted).unwrap();

        // First attempt
        h.apply(HandoffEvent::LoginAttempted).unwrap();
        h.apply(HandoffEvent::ValidationFailed).unwrap();
        assert_eq!(h.state, HandoffState::AwaitingLogin);

        // Second attempt fails, exhausted
        h.apply(HandoffEvent::LoginAttempted).unwrap();
        h.apply(HandoffEvent::ValidationFailed).unwrap();
        assert_eq!(h.state, HandoffState::Failed);
        assert!(h.is_terminal());
    }

    #[test]
    fn handoff_timeout_from_any_state() {
        let mut h = make_handoff();
        h.apply(HandoffEvent::SessionExpired).unwrap();
        h.apply(HandoffEvent::BrowserOpened).unwrap();

        // Timeout from BrowserLaunched
        h.apply(HandoffEvent::TimedOut).unwrap();
        assert_eq!(h.state, HandoffState::Failed);
    }

    #[test]
    fn handoff_invalid_transition() {
        let mut h = make_handoff();
        let err = h.apply(HandoffEvent::BrowserOpened).unwrap_err();
        assert!(err.to_string().contains("invalid transition"));
    }

    #[test]
    fn handoff_is_timed_out() {
        let h = make_handoff();
        // Not timed out yet
        assert!(!h.is_timed_out(1700000000 + 299));
        // Exactly at timeout
        assert!(h.is_timed_out(1700000000 + 300));
        // Past timeout
        assert!(h.is_timed_out(1700000000 + 600));
    }

    #[test]
    fn handoff_not_terminal_in_progress() {
        let mut h = make_handoff();
        assert!(!h.is_terminal());
        h.apply(HandoffEvent::SessionExpired).unwrap();
        assert!(!h.is_terminal());
        h.apply(HandoffEvent::BrowserOpened).unwrap();
        assert!(!h.is_terminal());
    }

    #[test]
    fn login_actor_serde_roundtrip() {
        let actors = [LoginActor::AuthScript, LoginActor::User];
        for actor in &actors {
            let json = serde_json::to_string(actor).expect("serialize");
            let parsed: LoginActor = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(&parsed, actor);
        }
    }

    #[test]
    fn login_handoff_serde_roundtrip() {
        let h = make_handoff();
        let json = serde_json::to_string(&h).expect("serialize");
        let parsed: LoginHandoff = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed.profile_id, "x-primary");
        assert_eq!(parsed.domain, "x.com");
        assert_eq!(parsed.state, HandoffState::Detecting);
    }
}
