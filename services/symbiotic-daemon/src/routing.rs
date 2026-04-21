use symbiotic_memory::recall_probes::RecallProbeTargetKind;

use crate::recall_probes::{
    DEFAULT_RECALL_PROBE_MAX_QUERIES_PER_SUBJECT, DEFAULT_RECALL_PROBE_MAX_SUBJECTS,
    DEFAULT_RECALL_PROBE_TOP_K,
};

/// Maps a room role (control, intake, alerts, status) to its Matrix room_id.
///
/// The daemon uses this map to route incoming messages by room_id rather than
/// relying on alias-pattern matching.  The installer writes the corresponding
/// `SYMBIOTIC_MATRIX_ROOM_*` env vars which are read at startup.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RoomRoleMap {
    pub control: Option<String>,
    pub intake: Option<String>,
    pub alerts: Option<String>,
    pub status: Option<String>,
    pub credentials: Option<String>,
    pub goals: Option<String>,
    /// The stream room aggregates all thread activity into a single feed.
    /// Thread rooms are dynamic (looked up via `ThreadRegistry`) and not
    /// stored in this static map.
    pub stream: Option<String>,
}

impl RoomRoleMap {
    /// Return the role for `room_id`, if any.
    pub fn role_for(&self, room_id: &str) -> Option<RoomRole> {
        if self.control.as_deref() == Some(room_id) {
            return Some(RoomRole::Control);
        }
        if self.intake.as_deref() == Some(room_id) {
            return Some(RoomRole::Intake);
        }
        if self.alerts.as_deref() == Some(room_id) {
            return Some(RoomRole::Alerts);
        }
        if self.status.as_deref() == Some(room_id) {
            return Some(RoomRole::Status);
        }
        if self.credentials.as_deref() == Some(room_id) {
            return Some(RoomRole::Credentials);
        }
        if self.goals.as_deref() == Some(room_id) {
            return Some(RoomRole::Goals);
        }
        if self.stream.as_deref() == Some(room_id) {
            return Some(RoomRole::Stream);
        }
        // Note: RoomRole::Thread is dynamic — looked up via ThreadRegistry,
        // not the static role map. It will never be returned from here.
        None
    }

    /// True when at least one room_id is configured.
    pub fn is_configured(&self) -> bool {
        self.control.is_some()
            || self.intake.is_some()
            || self.alerts.is_some()
            || self.status.is_some()
            || self.credentials.is_some()
            || self.goals.is_some()
            || self.stream.is_some()
    }

