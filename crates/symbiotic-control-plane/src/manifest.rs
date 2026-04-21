//! Manifest parser for YAML frontmatter in Markdown files.
//!
//! Parses goal manifests, identity (SOUL.md), and preferences from the
//! the Archive (filesystem path: `knowledge-base/`). Uses `---` delimiters to split YAML frontmatter from
//! Markdown body.

use std::path::Path;

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};

use crate::repo_manifest::RepoManifest;
use crate::types::{
    AvailabilityRuleManifest, DesiredState, GoalEventManifest, GoalManifest, GoalTaskManifest,
    IdentityFrontmatter, IdentityManifest, PolicyScopeManifest, PreferencesFrontmatter,
    PreferencesManifest, ProcessManifest, ProjectManifest, SkillManifest,
};

/// Parses Markdown manifests from the Archive.
#[derive(Debug, Clone, Default)]
pub struct ManifestParser;

impl ManifestParser {
    pub fn new() -> Self {
        Self
    }

    /// Parse all manifests from the Archive root.
    ///
    /// Reads:
    /// - `identity/SOUL.md` for identity
    /// - `identity/preferences.md` for preferences
    /// - `operations/projects/*/project.md` for project manifests
    /// - `operations/skills/*/manifest.toml` for skill references
    ///
    /// Parse failures for individual files are logged and skipped (non-fatal).
    pub fn parse_all(&self, kb_path: &Path) -> Result<DesiredState> {
        let mut state = DesiredState::default();

        // Identity
        if let Some(soul_path) = self.resolve_identity_path(kb_path) {
            match self.parse_identity(&soul_path) {
                Ok(identity) => state.identity = Some(identity),
                Err(e) => tracing::warn!("failed to parse SOUL.md: {e}"),
            }
        }

        // Preferences
        if let Some(prefs_path) = self.resolve_preferences_path(kb_path) {
            match self.parse_preferences(&prefs_path) {
                Ok(prefs) => state.preferences = Some(prefs),
                Err(e) => tracing::warn!("failed to parse preferences.md: {e}"),
            }
        }

        // Projects, nested goals, nested processes
        if let Some(projects_dir) = self.resolve_projects_dir(kb_path) {
            if let Ok(entries) = std::fs::read_dir(&projects_dir) {
                for entry in entries.flatten() {
                    let project_path = entry.path().join("project.md");
                    if project_path.exists() {
                        match self.parse_project(&project_path) {
                            Ok(mut project) => {
                                project.goals = self.parse_project_goals(&entry.path());
                                project.processes = self.parse_project_processes(&entry.path());
                                project.resolved_repos = self.parse_project_repos(&entry.path());
                                state.processes.extend(project.processes.iter().cloned());
                                state.goals.extend(project.goals.iter().cloned());
                                state.projects.push(project);
                            }
                            Err(e) => tracing::warn!(
                                "failed to parse project manifest {}: {e}",
                                project_path.display()
                            ),
                        }
                    }
                }
                state
                    .projects
                    .sort_by(|left, right| left.slug.cmp(&right.slug));
                state
                    .processes
                    .sort_by(|left, right| left.id.cmp(&right.id));
                state
                    .goals
                    .sort_by(|left, right| left.slug.cmp(&right.slug));
            }
        }

        // Shared policy scopes
        if let Some(scopes_dir) = self.resolve_policy_scopes_dir(kb_path) {
            if let Ok(entries) = std::fs::read_dir(&scopes_dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.extension().and_then(|ext| ext.to_str()) != Some("md") {
                        continue;
                    }
                    match self.parse_policy_scope(&path) {
                        Ok(scope) => state.policy_scopes.push(scope),
                        Err(e) => {
                            tracing::warn!("failed to parse policy scope {}: {e}", path.display())
                        }
                    }
                }
                state
                    .policy_scopes
                    .sort_by(|left, right| left.id.cmp(&right.id));
            }
        }