    /// Return the configured room_id for `role`, falling back to the default
    /// alias string (e.g. "#status", "#alerts") when no mapping is present.
    pub fn resolve(&self, role: RoomRole) -> &str {
        match role {
            RoomRole::Control => self.control.as_deref().unwrap_or("#control"),
            RoomRole::Intake => self.intake.as_deref().unwrap_or("#intake"),
            RoomRole::Alerts => self.alerts.as_deref().unwrap_or("#alerts"),
            RoomRole::Status => self.status.as_deref().unwrap_or("#status"),
            RoomRole::Credentials => self.credentials.as_deref().unwrap_or("#credentials"),
            RoomRole::Goals => self.goals.as_deref().unwrap_or("#goals"),
            RoomRole::Stream => self.stream.as_deref().unwrap_or("#stream"),
            // Thread rooms are dynamic — looked up via ThreadRegistry.
            // This fallback should never be reached in practice.
            RoomRole::Thread => "#thread",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoomRole {
    Control,
    Intake,
    Alerts,
    Status,
    Credentials,
    Goals,
    /// The stream room aggregates all thread activity into a single feed.
    Stream,
    /// Dynamic thread rooms — looked up via `ThreadRegistry`, not the
    /// static `RoomRoleMap`. Included here for match exhaustiveness.
    Thread,
}

pub(crate) const MAX_BOOKMARKS_SYNC_LIMIT: u32 = 500;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ControlCommand {
    GoalStart {
        template: String,
    },
    GoalRetry {
        template: String,
    },
    GoalStop {
        template: String,
    },
    GoalDeliberate {
        description: String,
    },
    GoalList,
    RunWorkflow {
        template: String,
    },
    InstallRun {
        mode: String,
        install_id: Option<String>,
    },
    InstallProvision {
        mode: String,
        install_id: Option<String>,
    },
    InstallBootstrap,
    InstallVerify {
        mode: String,
        install_id: Option<String>,
    },
    IssueAuth {
        target: String,
        scopes: Vec<String>,
    },
    CredentialSubmit {
        service: String,
        username: String,
        secret: String,
        totp_secret: Option<String>,
    },
    CredentialQuery {
        service: String,
    },
    CredentialRemove {
        service: String,
    },
    /// API credential submit (key=value format, e.g. ANTHROPIC_API_KEY).
    ApiCredentialSubmit {
        key: String,
        value: String,
        validate: bool,
    },
    /// API credential query (one or more comma-separated keys).
    ApiCredentialQuery {
        keys: Vec<String>,
    },
    /// API credential remove (single key).
    ApiCredentialRemove {
        key: String,
    },
    /// Browser-automated authentication for a domain.
    CredentialAuthenticate {
        domain: String,
    },
    CredentialApprove {
        request_id: String,
        remember_for_secs: Option<u64>,
    },
    CredentialDeny {
        request_id: String,
        reason: Option<String>,
    },
    CredentialRespond {
        request_id: String,
        value: String,
    },
    CredentialApprovalPolicyList {
        include_inactive: bool,
    },
    CredentialApprovalPolicyRevoke {
        policy_id: String,
    },
    RecallProbeRun {
        top_k: usize,
        max_subjects: usize,
        max_queries_per_subject: usize,
    },
    RecallProbeStatus {
        run_id: String,
    },
    RecallProbeSummary {
        target_kind: RecallProbeTargetKind,
        target_id: String,
    },
    RecallProbeHealth {
        limit: usize,
    },
    RecallProbeRegressions {
        run_id: String,
        limit: usize,
    },
    BookmarksSync {
        source: String,
        limit: u32,
    },
    RegisterPush {
        device_id: String,
        token: String,
        platform: String,
    },
    PushAck {
        notification_id: String,
        run_id: Option<String>,
        device_id: Option<String>,
    },
    UnregisterPush {
        device_id: String,
    },
    UpdatePushPreferences {
        failures: Option<bool>,
        goal_completions: Option<bool>,
        capture_confirmations: Option<bool>,
        install_progress: Option<bool>,
    },
    InstallSecretPut {
        mode: String,
        key: String,
        value: String,
    },
    InstallSecretValidate {
        mode: String,
    },
    KeyRotationStart,
    /// Approve a system-originated proposal, converting it to a goal.
    ProposalApprove {
        proposal_id: String,
    },
    /// Dismiss a system-originated proposal without executing it.
    ProposalDismiss {
        proposal_id: String,
    },
    /// Approve a pending approval ticket.
    /// Syntax: `approve <ticket_id>`
    ApprovalApprove {
        ticket_id: String,
    },
    /// Deny a pending approval ticket with optional reason.
    /// Syntax: `deny <ticket_id>` or `deny <ticket_id> <reason words...>`
    ApprovalDeny {
        ticket_id: String,
        reason: Option<String>,
    },
    /// Request a diff inspection for a pending ticket.
    /// Syntax: `inspect <ticket_id>`
    ApprovalInspect {
        ticket_id: String,
    },
    Unknown {
        reason: String,
    },
}

/// Extracted command fields from v2 JSON format (sym.c).
/// Used by both `parse_control_command` and the goal handler.
pub(crate) struct ExtractedCommand {
    pub command: String,
    pub fields: serde_json::Map<String, serde_json::Value>,
}

/// Try to extract command + fields from a v2 JSON body (sym.c format).
///
/// v2: `{"msgtype":"sym.c", "body":"", "sym":{"v":2, "c":"goal.answer", "t":"thr-1", "d":{"message":"..."}}}`
pub(crate) fn extract_command_from_json(body: &str) -> Option<ExtractedCommand> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    let obj = value.as_object()?;

    // v2 detection: has msgtype "sym.c"
    if obj.get("msgtype").and_then(|v| v.as_str()) == Some("sym.c") {
        let sym = obj.get("sym")?.as_object()?;
        let command = sym.get("c").and_then(|v| v.as_str())?.to_string();
        let mut fields = serde_json::Map::new();
        // Flatten sym.d fields to top level for handler compatibility
        if let Some(d) = sym.get("d").and_then(|v| v.as_object()) {
            fields.extend(d.clone());
        }
        // sym.t is the messaging thread. Preserve it explicitly.
        if let Some(t) = sym.get("t").and_then(|v| v.as_str()) {
            fields.insert("thread_id".to_string(), serde_json::json!(t));
            // Legacy compatibility: some goal handlers still fall back to
            // goal_id when no explicit goal target is present.
            fields
                .entry("goal_id".to_string())
                .or_insert_with(|| serde_json::json!(t));
        }
        // sym.r (reply_to) preserved
        if let Some(r) = sym.get("r").and_then(|v| v.as_str()) {
            fields.insert("reply_to".to_string(), serde_json::json!(r));
        }
        return Some(ExtractedCommand { command, fields });
    }

    None
}

pub(crate) fn parse_control_command(body: &str) -> ControlCommand {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return ControlCommand::Unknown {
            reason: "empty command".to_string(),
        };
    }

    if trimmed.starts_with('{') {
        // Only v2 (sym.c) format is accepted
        if let Some(extracted) = extract_command_from_json(trimmed) {
            return parse_extracted_command(extracted);
        }
        return ControlCommand::Unknown {
            reason: "JSON command must use v2 sym.c format".to_string(),
        };
    }

    let command = trimmed.strip_prefix('/').map(str::trim).unwrap_or(trimmed);
    let parts = command.split_whitespace().collect::<Vec<_>>();

    if !parts.is_empty() && parts[0].eq_ignore_ascii_case("goal") {
        if parts.len() >= 2 && parts[1].eq_ignore_ascii_case("list") {
            return ControlCommand::GoalList;
        }
        if parts.len() >= 3
            && (parts[1].eq_ignore_ascii_case("start") || parts[1].eq_ignore_ascii_case("run"))
        {
            return ControlCommand::GoalStart {
                template: parts[2].to_ascii_lowercase(),
            };
        }
        if parts.len() >= 3 && parts[1].eq_ignore_ascii_case("retry") {
            return ControlCommand::GoalRetry {
                template: parts[2].to_ascii_lowercase(),
            };
        }
        if parts.len() >= 3 && parts[1].eq_ignore_ascii_case("stop") {
            return ControlCommand::GoalStop {
                template: parts[2].to_ascii_lowercase(),
            };
        }
        if parts.len() >= 3 && parts[1].eq_ignore_ascii_case("deliberate") {
            let description = parts[2..].join(" ");
            if description.is_empty() {
                return ControlCommand::Unknown {
                    reason: "goal deliberate requires a description".to_string(),
                };
            }
            return ControlCommand::GoalDeliberate { description };
        }
        return ControlCommand::Unknown {
            reason:
                "goal commands: goal list | goal start <template> | goal retry <template> | goal stop <template> | goal deliberate <description>"
                    .to_string(),
        };
    }

    // proposal approve <id> | proposal dismiss <id>
    if !parts.is_empty() && parts[0].eq_ignore_ascii_case("proposal") {
        if parts.len() >= 3 && parts[1].eq_ignore_ascii_case("approve") {
            return ControlCommand::ProposalApprove {
                proposal_id: parts[2].to_string(),
            };
        }
        if parts.len() >= 3 && parts[1].eq_ignore_ascii_case("dismiss") {
            return ControlCommand::ProposalDismiss {
                proposal_id: parts[2].to_string(),
            };
        }
        return ControlCommand::Unknown {
            reason: "proposal commands: proposal approve <id> | proposal dismiss <id>".to_string(),
        };
    }

    if parts.len() >= 2 && parts[0].eq_ignore_ascii_case("workflow") {
        return ControlCommand::RunWorkflow {
            template: parts[1].to_ascii_lowercase(),
        };
    }

    if parts.len() >= 2 && parts[0].eq_ignore_ascii_case("install") {
        if parts[1].eq_ignore_ascii_case("run") {
            let mode = parts
                .get(2)
                .map(|value| value.trim().to_ascii_lowercase())
                .unwrap_or_else(|| "byok".to_string());
            if mode != "byok" && mode != "managed" {
                return ControlCommand::Unknown {
                    reason: "install run mode must be byok or managed".to_string(),
                };
            }
            let install_id = parts
                .get(3)
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty());
            return ControlCommand::InstallRun { mode, install_id };
        }
        if parts[1].eq_ignore_ascii_case("provision") {
            if parts.len() > 4 {
                return ControlCommand::Unknown {
                    reason: "install provision supports at most [byok|managed] [install_id]"
                        .to_string(),
                };
            }
            let mut mode = "byok".to_string();
            let mut install_id_idx = 2usize;
            if let Some(raw_mode) = parts.get(2).map(|value| value.trim().to_ascii_lowercase()) {
                if raw_mode == "byok" || raw_mode == "managed" {
                    mode = raw_mode;
                    install_id_idx = 3;
                } else if parts.len() >= 4 {
                    return ControlCommand::Unknown {
                        reason: "install provision mode must be byok or managed".to_string(),
                    };
                }
            }
            let install_id = parts
                .get(install_id_idx)
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty());
            return ControlCommand::InstallProvision { mode, install_id };
        }
        if parts[1].eq_ignore_ascii_case("bootstrap") {
            return ControlCommand::InstallBootstrap;
        }
        if parts[1].eq_ignore_ascii_case("verify") {
            if parts.len() > 4 {
                return ControlCommand::Unknown {
                    reason: "install verify supports at most [byok|managed] [install_id]"
                        .to_string(),
                };
            }
            let mut mode = "byok".to_string();
            let mut install_id_idx = 2usize;
            if let Some(raw_mode) = parts.get(2).map(|value| value.trim().to_ascii_lowercase()) {
                if raw_mode == "byok" || raw_mode == "managed" {
                    mode = raw_mode;
                    install_id_idx = 3;
                } else if parts.len() >= 4 {
                    return ControlCommand::Unknown {
                        reason: "install verify mode must be byok or managed".to_string(),
                    };
                }
            }
            let install_id = parts
                .get(install_id_idx)
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty());
            return ControlCommand::InstallVerify { mode, install_id };
        }
        if parts.len() >= 4
            && parts[1].eq_ignore_ascii_case("secret")
            && parts[2].eq_ignore_ascii_case("put")
        {
            if parts.len() < 6 {
                return ControlCommand::Unknown {
                    reason: "install secret put requires <mode> <key> <value>".to_string(),
                };
            }
            let mode = parts[3].trim().to_ascii_lowercase();
            if mode != "byok" && mode != "managed" {
                return ControlCommand::Unknown {
                    reason: "install secret put mode must be byok or managed".to_string(),
                };
            }
            let key = parts[4].trim().to_string();
            let value = parts[5..].join(" ");
            if key.is_empty() || value.is_empty() {
                return ControlCommand::Unknown {
                    reason: "install secret put requires non-empty key and value".to_string(),
                };
            }
            return ControlCommand::InstallSecretPut { mode, key, value };
        }
        if parts.len() >= 3
            && parts[1].eq_ignore_ascii_case("secret")
            && parts[2].eq_ignore_ascii_case("validate")
        {
            let mode = parts
                .get(3)
                .map(|value| value.trim().to_ascii_lowercase())
                .unwrap_or_else(|| "byok".to_string());
            if mode != "byok" && mode != "managed" {
                return ControlCommand::Unknown {
                    reason: "install secret validate mode must be byok or managed".to_string(),
                };
            }
            return ControlCommand::InstallSecretValidate { mode };
        }
        return ControlCommand::Unknown {
            reason: "supported install commands: install run [byok|managed] [install_id], install provision [byok|managed] [install_id], install bootstrap, install verify [byok|managed] [install_id], install secret put <mode> <key> <value>, install secret validate [byok|managed]".to_string(),
        };
    }

    if parts.len() >= 3
        && parts[0].eq_ignore_ascii_case("auth")
        && parts[1].eq_ignore_ascii_case("issue")
    {
        let target = parts[2].to_ascii_lowercase();
        let scopes = if parts.len() >= 4 {
            parts[3]
                .split(',')
                .map(str::trim)
                .filter(|scope| !scope.is_empty())
                .map(|scope| scope.to_ascii_lowercase())
                .collect::<Vec<_>>()
        } else {
            vec!["web.login".to_string()]
        };
        if scopes.is_empty() {
            return ControlCommand::Unknown {
                reason: "auth.issue requires one scope".to_string(),
            };
        }
        return ControlCommand::IssueAuth { target, scopes };
    }

    if parts.len() >= 3
        && parts[0].eq_ignore_ascii_case("credential")
        && parts[1].eq_ignore_ascii_case("approve")
    {
        return ControlCommand::CredentialApprove {
            request_id: parts[2].trim().to_string(),
            remember_for_secs: parts.get(3).and_then(|value| value.parse::<u64>().ok()),
        };
    }

    if parts.len() >= 3
        && parts[0].eq_ignore_ascii_case("credential")
        && parts[1].eq_ignore_ascii_case("deny")
    {
        return ControlCommand::CredentialDeny {
            request_id: parts[2].trim().to_string(),
            reason: parts
                .get(3)
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty()),
        };
    }

    if parts.len() >= 4
        && parts[0].eq_ignore_ascii_case("credential")
        && parts[1].eq_ignore_ascii_case("respond")
    {
        return ControlCommand::CredentialRespond {
            request_id: parts[2].trim().to_string(),
            value: parts[3].trim().to_string(),
        };
    }

    if parts.len() >= 3
        && parts[0].eq_ignore_ascii_case("recall")
        && parts[1].eq_ignore_ascii_case("probe")
    {
        if parts[2].eq_ignore_ascii_case("run") {
            let max_subjects = match parts.get(3) {
                Some(raw) => match raw.parse::<usize>() {
                    Ok(value) if value > 0 => value,
                    Ok(_) => {
                        return ControlCommand::Unknown {
                            reason: "recall probe run max_subjects must be >= 1".to_string(),
                        };
                    }
                    Err(_) => {
                        return ControlCommand::Unknown {
                            reason: "recall probe run max_subjects must be a positive integer"
                                .to_string(),
                        };
                    }
                },
                None => DEFAULT_RECALL_PROBE_MAX_SUBJECTS,
            };
            return ControlCommand::RecallProbeRun {
                top_k: DEFAULT_RECALL_PROBE_TOP_K,
                max_subjects,
                max_queries_per_subject: DEFAULT_RECALL_PROBE_MAX_QUERIES_PER_SUBJECT,
            };
        }
        if parts.len() >= 4 && parts[2].eq_ignore_ascii_case("status") {
            return ControlCommand::RecallProbeStatus {
                run_id: parts[3].trim().to_string(),
            };
        }
        if parts.len() >= 5 && parts[2].eq_ignore_ascii_case("summary") {
            let target_kind = match parts[3].trim().parse::<RecallProbeTargetKind>() {
                Ok(kind) => kind,
                Err(_) => {
                    return ControlCommand::Unknown {
                        reason:
                            "recall probe summary target_kind must be archive_entry or graph_entity"
                                .to_string(),
                    };
                }
            };
            return ControlCommand::RecallProbeSummary {
                target_kind,
                target_id: parts[4].trim().to_string(),
            };
        }
        if parts[2].eq_ignore_ascii_case("health") {
            let limit = match parts.get(3) {
                Some(raw) => match raw.parse::<usize>() {
                    Ok(value) if value > 0 => value,
                    Ok(_) => {
                        return ControlCommand::Unknown {
                            reason: "recall probe health limit must be >= 1".to_string(),
                        };
                    }
                    Err(_) => {
                        return ControlCommand::Unknown {
                            reason: "recall probe health limit must be a positive integer"
                                .to_string(),
                        };
                    }
                },
                None => 20,
            };
            return ControlCommand::RecallProbeHealth { limit };
        }
        if parts.len() >= 4 && parts[2].eq_ignore_ascii_case("regressions") {
            let run_id = parts[3].trim().to_string();
            if run_id.is_empty() {
                return ControlCommand::Unknown {
                    reason: "recall probe regressions requires run_id".to_string(),
                };
            }
            let limit = match parts.get(4) {
                Some(raw) => match raw.parse::<usize>() {
                    Ok(value) if value > 0 => value,
                    Ok(_) => {
                        return ControlCommand::Unknown {
                            reason: "recall probe regressions limit must be >= 1".to_string(),
                        };
                    }
                    Err(_) => {
                        return ControlCommand::Unknown {
                            reason: "recall probe regressions limit must be a positive integer"
                                .to_string(),
                        };
                    }
                },
                None => 20,
            };
            return ControlCommand::RecallProbeRegressions { run_id, limit };
        }
        return ControlCommand::Unknown {
            reason: "recall probe commands: recall probe run [max_subjects] | recall probe status <run_id> | recall probe summary <archive_entry|graph_entity> <target_id> | recall probe health [limit] | recall probe regressions <run_id> [limit]".to_string(),
        };
    }

    if parts.len() >= 2
        && parts[0].eq_ignore_ascii_case("bookmarks")
        && parts[1].eq_ignore_ascii_case("sync")
    {
        let source = parts
            .get(2)
            .map(|value| value.to_ascii_lowercase())
            .unwrap_or_else(|| "api".to_string());
        if source != "api" && source != "browser" {
            return ControlCommand::Unknown {
                reason: "bookmarks source must be api or browser".to_string(),
            };
        }
        let limit = match parts.get(3) {
            Some(raw) => match raw.parse::<u32>() {
                Ok(value) if value > 0 => value,
                Ok(_) => {
                    return ControlCommand::Unknown {
                        reason: "bookmarks sync limit must be >= 1".to_string(),
                    };
                }
                Err(_) => {
                    return ControlCommand::Unknown {
                        reason: "bookmarks sync limit must be a positive integer".to_string(),
                    };
                }
            },
            None => 100,
        };
        if limit > MAX_BOOKMARKS_SYNC_LIMIT {
            return ControlCommand::Unknown {
                reason: format!("bookmarks sync limit must be <= {MAX_BOOKMARKS_SYNC_LIMIT}"),
            };
        }
        return ControlCommand::BookmarksSync { source, limit };
    }

    if parts.len() >= 5
        && parts[0].eq_ignore_ascii_case("push")
        && parts[1].eq_ignore_ascii_case("register")
    {
        let device_id = parts[2].trim().to_string();
        let platform = parts[3].trim().to_ascii_lowercase();
        let token = parts[4].trim().to_string();
        if device_id.is_empty() || token.is_empty() || platform.is_empty() {
            return ControlCommand::Unknown {
                reason: "push.register requires device_id, platform, token".to_string(),
            };
        }
        return ControlCommand::RegisterPush {
            device_id,
            token,
            platform,
        };
    }

    if parts.len() >= 2
        && parts[0].eq_ignore_ascii_case("key")
        && parts[1].eq_ignore_ascii_case("rotate")
    {
        return ControlCommand::KeyRotationStart;
    }

    if parts.len() >= 3
        && parts[0].eq_ignore_ascii_case("push")
        && parts[1].eq_ignore_ascii_case("ack")
    {
        let notification_id = parts[2].trim().to_string();
        if notification_id.is_empty() {
            return ControlCommand::Unknown {
                reason: "push.ack requires notification_id".to_string(),
            };
        }
        let run_id = parts
            .get(3)
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());
        let device_id = parts
            .get(4)
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());
        return ControlCommand::PushAck {
            notification_id,
            run_id,
            device_id,
        };
    }

    if parts.len() >= 3
        && parts[0].eq_ignore_ascii_case("push")
        && parts[1].eq_ignore_ascii_case("unregister")
    {
        let device_id = parts[2].trim().to_string();
        if device_id.is_empty() {
            return ControlCommand::Unknown {
                reason: "push.unregister requires device_id".to_string(),
            };
        }
        return ControlCommand::UnregisterPush { device_id };
    }

    // approve <ticket_id>
    if !parts.is_empty() && parts[0].eq_ignore_ascii_case("approve") {
        if parts.len() == 2 {
            return ControlCommand::ApprovalApprove {
                ticket_id: parts[1].to_string(),
            };
        }
        return ControlCommand::Unknown {
            reason: "approve command: approve <ticket_id>".to_string(),
        };
    }

    // deny <ticket_id> [<reason...>]
    if !parts.is_empty() && parts[0].eq_ignore_ascii_case("deny") {
        if parts.len() >= 2 {
            let reason = if parts.len() > 2 {
                Some(parts[2..].join(" "))
            } else {
                None
            };
            return ControlCommand::ApprovalDeny {
                ticket_id: parts[1].to_string(),
                reason,
            };
        }
        return ControlCommand::Unknown {
            reason: "deny command: deny <ticket_id> [reason]".to_string(),
        };
    }

    // inspect <ticket_id>
    if !parts.is_empty() && parts[0].eq_ignore_ascii_case("inspect") {
        if parts.len() == 2 {
            return ControlCommand::ApprovalInspect {
                ticket_id: parts[1].to_string(),
            };
        }
        return ControlCommand::Unknown {
            reason: "inspect command: inspect <ticket_id>".to_string(),
        };
    }

    ControlCommand::Unknown {
        reason: "supported commands: goal list|start|retry|stop, workflow <template>, install run [byok|managed] [install_id], install provision [byok|managed] [install_id], install bootstrap, install verify [byok|managed] [install_id], auth issue <target> [scopes], recall probe run [max_subjects], recall probe status <run_id>, recall probe summary <archive_entry|graph_entity> <target_id>, recall probe health [limit], recall probe regressions <run_id> [limit], bookmarks sync [api|browser] [limit], push register <device_id> <platform> <token>, push ack <notification_id> [run_id] [device_id], push unregister <device_id>, key rotate".to_string(),
    }
}

/// Parse a v2 command (extracted from sym.c format) into a ControlCommand.
fn parse_extracted_command(ext: ExtractedCommand) -> ControlCommand {
    let field_str = |key: &str| -> Option<String> {
        ext.fields
            .get(key)
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    };

    match ext.command.as_str() {
        "goal.start" => {
            let Some(template) = field_str("template") else {
                return ControlCommand::Unknown {
                    reason: "goal.start requires template".to_string(),
                };
            };
            ControlCommand::GoalStart {
                template: template.to_ascii_lowercase(),
            }
        }
        "goal.retry" => {
            let Some(template) = field_str("template") else {
                return ControlCommand::Unknown {
                    reason: "goal.retry requires template".to_string(),
                };
            };
            ControlCommand::GoalRetry {
                template: template.to_ascii_lowercase(),
            }
        }
        "goal.stop" => {
            let Some(template) = field_str("template") else {
                return ControlCommand::Unknown {
                    reason: "goal.stop requires template".to_string(),
                };
            };
            ControlCommand::GoalStop {
                template: template.to_ascii_lowercase(),
            }
        }
        "goal.list" => ControlCommand::GoalList,
        "goal.deliberate" => {
            let Some(description) = field_str("description") else {
                return ControlCommand::Unknown {
                    reason: "goal.deliberate requires description".to_string(),
                };
            };
            ControlCommand::GoalDeliberate { description }
        }
        "workflow.run" => {
            let Some(template) = field_str("template") else {
                return ControlCommand::Unknown {
                    reason: "workflow.run requires template".to_string(),
                };
            };
            ControlCommand::RunWorkflow {
                template: template.to_ascii_lowercase(),
            }
        }
        "install.run" => {
            let mode = field_str("mode").unwrap_or_else(|| "byok".to_string());
            let install_id = field_str("install_id");
            ControlCommand::InstallRun { mode, install_id }
        }
        "install.provision" => {
            let mode = field_str("mode").unwrap_or_else(|| "byok".to_string());
            let install_id = field_str("install_id");
            ControlCommand::InstallProvision { mode, install_id }
        }
        "install.bootstrap" => ControlCommand::InstallBootstrap,
        "install.verify" => {
            let mode = field_str("mode").unwrap_or_else(|| "byok".to_string());
            let install_id = field_str("install_id");
            ControlCommand::InstallVerify { mode, install_id }
        }
        "install.secret.put" => {
            let mode = field_str("mode").unwrap_or_else(|| "byok".to_string());
            let key = field_str("key").unwrap_or_default();
            let value = field_str("value").unwrap_or_default();
            ControlCommand::InstallSecretPut { mode, key, value }
        }
        "bookmarks.sync" => {
            let source = field_str("source").unwrap_or_else(|| "api".to_string());
            let limit = ext
                .fields
                .get("limit")
                .and_then(|v| v.as_u64())
                .unwrap_or(100) as u32;
            ControlCommand::BookmarksSync { source, limit }
        }
        "push.register" => {
            let Some(device_id) = field_str("device_id") else {
                return ControlCommand::Unknown {
                    reason: "push.register requires device_id".to_string(),
                };
            };
            let Some(token) = field_str("token") else {
                return ControlCommand::Unknown {
                    reason: "push.register requires token".to_string(),
                };
            };
            let Some(platform) = field_str("platform") else {
                return ControlCommand::Unknown {
                    reason: "push.register requires platform".to_string(),
                };
            };
            ControlCommand::RegisterPush {
                device_id,
                token,
                platform,
            }
        }
        "credential.submit" => {
            let service = field_str("service").unwrap_or_default();
            let username = field_str("username").unwrap_or_default();
            let secret = field_str("secret").unwrap_or_default();
            let totp_secret = field_str("totp_secret");
            ControlCommand::CredentialSubmit {
                service,
                username,
                secret,
                totp_secret,
            }
        }
        "credential.query" => {
            let service = field_str("service").unwrap_or_default();
            ControlCommand::CredentialQuery { service }
        }
        "credential.remove" => {
            let service = field_str("service").unwrap_or_default();
            ControlCommand::CredentialRemove { service }
        }
        "api_credential.submit" => {
            let key = field_str("key").unwrap_or_default();
            let value = field_str("value").unwrap_or_default();
            let validate = ext
                .fields
                .get("validate")
                .and_then(|v| v.as_bool())
                .unwrap_or(true);
            ControlCommand::ApiCredentialSubmit {
                key,
                value,
                validate,
            }
        }
        "api_credential.query" => {
            let keys_str = field_str("keys").unwrap_or_default();
            let keys: Vec<String> = keys_str
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            ControlCommand::ApiCredentialQuery { keys }
        }
        "api_credential.remove" => {
            let key = field_str("key").unwrap_or_default();
            ControlCommand::ApiCredentialRemove { key }
        }
        "credential.authenticate" => {
            let domain = field_str("domain").unwrap_or_default();
            ControlCommand::CredentialAuthenticate { domain }
        }
        "credential.approve" => {
            let request_id = field_str("request_id").unwrap_or_default();
            let remember_for_secs = ext
                .fields
                .get("remember_for_secs")
                .and_then(|value| value.as_u64());
            ControlCommand::CredentialApprove {
                request_id,
                remember_for_secs,
            }
        }
        "credential.deny" => {
            let request_id = field_str("request_id").unwrap_or_default();
            let reason = field_str("reason");
            ControlCommand::CredentialDeny { request_id, reason }
        }
        "credential.respond" => {
            let request_id = field_str("request_id").unwrap_or_default();
            let value = field_str("value").unwrap_or_default();
            ControlCommand::CredentialRespond { request_id, value }
        }
        "credential.approval_policy.list" => {
            let include_inactive = ext
                .fields
                .get("include_inactive")
                .and_then(|value| value.as_bool())
                .unwrap_or(false);
            ControlCommand::CredentialApprovalPolicyList { include_inactive }
        }
        "credential.approval_policy.revoke" => {
            let policy_id = field_str("policy_id").unwrap_or_default();
            ControlCommand::CredentialApprovalPolicyRevoke { policy_id }
        }
        "recall.probe.run" => {
            let top_k = ext
                .fields
                .get("top_k")
                .and_then(|value| value.as_u64())
                .unwrap_or(DEFAULT_RECALL_PROBE_TOP_K as u64) as usize;
            let max_subjects =
                ext.fields
                    .get("max_subjects")
                    .and_then(|value| value.as_u64())
                    .unwrap_or(DEFAULT_RECALL_PROBE_MAX_SUBJECTS as u64) as usize;
            let max_queries_per_subject = ext
                .fields
                .get("max_queries_per_subject")
                .and_then(|value| value.as_u64())
                .unwrap_or(DEFAULT_RECALL_PROBE_MAX_QUERIES_PER_SUBJECT as u64)
                as usize;
            if top_k == 0 || max_subjects == 0 || max_queries_per_subject == 0 {
                return ControlCommand::Unknown {
                    reason: "recall.probe.run requires positive top_k, max_subjects, and max_queries_per_subject".to_string(),
                };
            }
            ControlCommand::RecallProbeRun {
                top_k,
                max_subjects,
                max_queries_per_subject,
            }
        }
        "recall.probe.status" => {
            let Some(run_id) = field_str("run_id") else {
                return ControlCommand::Unknown {
                    reason: "recall.probe.status requires run_id".to_string(),
                };
            };
            ControlCommand::RecallProbeStatus { run_id }
        }
        "recall.probe.summary" => {
            let Some(target_kind_raw) = field_str("target_kind") else {
                return ControlCommand::Unknown {
                    reason: "recall.probe.summary requires target_kind".to_string(),
                };
            };
            let Ok(target_kind) = target_kind_raw.parse::<RecallProbeTargetKind>() else {
                return ControlCommand::Unknown {
                    reason:
                        "recall.probe.summary target_kind must be archive_entry or graph_entity"
                            .to_string(),
                };
            };
            let Some(target_id) = field_str("target_id") else {
                return ControlCommand::Unknown {
                    reason: "recall.probe.summary requires target_id".to_string(),
                };
            };
            ControlCommand::RecallProbeSummary {
                target_kind,
                target_id,
            }
        }
        "recall.probe.health" => {
            let limit = ext
                .fields
                .get("limit")
                .and_then(|value| value.as_u64())
                .unwrap_or(20) as usize;
            if limit == 0 {
                return ControlCommand::Unknown {
                    reason: "recall.probe.health requires a positive limit".to_string(),
                };
            }
            ControlCommand::RecallProbeHealth { limit }
        }
        "recall.probe.regressions" => {
            let Some(run_id) = field_str("run_id") else {
                return ControlCommand::Unknown {
                    reason: "recall.probe.regressions requires run_id".to_string(),
                };
            };
            let limit = ext
                .fields
                .get("limit")
                .and_then(|value| value.as_u64())
                .unwrap_or(20) as usize;
            if limit == 0 {
                return ControlCommand::Unknown {
                    reason: "recall.probe.regressions requires a positive limit".to_string(),
                };
            }
            ControlCommand::RecallProbeRegressions { run_id, limit }
        }
        "push.ack" => {
            let Some(notification_id) = field_str("notification_id") else {
                return ControlCommand::Unknown {
                    reason: "push.ack requires notification_id".to_string(),
                };
            };
            let run_id = field_str("run_id");
            let device_id = field_str("device_id");
            ControlCommand::PushAck {
                notification_id,
                run_id,
                device_id,
            }
        }
        "push.unregister" => {
            let Some(device_id) = field_str("device_id") else {
                return ControlCommand::Unknown {
                    reason: "push.unregister requires device_id".to_string(),
                };
            };
            ControlCommand::UnregisterPush { device_id }
        }
        "push.preferences" => {
            let failures = ext.fields.get("failures").and_then(|v| v.as_bool());
            let goal_completions = ext.fields.get("goal_completions").and_then(|v| v.as_bool());
            let capture_confirmations = ext
                .fields
                .get("capture_confirmations")
                .and_then(|v| v.as_bool());
            let install_progress = ext.fields.get("install_progress").and_then(|v| v.as_bool());
            ControlCommand::UpdatePushPreferences {
                failures,
                goal_completions,
                capture_confirmations,
                install_progress,
            }
        }
        "install.secret.validate" => {
            let mode = field_str("mode").unwrap_or_else(|| "byok".to_string());
            ControlCommand::InstallSecretValidate { mode }
        }
        "auth.issue" => {
            let Some(target) = field_str("target") else {
                return ControlCommand::Unknown {
                    reason: "auth.issue requires target".to_string(),
                };
            };
            let scopes = if let Some(raw) = ext.fields.get("scopes") {
                parse_scope_list(raw).unwrap_or_default()
            } else {
                vec!["web.login".to_string()]
            };
            if scopes.is_empty() {
                return ControlCommand::Unknown {
                    reason: "auth.issue requires one scope".to_string(),
                };
            }
            ControlCommand::IssueAuth {
                target: target.to_ascii_lowercase(),
                scopes,
            }
        }
        "key.rotate" => ControlCommand::KeyRotationStart,
        "proposal.approve" => {
            let Some(proposal_id) = field_str("proposal_id") else {
                return ControlCommand::Unknown {
                    reason: "proposal.approve requires proposal_id".to_string(),
                };
            };
            ControlCommand::ProposalApprove { proposal_id }
        }
        "proposal.dismiss" => {
            let Some(proposal_id) = field_str("proposal_id") else {
                return ControlCommand::Unknown {
                    reason: "proposal.dismiss requires proposal_id".to_string(),
                };
            };
            ControlCommand::ProposalDismiss { proposal_id }
        }
        // goal.answer, goal.plan.approved, goal.plan.rejected,
        // goal.task.transition, and goal.task.assign are handled by the goal handler
        // in commands.rs, not here.
        "goal.answer"
        | "goal.message"
        | "goal.plan.approved"
        | "goal.plan.rejected"
        | "goal.task.transition"
        | "goal.task.assign" => ControlCommand::Unknown {
            reason: format!("goal command {} handled by goal handler", ext.command),
        },
        other => ControlCommand::Unknown {
            reason: format!("unknown v2 command: {other}"),
        },
    }
}