        // Availability rules
        if let Some(availability_dir) = self.resolve_calendar_availability_dir(kb_path) {
            if let Ok(entries) = std::fs::read_dir(&availability_dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.extension().and_then(|ext| ext.to_str()) != Some("md") {
                        continue;
                    }
                    match self.parse_availability_rule(&path) {
                        Ok(rule) => state.availability_rules.push(rule),
                        Err(e) => tracing::warn!(
                            "failed to parse availability rule {}: {e}",
                            path.display()
                        ),
                    }
                }
                state.availability_rules.sort_by(|left, right| {
                    left.subject
                        .cmp(&right.subject)
                        .then(left.id.cmp(&right.id))
                });
            }
        }

        // Skills
        if let Some(skills_dir) = self.resolve_skills_dir(kb_path) {
            if let Ok(entries) = std::fs::read_dir(&skills_dir) {
                for entry in entries.flatten() {
                    let manifest_path = entry.path().join("manifest.toml");
                    if manifest_path.exists() {
                        state.skills.push(SkillManifest {
                            name: entry.file_name().to_string_lossy().to_string(),
                            path: manifest_path,
                        });
                    }
                }
            }
        }

        Ok(state)
    }

    pub fn resolve_identity_path(&self, kb_path: &Path) -> Option<std::path::PathBuf> {
        let path = kb_path.join("identity/SOUL.md");
        path.exists().then_some(path)
    }

    pub fn resolve_preferences_path(&self, kb_path: &Path) -> Option<std::path::PathBuf> {
        let path = kb_path.join("identity/preferences.md");
        path.exists().then_some(path)
    }

    pub fn resolve_projects_dir(&self, kb_path: &Path) -> Option<std::path::PathBuf> {
        let path = kb_path.join("operations/projects");
        path.is_dir().then_some(path)
    }

    /// Current daemon-owned goal directory.
    ///
    /// This remains available while runtime goal-writing still targets the
    /// flat Archive shape. The end-state parser path is `operations/projects/*`.
    pub fn resolve_goals_dir(&self, kb_path: &Path) -> Option<std::path::PathBuf> {
        let path = kb_path.join("operations/goals");
        path.is_dir().then_some(path)
    }

    pub fn resolve_policy_scopes_dir(&self, kb_path: &Path) -> Option<std::path::PathBuf> {
        let path = kb_path.join("operations/policy/scopes");
        path.is_dir().then_some(path)
    }

    pub fn resolve_calendar_availability_dir(&self, kb_path: &Path) -> Option<std::path::PathBuf> {
        let path = kb_path.join("operations/calendar/availability");
        path.is_dir().then_some(path)
    }

    pub fn resolve_skills_dir(&self, kb_path: &Path) -> Option<std::path::PathBuf> {
        let path = kb_path.join("operations/skills");
        path.is_dir().then_some(path)
    }

    /// Parse a single project manifest from a Markdown file with YAML frontmatter.
    pub fn parse_project(&self, path: &Path) -> Result<ProjectManifest> {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("reading project manifest: {}", path.display()))?;

        let (yaml_str, body) = split_frontmatter(&content)
            .with_context(|| format!("splitting frontmatter: {}", path.display()))?;

        let mut manifest: ProjectManifest = serde_yml::from_str(yaml_str)
            .with_context(|| format!("parsing YAML frontmatter: {}", path.display()))?;

        manifest.project_markdown = body.to_string();
        Ok(manifest)
    }

    /// Parse a single goal manifest from a Markdown file with YAML frontmatter.
    pub fn parse_goal(&self, path: &Path) -> Result<GoalManifest> {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("reading goal manifest: {}", path.display()))?;

        let (yaml_str, body) = split_frontmatter(&content)
            .with_context(|| format!("splitting frontmatter: {}", path.display()))?;

        let mut manifest: GoalManifest = serde_yml::from_str(yaml_str)
            .with_context(|| format!("parsing YAML frontmatter: {}", path.display()))?;

        manifest.plan_markdown = body.to_string();
        Ok(manifest)
    }

    pub fn parse_process(&self, path: &Path) -> Result<ProcessManifest> {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("reading process manifest: {}", path.display()))?;

        let (yaml_str, body) = split_frontmatter(&content)
            .with_context(|| format!("splitting frontmatter: {}", path.display()))?;

        let mut manifest: ProcessManifest = serde_yml::from_str(yaml_str)
            .with_context(|| format!("parsing YAML frontmatter: {}", path.display()))?;

        manifest.process_markdown = body.to_string();
        Ok(manifest)
    }

    pub fn parse_goal_task(&self, path: &Path) -> Result<GoalTaskManifest> {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("reading goal task manifest: {}", path.display()))?;

        let (yaml_str, body) = split_frontmatter(&content)
            .with_context(|| format!("splitting frontmatter: {}", path.display()))?;

        let mut manifest: GoalTaskManifest = serde_yml::from_str(yaml_str)
            .with_context(|| format!("parsing YAML frontmatter: {}", path.display()))?;

        manifest.task_markdown = body.to_string();
        Ok(manifest)
    }

    pub fn parse_goal_tasks(&self, goal_dir: &Path) -> Vec<GoalTaskManifest> {
        let tasks_dir = goal_dir.join("tasks");
        let Ok(entries) = std::fs::read_dir(&tasks_dir) else {
            return Vec::new();
        };
        let mut tasks = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("md") {
                continue;
            }
            match self.parse_goal_task(&path) {
                Ok(task) => tasks.push(task),
                Err(error) => tracing::warn!(
                    "failed to parse goal task manifest {}: {error}",
                    path.display()
                ),
            }
        }
        tasks.sort_by(|left, right| left.task_id.cmp(&right.task_id));
        tasks
    }

    pub fn parse_project_goals(&self, project_dir: &Path) -> Vec<GoalManifest> {
        let goals_dir = project_dir.join("goals");
        let Ok(entries) = std::fs::read_dir(&goals_dir) else {
            return Vec::new();
        };
        let mut goals = Vec::new();
        for entry in entries.flatten() {
            let goal_dir = entry.path();
            let plan_path = goal_dir.join("plan.md");
            if !plan_path.exists() {
                continue;
            }
            match self.parse_goal(&plan_path) {
                Ok(mut goal) => {
                    goal.tasks = self.parse_goal_tasks(&goal_dir);
                    goals.push(goal);
                }
                Err(error) => tracing::warn!(
                    "failed to parse goal manifest {}: {error}",
                    plan_path.display()
                ),
            }
        }
        goals.sort_by(|left, right| left.slug.cmp(&right.slug));
        goals
    }

    pub fn parse_project_processes(&self, project_dir: &Path) -> Vec<ProcessManifest> {
        let processes_dir = project_dir.join("processes");
        let Ok(entries) = std::fs::read_dir(&processes_dir) else {
            return Vec::new();
        };
        let mut processes = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("md") {
                continue;
            }
            match self.parse_process(&path) {
                Ok(process) => processes.push(process),
                Err(error) => tracing::warn!(
                    "failed to parse process manifest {}: {error}",
                    path.display()
                ),
            }
        }
        processes.sort_by(|left, right| left.id.cmp(&right.id));
        processes
    }

    /// Parse a single repo manifest from a Markdown file with YAML frontmatter.
    ///
    /// Runs `RepoManifest::validate()` as a load-time schema check; validation
    /// failures are surfaced as errors.
    pub fn parse_repo(&self, path: &Path) -> Result<RepoManifest> {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("reading repo manifest: {}", path.display()))?;

        let (yaml_str, body) = split_frontmatter(&content)
            .with_context(|| format!("splitting frontmatter: {}", path.display()))?;

        let mut manifest: RepoManifest = serde_yml::from_str(yaml_str)
            .with_context(|| format!("parsing YAML frontmatter: {}", path.display()))?;

        manifest.body_markdown = body.to_string();
        manifest
            .validate()
            .with_context(|| format!("validating repo manifest: {}", path.display()))?;
        Ok(manifest)
    }

    /// Fan-out over `{project_dir}/repos/*.md`. Per-file failures are logged
    /// and skipped, mirroring `parse_project_goals` / `parse_project_processes`.
    pub fn parse_project_repos(&self, project_dir: &Path) -> Vec<RepoManifest> {
        let repos_dir = project_dir.join("repos");
        let Ok(entries) = std::fs::read_dir(&repos_dir) else {
            return Vec::new();
        };
        let mut repos = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("md") {
                continue;
            }
            match self.parse_repo(&path) {
                Ok(repo) => repos.push(repo),
                Err(error) => {
                    tracing::warn!("failed to parse repo manifest {}: {error}", path.display())
                }
            }
        }
        repos.sort_by(|left, right| left.slug.cmp(&right.slug));
        repos
    }

    pub fn parse_goal_event(&self, path: &Path) -> Result<GoalEventManifest> {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("reading goal event manifest: {}", path.display()))?;

        let (yaml_str, body) = split_frontmatter(&content)
            .with_context(|| format!("splitting frontmatter: {}", path.display()))?;

        let mut manifest: GoalEventManifest = serde_yml::from_str(yaml_str)
            .with_context(|| format!("parsing YAML frontmatter: {}", path.display()))?;

        manifest.event_markdown = body.to_string();
        Ok(manifest)
    }

    pub fn parse_policy_scope(&self, path: &Path) -> Result<PolicyScopeManifest> {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("reading policy scope manifest: {}", path.display()))?;

        let (yaml_str, body) = split_frontmatter(&content)
            .with_context(|| format!("splitting frontmatter: {}", path.display()))?;

        let mut manifest: PolicyScopeManifest = serde_yml::from_str(yaml_str)
            .with_context(|| format!("parsing YAML frontmatter: {}", path.display()))?;

        manifest.body_markdown = body.to_string();
        Ok(manifest)
    }

    pub fn parse_availability_rule(&self, path: &Path) -> Result<AvailabilityRuleManifest> {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("reading availability rule manifest: {}", path.display()))?;

        let (yaml_str, body) = split_frontmatter(&content)
            .with_context(|| format!("splitting frontmatter: {}", path.display()))?;

        let mut manifest: AvailabilityRuleManifest = serde_yml::from_str(yaml_str)
            .with_context(|| format!("parsing YAML frontmatter: {}", path.display()))?;

        manifest.body_markdown = body.to_string();
        Ok(manifest)
    }

    /// Parse the SOUL identity manifest.
    pub fn parse_identity(&self, path: &Path) -> Result<IdentityManifest> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading identity: {}", path.display()))?;

        let content_hash = sha256_hex(&raw);

        let (yaml_str, body) = split_frontmatter(&raw)
            .with_context(|| format!("splitting frontmatter: {}", path.display()))?;

        let fm: IdentityFrontmatter = serde_yml::from_str(yaml_str)
            .with_context(|| format!("parsing identity YAML: {}", path.display()))?;

        Ok(IdentityManifest {
            version: fm.version,
            updated_at: fm.updated_at,
            content: body.to_string(),
            content_hash,
        })
    }

    /// Parse operator preferences.
    pub fn parse_preferences(&self, path: &Path) -> Result<PreferencesManifest> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading preferences: {}", path.display()))?;

        let content_hash = sha256_hex(&raw);

        let (yaml_str, body) = split_frontmatter(&raw)
            .with_context(|| format!("splitting frontmatter: {}", path.display()))?;

        let fm: PreferencesFrontmatter = serde_yml::from_str(yaml_str)
            .with_context(|| format!("parsing preferences YAML: {}", path.display()))?;

        Ok(PreferencesManifest {
            version: fm.version,
            updated_at: fm.updated_at,
            task_policy_defaults: fm.task_policy_defaults,
            content: body.to_string(),
            content_hash,
        })
    }
}

/// Split a Markdown file into YAML frontmatter and body.
///
/// Expects the file to start with `---`, followed by YAML, followed by
/// another `---`, followed by the Markdown body.
///
/// Returns `(yaml_str, body)`.
fn split_frontmatter(content: &str) -> Result<(&str, &str)> {
    let trimmed = content.trim_start();
    if !trimmed.starts_with("---") {
        anyhow::bail!("file does not start with YAML frontmatter delimiter (---)");
    }

    // Skip the opening ---
    let after_open = &trimmed[3..];
    let after_open = after_open.strip_prefix('\n').unwrap_or(after_open);

    // Find closing ---
    let closing_pos = after_open
        .find("\n---")
        .ok_or_else(|| anyhow::anyhow!("no closing frontmatter delimiter (---) found"))?;

    let yaml_str = &after_open[..closing_pos];
    let rest = &after_open[closing_pos + 4..]; // skip \n---
    let body = rest.strip_prefix('\n').unwrap_or(rest);

    Ok((yaml_str, body))
}

/// Compute SHA-256 hex digest of a string.
fn sha256_hex(input: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    hex::encode(hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write_file(dir: &Path, rel_path: &str, content: &str) {
        let path = dir.join(rel_path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, content).unwrap();
    }

    #[test]
    fn parse_goal_manifest() {
        let tmp = TempDir::new().unwrap();
        let content = r#"---
id: "550e8400-e29b-41d4-a716-446655440000"
project_id: "project:algorithmic-trading"
slug: algorithmic-trading
title: "Algorithmic Trading Research"
state: active
priority: 2
autonomy_level: semi
phase: research
plan_version: 3
process:
  type: persistent
  check_frequency: daily
  max_parallel_agents: 3
streams:
  - name: market-analysis
    domain: finance
    focus: "Analyze order books"
    autonomy: semi
domains: [finance, projects]
vault_namespace: goal-algorithmic-trading
constraints:
  budget_usd: 500.0
  time_horizon_days: 90
  risk_tolerance: medium
---

# Trading Plan

## Phase 1: Research
- [ ] Survey frameworks
"#;
        write_file(tmp.path(), "plan.md", content);

        let parser = ManifestParser::new();
        let goal = parser.parse_goal(&tmp.path().join("plan.md")).unwrap();

        assert_eq!(goal.slug, "algorithmic-trading");
        assert_eq!(goal.state, crate::types::GoalState::Active);
        assert_eq!(goal.priority, 2);
        assert_eq!(goal.autonomy_level, crate::types::AutonomyLevel::Semi);
        assert_eq!(goal.phase, crate::types::GoalPhase::Research);
        assert_eq!(goal.plan_version, 3);
        assert_eq!(goal.streams.len(), 1);
        assert_eq!(goal.streams[0].name, "market-analysis");
        assert_eq!(goal.domains, vec!["finance", "projects"]);
        assert_eq!(goal.vault_namespace, "goal-algorithmic-trading");
        assert_eq!(goal.constraints.budget_usd, Some(500.0));
        assert_eq!(
            goal.constraints.risk_tolerance,
            Some(crate::types::RiskTolerance::Medium)
        );
        assert!(goal.plan_markdown.contains("# Trading Plan"));
        assert!(goal.tasks.is_empty());
    }

    #[test]
    fn parse_identity_manifest() {
        let tmp = TempDir::new().unwrap();
        let content = r#"---
version: 1
updated_at: "2026-02-22T00:00:00Z"
---

# SOUL: System Operating Under Limits

## Core Directives
1. Sovereignty First
"#;
        write_file(tmp.path(), "SOUL.md", content);

        let parser = ManifestParser::new();
        let identity = parser.parse_identity(&tmp.path().join("SOUL.md")).unwrap();

        assert_eq!(identity.version, 1);
        assert_eq!(identity.updated_at, "2026-02-22T00:00:00Z");
        assert!(identity
            .content
            .contains("# SOUL: System Operating Under Limits"));
        assert!(!identity.content_hash.is_empty());
    }

    #[test]
    fn parse_preferences_manifest() {
        let tmp = TempDir::new().unwrap();
        let content = r#"---
version: 1
updated_at: "2026-02-22T00:00:00Z"
task_policy_defaults:
  evaluator:
    interval_secs: 30
    timezone: "Europe/Bratislava"
    lateness_basis: delivery_window_elapsed
    delivery_window:
      mode: outside_quiet_hours
      quiet_hours:
        start_local: "22:00"
        end_local: "08:00"
  declared_task_defaults:
    waiting:
      mode: notify_operator
      after_secs: 3600
---

# Operator Preferences

## Confidence Thresholds
- auto_approve_threshold: 0.85
"#;
        write_file(tmp.path(), "preferences.md", content);

        let parser = ManifestParser::new();
        let prefs = parser
            .parse_preferences(&tmp.path().join("preferences.md"))
            .unwrap();

        assert_eq!(prefs.version, 1);
        assert_eq!(prefs.task_policy_defaults.evaluator.interval_secs, Some(30));
        assert_eq!(
            prefs.task_policy_defaults.evaluator.timezone.as_deref(),
            Some("Europe/Bratislava")
        );
        assert_eq!(
            prefs.task_policy_defaults.evaluator.lateness_basis,
            Some(crate::types::GoalTaskLatenessBasis::DeliveryWindowElapsed)
        );
        assert_eq!(
            prefs
                .task_policy_defaults
                .evaluator
                .delivery_window
                .as_ref()
                .map(|value| value.mode),
            Some(crate::types::GoalTaskDeliveryWindowMode::OutsideQuietHours)
        );
        assert_eq!(
            prefs
                .task_policy_defaults
                .declared_task_defaults
                .waiting
                .as_ref()
                .and_then(|value| value.after_secs),
            Some(3600)
        );
        assert!(prefs.content.contains("auto_approve_threshold: 0.85"));
        assert!(!prefs.content_hash.is_empty());
    }

    #[test]
    fn parse_policy_scope_manifest() {
        let tmp = TempDir::new().unwrap();
        let content = r#"---
id: "team:infra"
kind: team
title: "Infrastructure Team"
enabled: true
priority: 200
delivery_subject: "team:infra"
task_policy_defaults:
  evaluator:
    timezone: "Europe/Bratislava"
  waiting:
    mode: notify_operator
    audience: "team:infra"
    severity: urgent
    after_secs: 3600
---

# Infrastructure Team
"#;
        write_file(tmp.path(), "team-infra.md", content);

        let parser = ManifestParser::new();
        let scope = parser
            .parse_policy_scope(&tmp.path().join("team-infra.md"))
            .unwrap();

        assert_eq!(scope.id, "team:infra");
        assert_eq!(scope.kind, crate::types::PolicyScopeKind::Team);
        assert_eq!(scope.priority, 200);
        assert_eq!(scope.delivery_subject.as_deref(), Some("team:infra"));
        assert_eq!(
            scope
                .task_policy_defaults
                .waiting
                .as_ref()
                .and_then(|value| value.audience.as_deref()),
            Some("team:infra")
        );
        assert!(scope.body_markdown.contains("# Infrastructure Team"));
    }

    #[test]
    fn parse_availability_rule_manifest() {
        let tmp = TempDir::new().unwrap();
        let content = r#"---
id: "availability:oncall:infra"
subject: "oncall:infra"
enabled: true
timezone: "UTC"
working_hours:
  weekdays: [mon, tue, wed, thu, fri, sat, sun]
  start_local: "00:00"
  end_local: "23:59"
quiet_hours: null
---

# On-call availability
"#;
        write_file(tmp.path(), "oncall-infra.md", content);

        let parser = ManifestParser::new();
        let rule = parser
            .parse_availability_rule(&tmp.path().join("oncall-infra.md"))
            .unwrap();

        assert_eq!(rule.subject, "oncall:infra");
        assert_eq!(rule.timezone.as_deref(), Some("UTC"));
        assert_eq!(
            rule.working_hours
                .as_ref()
                .map(|value| value.start_local.as_str()),
            Some("00:00")
        );
        assert!(rule.body_markdown.contains("# On-call availability"));
    }

    #[test]
    fn parse_project_manifest() {
        let tmp = TempDir::new().unwrap();
        let content = r#"---
id: "project:symbiotic"
slug: symbiotic
title: "Symbiotic"
state: active
owner_hint: "operator:k"
priority: 10
thread_id: "thread-symbiotic"
policy_scopes: ["company:default", "team:core"]
repos: ["symbiotic", "symbiotic-runtime"]
domains: ["product", "runtime"]
---

# Symbiotic

Core product container.
"#;
        write_file(tmp.path(), "project.md", content);

        let parser = ManifestParser::new();
        let project = parser
            .parse_project(&tmp.path().join("project.md"))
            .unwrap();

        assert_eq!(project.id, "project:symbiotic");
        assert_eq!(project.slug, "symbiotic");
        assert_eq!(project.state, crate::types::ProjectState::Active);
        assert_eq!(project.owner_hint.as_deref(), Some("operator:k"));
        assert_eq!(project.policy_scopes, vec!["company:default", "team:core"]);
        assert_eq!(project.repos, vec!["symbiotic", "symbiotic-runtime"]);
        assert!(project.project_markdown.contains("Core product container."));
    }

    #[test]
    fn parse_process_manifest() {
        let tmp = TempDir::new().unwrap();
        let content = r#"---
id: "process:symbiotic:upgrade-loop"
project_id: "project:symbiotic"
slug: upgrade-loop
title: "Upgrade Loop"
state: active
thread_id: "thread-symbiotic-upgrades"
owner_hint: "operator:k"
cadence:
  kind: weekly
  weekday: mon
  local_time: "09:00"
generator:
  mode: recurring_tasks
  target_goal_id: "goal:symbiotic:upgrade-runtime"
task_template:
  task_kind: execution
  task_driver: agent
  title: "Review upgrade backlog"
  policy:
    escalation:
      mode: notify_operator
      audience: "operator"
      severity: normal
      on_enter_blocked: true
      after_secs: 3600
      max_count: 2
      cooldown_secs: 1800
---

# Upgrade Loop

Recurring upgrade review process.
"#;
        write_file(tmp.path(), "upgrade-loop.md", content);

        let parser = ManifestParser::new();
        let process = parser
            .parse_process(&tmp.path().join("upgrade-loop.md"))
            .unwrap();

        assert_eq!(process.id, "process:symbiotic:upgrade-loop");
        assert_eq!(process.project_id, "project:symbiotic");
        assert_eq!(process.slug, "upgrade-loop");
        assert_eq!(process.state, crate::types::ProcessState::Active);
        assert_eq!(process.owner_hint.as_deref(), Some("operator:k"));
        assert_eq!(
            process.cadence.kind,
            crate::types::ProcessCadenceKind::Weekly
        );
        assert_eq!(process.cadence.local_time.as_deref(), Some("09:00"));
        assert_eq!(
            process.generator.mode,
            crate::types::ProcessGeneratorMode::RecurringTasks
        );
        assert_eq!(
            process
                .task_template
                .as_ref()
                .map(|value| value.title.as_str()),
            Some("Review upgrade backlog")
        );
        assert!(process
            .process_markdown
            .contains("Recurring upgrade review process."));
    }

    #[test]
    fn parse_all_from_kb_directory() {
        let tmp = TempDir::new().unwrap();

        // Identity
        write_file(
            tmp.path(),
            "identity/SOUL.md",
            "---\nversion: 1\n---\n\n# SOUL\n",
        );

        // Preferences
        write_file(
            tmp.path(),
            "identity/preferences.md",
            "---\nversion: 1\n---\n\n# Prefs\n",
        );

        // Project
        write_file(
            tmp.path(),
            "operations/projects/test-project/project.md",
            r#"---
id: "project:test-project"
slug: test-project
title: "Test Project"
state: active
---

# Test Project
"#,
        );
        write_file(
            tmp.path(),
            "operations/projects/test-project/processes/upgrade-loop.md",
            r#"---
id: "process:test-project:upgrade-loop"
project_id: "project:test-project"
slug: upgrade-loop
title: "Upgrade Loop"
state: active
cadence:
  kind: weekly
  weekday: mon
  local_time: "09:00"
generator:
  mode: recurring_tasks
  target_goal_id: "test-id"
task_template:
  task_kind: execution
  task_driver: agent
  title: "Review upgrade backlog"
---

# Upgrade Loop
"#,
        );

        // Goal
        write_file(
            tmp.path(),
            "operations/projects/test-project/goals/test-goal/plan.md",
            r#"---
id: "test-id"
project_id: "project:test-project"
slug: test-goal
title: "Test Goal"
state: active
priority: 1
autonomy_level: auto
phase: research
plan_version: 2
process:
  type: on_demand
  check_frequency: weekly
  max_parallel_agents: 1
domains: [engineering]
vault_namespace: goal-test
---

# Test Goal Plan
"#,
        );
        write_file(
            tmp.path(),
            "operations/projects/test-project/goals/test-goal/tasks/research-flights.md",
            r#"---
id: "task-1"
goal_id: "test-id"
task_id: "research-flights"
task_slug: "research-flights"
task_kind: execution
task_driver: agent
title: "Research flights"
state: active
execution_status: planned
role: researcher
depends_on: []
questionnaire_context: ["Budget: under $500"]
owner_hint: "role:researcher"
declared_context:
  review_target: null
  waiting_for: null
  coordination_target: null
  external_dependency: "amadeus-api"
policy:
  escalation:
    mode: notify_operator
    audience: "oncall:infra"
    severity: urgent
    on_enter_blocked: true
    after_secs: null
    max_count: null
    cooldown_secs: null
retry_count: 0
reopen_count: 0
last_status_change_at: 123
plan_version: 2
superseded_by: []
derived_from: []
replaces: []
source_step_id: "step_1"
---

# Research flights

## Summary
- find candidate routes
"#,
        );

        write_file(
            tmp.path(),
            "operations/policy/scopes/team-infra.md",
            r#"---
id: "team:infra"
kind: team
title: "Infrastructure Team"
enabled: true
priority: 200
delivery_subject: "team:infra"
task_policy_defaults:
  waiting:
    mode: notify_operator
    audience: "team:infra"
    severity: urgent
    after_secs: 3600
---

# Infrastructure Team
"#,
        );
        write_file(
            tmp.path(),
            "operations/calendar/availability/oncall-infra.md",
            r#"---
id: "availability:oncall:infra"
subject: "oncall:infra"
enabled: true
timezone: "UTC"
working_hours:
  weekdays: [mon, tue, wed, thu, fri, sat, sun]
  start_local: "00:00"
  end_local: "23:59"
quiet_hours: null
---

# On-call availability
"#,
        );

        // Skill
        write_file(
            tmp.path(),
            "operations/skills/web-scraper/manifest.toml",
            "[skill]\nname = \"web-scraper\"\n",
        );

        let parser = ManifestParser::new();
        let state = parser.parse_all(tmp.path()).unwrap();

        assert!(state.identity.is_some());
        assert!(state.preferences.is_some());
        assert_eq!(state.projects.len(), 1);
        assert_eq!(state.projects[0].slug, "test-project");
        assert_eq!(state.projects[0].processes.len(), 1);
        assert_eq!(state.projects[0].processes[0].slug, "upgrade-loop");
        assert_eq!(state.policy_scopes.len(), 1);
        assert_eq!(state.policy_scopes[0].id, "team:infra");
        assert_eq!(state.availability_rules.len(), 1);
        assert_eq!(state.availability_rules[0].subject, "oncall:infra");
        assert_eq!(state.goals.len(), 1);
        assert_eq!(state.goals[0].slug, "test-goal");
        assert_eq!(state.goals[0].project_id, "project:test-project");
        assert_eq!(state.processes.len(), 1);
        assert_eq!(state.processes[0].project_id, "project:test-project");
        assert_eq!(state.goals[0].tasks.len(), 1);
        assert_eq!(state.goals[0].tasks[0].task_slug, "research-flights");
        assert_eq!(state.skills.len(), 1);
        assert_eq!(state.skills[0].name, "web-scraper");
    }

    #[test]
    fn parse_goal_task_manifest() {
        let tmp = TempDir::new().unwrap();
        let content = r#"---
id: "task-1"
goal_id: "goal-1"
task_id: "search-flights"
task_slug: "search-flights"
task_kind: execution
task_driver: agent
title: "Search flights"
state: active
execution_status: planned
role: researcher
depends_on: []
questionnaire_context: ["Budget: under $500"]
owner_hint: "role:researcher"
declared_context:
  review_target: null
  waiting_for: null
  coordination_target: null
  external_dependency: "amadeus-api"
policy:
  escalation:
    mode: notify_operator
    audience: "oncall:infra"
    severity: urgent
    on_enter_blocked: true
    after_secs: null
    max_count: null
    cooldown_secs: null
  timing:
    timezone: "Europe/Bratislava"
    lateness_basis: delivery_window_elapsed
    delivery_window:
      mode: working_hours
      quiet_hours: null
      working_hours:
        weekdays: [mon, tue, wed, thu, fri]
        start_local: "09:00"
        end_local: "18:00"
retry_count: 1
reopen_count: 0
last_status_change_at: 456
plan_version: 3
superseded_by: []
derived_from: []
replaces: []
thread_id: "thread-travel"
source_step_id: "step_1"
---

# Search flights

## Summary
- look at aggregators
"#;
        write_file(tmp.path(), "task.md", content);

        let parser = ManifestParser::new();
        let task = parser.parse_goal_task(&tmp.path().join("task.md")).unwrap();

        assert_eq!(task.goal_id, "goal-1");
        assert_eq!(task.task_id, "search-flights");
        assert_eq!(task.task_slug, "search-flights");
        assert_eq!(task.task_kind, crate::types::GoalTaskKind::Execution);
        assert_eq!(task.task_driver, crate::types::GoalTaskDriver::Agent);
        assert_eq!(task.title, "Search flights");
        assert_eq!(task.state, crate::types::GoalTaskPlanState::Active);
        assert_eq!(task.execution_status, crate::types::GoalTaskStatus::Planned);
        assert_eq!(task.role.as_deref(), Some("researcher"));
        assert_eq!(
            task.questionnaire_context,
            vec!["Budget: under $500".to_string()]
        );
        assert_eq!(task.owner_hint.as_deref(), Some("role:researcher"));
        assert_eq!(
            task.declared_context.external_dependency.as_deref(),
            Some("amadeus-api")
        );
        assert_eq!(
            task.policy.escalation.as_ref().map(|value| value.mode),
            Some(crate::types::GoalTaskEscalationPolicy::NotifyOperator)
        );
        assert_eq!(
            task.policy
                .escalation
                .as_ref()
                .and_then(|value| value.audience.as_deref()),
            Some("oncall:infra")
        );
        assert_eq!(
            task.policy
                .escalation
                .as_ref()
                .and_then(|value| value.severity),
            Some(crate::types::GoalTaskEscalationSeverity::Urgent)
        );
        assert_eq!(
            task.policy
                .escalation
                .as_ref()
                .map(|value| value.on_enter_blocked),
            Some(true)
        );
        assert_eq!(
            task.policy
                .timing
                .as_ref()
                .and_then(|value| value.timezone.as_deref()),
            Some("Europe/Bratislava")
        );
        assert_eq!(
            task.policy
                .timing
                .as_ref()
                .and_then(|value| value.lateness_basis),
            Some(crate::types::GoalTaskLatenessBasis::DeliveryWindowElapsed)
        );
        assert_eq!(
            task.policy
                .timing
                .as_ref()
                .and_then(|value| value.delivery_window.as_ref())
                .map(|value| value.mode),
            Some(crate::types::GoalTaskDeliveryWindowMode::WorkingHours)
        );
        assert_eq!(task.retry_count, 1);
        assert_eq!(task.last_status_change_at, Some(456));
        assert_eq!(task.plan_version, 3);
        assert_eq!(task.thread_id.as_deref(), Some("thread-travel"));
        assert!(task.task_markdown.contains("# Search flights"));
    }

    #[test]
    fn parse_all_handles_missing_dirs() {
        let tmp = TempDir::new().unwrap();

        let parser = ManifestParser::new();
        let state = parser.parse_all(tmp.path()).unwrap();

        assert!(state.identity.is_none());
        assert!(state.preferences.is_none());
        assert!(state.policy_scopes.is_empty());
        assert!(state.availability_rules.is_empty());
        assert!(state.goals.is_empty());
        assert!(state.skills.is_empty());
    }

    #[test]
    fn parse_goal_event_manifest() {
        let tmp = TempDir::new().unwrap();
        let content = r#"---
goal_id: "goal-1"
event_type: "task_owner_changed"
observed_at: 456
plan_version: 3
thread_id: "thread-travel"
task_id: "search-flights"
previous_status: null
next_status: null
previous_owner: "role:researcher"
next_owner: "@operator:test"
escalation_policy: "notify_operator"
escalation_trigger: "after_secs"
escalation_audience: "oncall:infra"
escalation_severity: "urgent"
escalation_count: 2
cooldown_until: 900
condition_kind: null
condition_value: null
actor: "@lead:test"
note: "Handing off operator ownership"
added_task_ids: []
preserved_task_ids: ["search-flights"]
deactivated_task_ids: []
supersession_edges: []
owner_change_edges: ["search-flights:role:researcher->@operator:test"]
---

# task owner changed

Handing off operator ownership
"#;
        write_file(tmp.path(), "event.md", content);

        let parser = ManifestParser::new();
        let event = parser
            .parse_goal_event(&tmp.path().join("event.md"))
            .unwrap();

        assert_eq!(event.goal_id, "goal-1");
        assert_eq!(event.event_type, "task_owner_changed");
        assert_eq!(event.observed_at, 456);
        assert_eq!(event.plan_version, 3);
        assert_eq!(event.thread_id.as_deref(), Some("thread-travel"));
        assert_eq!(event.task_id.as_deref(), Some("search-flights"));
        assert_eq!(event.previous_owner.as_deref(), Some("role:researcher"));
        assert_eq!(event.next_owner.as_deref(), Some("@operator:test"));
        assert_eq!(event.escalation_policy.as_deref(), Some("notify_operator"));
        assert_eq!(event.escalation_trigger.as_deref(), Some("after_secs"));
        assert_eq!(event.escalation_audience.as_deref(), Some("oncall:infra"));
        assert_eq!(event.escalation_severity.as_deref(), Some("urgent"));
        assert_eq!(event.escalation_count, Some(2));
        assert_eq!(event.cooldown_until, Some(900));
        assert_eq!(event.actor.as_deref(), Some("@lead:test"));
        assert_eq!(
            event.owner_change_edges,
            vec!["search-flights:role:researcher->@operator:test".to_string()]
        );
        assert!(event
            .event_markdown
            .contains("Handing off operator ownership"));
    }

    #[test]
    fn parse_goal_rejects_missing_frontmatter() {
        let tmp = TempDir::new().unwrap();
        write_file(tmp.path(), "plan.md", "# No frontmatter here\n");

        let parser = ManifestParser::new();
        let result = parser.parse_goal(&tmp.path().join("plan.md"));

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("frontmatter"));
    }

    #[test]
    fn parse_goal_rejects_invalid_yaml() {
        let tmp = TempDir::new().unwrap();
        write_file(tmp.path(), "plan.md", "---\n[invalid yaml\n---\n\n# Plan\n");

        let parser = ManifestParser::new();
        let result = parser.parse_goal(&tmp.path().join("plan.md"));

        assert!(result.is_err());
    }

    #[test]
    fn identity_hash_changes_with_content() {
        let tmp = TempDir::new().unwrap();

        write_file(tmp.path(), "v1.md", "---\nversion: 1\n---\n\n# Version 1\n");
        write_file(tmp.path(), "v2.md", "---\nversion: 1\n---\n\n# Version 2\n");

        let parser = ManifestParser::new();
        let id1 = parser.parse_identity(&tmp.path().join("v1.md")).unwrap();
        let id2 = parser.parse_identity(&tmp.path().join("v2.md")).unwrap();

        assert_ne!(id1.content_hash, id2.content_hash);
    }
}