fn parse_scope_list(raw: &serde_json::Value) -> Option<Vec<String>> {
    if let Some(value) = raw.as_str() {
        let scopes = value
            .split(',')
            .map(str::trim)
            .filter(|scope| !scope.is_empty())
            .map(|scope| scope.to_ascii_lowercase())
            .collect::<Vec<_>>();
        return Some(scopes);
    }

    let values = raw.as_array()?;
    let mut scopes = Vec::new();
    for entry in values {
        let scope = entry.as_str()?;
        let normalized = scope.trim().to_ascii_lowercase();
        if normalized.is_empty() {
            continue;
        }
        scopes.push(normalized);
    }
    Some(scopes)
}

pub(crate) fn is_intake_room(room_id: &str) -> bool {
    let alias = room_alias(room_id);
    alias.eq_ignore_ascii_case("#intake") || alias.eq_ignore_ascii_case("intake")
}

pub(crate) fn is_control_room(room_id: &str) -> bool {
    let alias = room_alias(room_id);
    alias.eq_ignore_ascii_case("#control") || alias.eq_ignore_ascii_case("control")
}

pub(crate) fn is_status_room(room_id: &str) -> bool {
    let alias = room_alias(room_id);
    alias.eq_ignore_ascii_case("#status") || alias.eq_ignore_ascii_case("status")
}

pub(crate) fn is_goal_room(room_id: &str) -> bool {
    room_alias(room_id)
        .to_ascii_lowercase()
        .starts_with("#goal-")
}

pub(crate) fn is_goals_room(room_id: &str) -> bool {
    let alias = room_alias(room_id).to_ascii_lowercase();
    alias == "#goals" || alias == "goals"
}

pub(crate) fn is_stream_room(room_id: &str) -> bool {
    let alias = room_alias(room_id);
    alias.eq_ignore_ascii_case("#stream") || alias.eq_ignore_ascii_case("stream")
}

pub(crate) fn is_credentials_room(room_id: &str) -> bool {
    let alias = room_alias(room_id).to_ascii_lowercase();
    alias == "#credentials" || alias == "credentials" || alias.starts_with("#cred-")
}

pub(crate) fn is_alerts_room(room_id: &str) -> bool {
    let alias = room_alias(room_id);
    alias.eq_ignore_ascii_case("#alerts") || alias.eq_ignore_ascii_case("alerts")
}

pub(crate) fn room_alias(room_id: &str) -> &str {
    room_id.split(':').next().unwrap_or(room_id)
}

pub(crate) fn parse_goal_template(body: &str) -> Option<String> {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return None;
    }
    let command = trimmed.strip_prefix('/').map(str::trim).unwrap_or(trimmed);
    let parts = command.split_whitespace().collect::<Vec<_>>();
    if parts.len() < 2 {
        return None;
    }
    if parts[0].eq_ignore_ascii_case("run") || parts[0].eq_ignore_ascii_case("start") {
        return Some(parts[1].to_ascii_lowercase());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_approve_with_ticket_id() {
        let result = parse_control_command("approve abc-123");
        assert_eq!(
            result,
            ControlCommand::ApprovalApprove {
                ticket_id: "abc-123".to_string(),
            }
        );
    }

    #[test]
    fn parse_approve_missing_ticket_id() {
        let result = parse_control_command("approve");
        match result {
            ControlCommand::Unknown { reason } => {
                assert!(
                    reason.contains("approve"),
                    "expected reason to mention 'approve', got: {reason}"
                );
            }
            other => panic!("expected Unknown, got: {other:?}"),
        }
    }

    #[test]
    fn parse_approve_too_many_args() {
        let result = parse_control_command("approve abc extra");
        assert!(matches!(result, ControlCommand::Unknown { .. }));
    }

    #[test]
    fn parse_deny_without_reason() {
        let result = parse_control_command("deny xyz-456");
        assert_eq!(
            result,
            ControlCommand::ApprovalDeny {
                ticket_id: "xyz-456".to_string(),
                reason: None,
            }
        );
    }

    #[test]
    fn parse_deny_with_reason() {
        let result = parse_control_command("deny xyz-456 too risky to proceed");
        assert_eq!(
            result,
            ControlCommand::ApprovalDeny {
                ticket_id: "xyz-456".to_string(),
                reason: Some("too risky to proceed".to_string()),
            }
        );
    }

    #[test]
    fn parse_inspect_with_ticket_id() {
        let result = parse_control_command("inspect abc-123");
        assert_eq!(
            result,
            ControlCommand::ApprovalInspect {
                ticket_id: "abc-123".to_string(),
            }
        );
    }

    #[test]
    fn parse_inspect_missing_ticket_id() {
        let result = parse_control_command("inspect");
        assert!(matches!(result, ControlCommand::Unknown { .. }));
    }

    #[test]
    fn parse_approve_is_case_insensitive() {
        let mixed = parse_control_command("Approve abc-123");
        assert_eq!(
            mixed,
            ControlCommand::ApprovalApprove {
                ticket_id: "abc-123".to_string(),
            }
        );
        let upper = parse_control_command("APPROVE abc-123");
        assert_eq!(
            upper,
            ControlCommand::ApprovalApprove {
                ticket_id: "abc-123".to_string(),
            }
        );
    }

    #[test]
    fn extract_command_preserves_thread_id_and_goal_id() {
        let body = r#"{"msgtype":"sym.c","body":"","sym":{"v":2,"c":"goal.answer","t":"thread-abc","d":{"goal_id":"goal-123","message":"yes"}}}"#;
        let extracted = extract_command_from_json(body).expect("command");
        assert_eq!(
            extracted
                .fields
                .get("thread_id")
                .and_then(|value| value.as_str()),
            Some("thread-abc")
        );
        assert_eq!(
            extracted
                .fields
                .get("goal_id")
                .and_then(|value| value.as_str()),
            Some("goal-123")
        );
    }

    #[test]
    fn extract_command_uses_thread_id_as_legacy_goal_fallback() {
        let body = r#"{"msgtype":"sym.c","body":"","sym":{"v":2,"c":"goal.answer","t":"thread-abc","d":{"message":"yes"}}}"#;
        let extracted = extract_command_from_json(body).expect("command");
        assert_eq!(
            extracted
                .fields
                .get("thread_id")
                .and_then(|value| value.as_str()),
            Some("thread-abc")
        );
        assert_eq!(
            extracted
                .fields
                .get("goal_id")
                .and_then(|value| value.as_str()),
            Some("thread-abc")
        );
    }
}
