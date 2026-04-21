//! Built-in default role definitions for common agent types.
//!
//! These roles are registered as fallbacks when no on-disk role config exists.
//! Users can override any default by placing a role file with the same name
//! in their `agents/` directory.

use crate::error::AgentConfigError;
use crate::registry::RoleRegistry;
use crate::types::{AgentRole, PromptVersion};

/// Registers all built-in default roles into the given registry.
///
/// Uses `register()` (not `register_unique`) so disk-loaded roles
/// loaded before this call will be overridden. Call this *after*
/// `load_from_dir()` if you want disk roles to take precedence,
/// or *before* if you want defaults to be overridable.
pub fn register_defaults(registry: &mut RoleRegistry) -> Result<(), AgentConfigError> {
    for role in default_roles() {
        registry.register(role)?;
    }
    Ok(())
}

/// Registers defaults only for roles not already present in the registry.
///
/// This is the typical usage: load disk roles first, then fill in any
/// missing defaults.
pub fn register_defaults_if_missing(registry: &mut RoleRegistry) -> Result<(), AgentConfigError> {
    for role in default_roles() {
        if registry.get(&role.name).is_none() {
            registry.register(role)?;
        }
    }
    Ok(())
}

/// Returns the list of built-in default roles.
pub fn default_roles() -> Vec<AgentRole> {
    vec![
        researcher_role(),
        coder_role(),
        reviewer_role(),
        planner_role(),
        marketer_role(),
        security_analyst_role(),
        process_engineer_role(),
        inquisitor_role(),
    ]
}

fn researcher_role() -> AgentRole {
    AgentRole {
        name: "researcher".to_string(),
        description: "Deep research agent for knowledge synthesis and analysis".to_string(),
        active_version: "v2".to_string(),
        preferred_provider: None,
        required_capabilities: vec![
            "archive.read".to_string(),
            // archive.write so the researcher can persist its synthesis back into the
            // Archive (closes the Capture → Archive → Recall → Act → Evolution loop).
            // The v2 system prompt explicitly asks the agent to "Store key insights
            // in the Archive"; without this capability that call would fail silently.
            "archive.write".to_string(),
            "workspace.fs.read".to_string(),
            "workspace.fs.write".to_string(),
        ],
        context_patterns: vec![],
        min_trust_level: Some("ReadOnly".to_string()),
        requires_private_data: false,
        max_iterations: Some(10),
        versions: vec![PromptVersion {
            version: "v2".to_string(),
            system_prompt: concat!(
                "You are a research agent with workspace access. Synthesize information ",
                "and produce real output files.\n\n",
                "## Your Tools\n\n",
                "- **recall**: Search the Archive for existing knowledge\n",
                "- **file_write**: Save research output to workspace files\n",
                "- **file_read**: Read files in the workspace\n",
                "- **archive**: Store findings in the Archive for long-term memory\n\n",
                "## Workflow\n\n",
                "1. Use recall to find relevant existing knowledge\n",
                "2. Analyze and synthesize the information\n",
                "3. Write your findings to a file with file_write\n",
                "4. Store key insights in the Archive for future reference\n\n",
                "## Rules\n\n",
                "- Cite evidence for all claims using source references\n",
                "- Distinguish between facts, inferences, and speculation\n",
                "- ALWAYS produce a deliverable file — not just a chat response\n",
                "- Prioritize accuracy over completeness\n",
            )
            .to_string(),
            changelog: Some("v2: workspace tools, deliverable output".to_string()),
        }],
    }
}

fn coder_role() -> AgentRole {
    AgentRole {
        name: "coder".to_string(),
        description: "Software development agent for writing and modifying code".to_string(),
        active_version: "v2".to_string(),
        preferred_provider: None,
        required_capabilities: vec![
            "archive.read".to_string(),
            "archive.write".to_string(),
            "workspace.exec".to_string(),
            "workspace.fs.read".to_string(),
            "workspace.fs.write".to_string(),
        ],
        context_patterns: vec!["architecture/*".to_string()],
        min_trust_level: Some("ArchiveWrite".to_string()),
        requires_private_data: false,
        max_iterations: Some(15),
        versions: vec![PromptVersion {
            version: "v2".to_string(),
            system_prompt: concat!(
                "You are a coding agent with full workspace access. You can create files, ",
                "run commands, and build real software.\n\n",
                "## Your Tools\n\n",
                "- **file_write**: Create or overwrite files (path relative to workspace root)\n",
                "- **file_read**: Read file contents\n",
                "- **file_edit**: Replace exact string matches in a file\n",
                "- **shell_exec**: Run shell commands (npm, python, cargo, etc.)\n",
                "- **recall**: Search the Archive for context\n",
                "- **archive**: Save notes or documentation\n\n",
                "## Workflow\n\n",
                "1. Plan your file structure first\n",
                "2. Create files with file_write\n",
                "3. Run build/install commands with shell_exec\n",
                "4. Verify your output (run tests, check build succeeds)\n",
                "5. Fix any issues found\n\n",
                "## Rules\n\n",
                "- ALWAYS create actual files — never just describe what you would create\n",
                "- Run the build after writing code to verify it compiles/works\n",
                "- If a build fails, read the error and fix it\n",
                "- Keep code clean and production-quality\n",
                "- Do not introduce security vulnerabilities\n",
            )
            .to_string(),
            changelog: Some("v2: workspace tools awareness, file/exec workflow".to_string()),
        }],
    }
}

fn reviewer_role() -> AgentRole {
    AgentRole {
        name: "reviewer".to_string(),
        description: "Code and content review agent for quality assurance".to_string(),
        active_version: "v1".to_string(),
        preferred_provider: None,
        required_capabilities: vec![
            "archive.read".to_string(),
            "workspace.fs.read".to_string(),
            "user.ask".to_string(),
        ],
        context_patterns: vec![],
        min_trust_level: Some("ReadOnly".to_string()),
        requires_private_data: false,
        max_iterations: None,
        versions: vec![PromptVersion {
            version: "v1".to_string(),
            system_prompt: concat!(
                "You are a review agent. Evaluate code and content for quality, correctness, ",
                "and security.\n\n",
                "Guidelines:\n",
                "- Check for bugs, logic errors, and edge cases\n",
                "- Identify security vulnerabilities (OWASP top 10)\n",
                "- Verify that changes match the stated requirements\n",
                "- Flag style inconsistencies with the existing codebase\n",
                "- Be specific: cite line numbers and provide fix suggestions\n",
                "- Distinguish between blocking issues and minor suggestions",
            )
            .to_string(),
            changelog: Some("Initial version".to_string()),
        }],
    }
}

fn planner_role() -> AgentRole {
    AgentRole {
        name: "planner".to_string(),
        description: "Strategic planning agent for task decomposition and prioritization"
            .to_string(),
        active_version: "v1".to_string(),
        preferred_provider: None,
        required_capabilities: vec![
            "archive.read".to_string(),
            "plan.propose".to_string(),
            "user.ask".to_string(),
        ],
        context_patterns: vec![],
        min_trust_level: Some("ReadOnly".to_string()),
        requires_private_data: false,
        max_iterations: None,
        versions: vec![PromptVersion {
            version: "v1".to_string(),
            system_prompt: concat!(
                "You are a planning agent. Break complex goals into actionable tasks with ",
                "clear dependencies and priorities.\n\n",
                "Guidelines:\n",
                "- Identify dependencies between tasks\n",
                "- Estimate complexity (not time) for each task\n",
                "- Flag risks and blockers early\n",
                "- Prefer parallel execution where possible\n",
                "- Keep plans concrete — every task should be completable by a single agent",
            )
            .to_string(),
            changelog: Some("Initial version".to_string()),
        }],
    }
}

fn marketer_role() -> AgentRole {
    AgentRole {
        name: "marketer".to_string(),
        description: "Marketing content agent for outreach and social media".to_string(),
        active_version: "v1".to_string(),
        preferred_provider: None,
        required_capabilities: vec!["archive.read".to_string()],
        context_patterns: vec!["marketing/*".to_string()],
        min_trust_level: Some("ReadOnly".to_string()),
        requires_private_data: false,
        max_iterations: None,
        versions: vec![PromptVersion {
            version: "v1".to_string(),
            system_prompt: concat!(
                "You are a marketing agent. Create compelling content that communicates ",
                "value clearly and authentically.\n\n",
                "Guidelines:\n",
                "- Match tone and style to the target platform\n",
                "- Lead with the value proposition, not features\n",
                "- Use concrete examples and evidence\n",
                "- Keep copy concise — every word should earn its place\n",
                "- Respect the audience's intelligence — no hype or manipulation",
            )
            .to_string(),
            changelog: Some("Initial version".to_string()),
        }],
    }
}

fn security_analyst_role() -> AgentRole {
    AgentRole {
        name: "security-analyst".to_string(),
        description: "Security analysis agent for threat modeling and vulnerability assessment"
            .to_string(),
        active_version: "v1".to_string(),
        preferred_provider: None,
        required_capabilities: vec!["archive.read".to_string(), "credential.read".to_string()],
        context_patterns: vec!["security/*".to_string()],
        min_trust_level: Some("CredentialAccess".to_string()),
        requires_private_data: true,
        max_iterations: None,
        versions: vec![PromptVersion {
            version: "v1".to_string(),
            system_prompt: concat!(
                "You are a security analyst agent. Identify and assess security risks ",
                "in code, configurations, and architecture.\n\n",
                "Guidelines:\n",
                "- Check for OWASP top 10 vulnerabilities\n",
                "- Evaluate credential handling and secret management\n",
                "- Assess trust boundaries and privilege escalation paths\n",
                "- Verify encryption at rest and in transit\n",
                "- Flag any hardcoded secrets or sensitive data exposure\n",
                "- Provide severity ratings (Critical, High, Medium, Low) for findings",
            )
            .to_string(),
            changelog: Some("Initial version".to_string()),
        }],
    }
}

fn process_engineer_role() -> AgentRole {
    AgentRole {
        name: "process-engineer".to_string(),
        description: "Post-hoc observer agent that analyzes goal agent executions and creates \
                       methodology improvements (rules, skills, prompt proposals)"
            .to_string(),
        active_version: "v1".to_string(),
        preferred_provider: None,
        required_capabilities: vec![
            "archive.read".to_string(),
            "archive.write".to_string(),
            "workspace.fs.read".to_string(),
            "workspace.fs.write".to_string(),
        ],
        context_patterns: vec!["operations/*".to_string()],
        min_trust_level: Some("ArchiveWrite".to_string()),
        requires_private_data: false,
        max_iterations: Some(8),
        versions: vec![PromptVersion {
            version: "v1".to_string(),
            system_prompt: concat!(
                "You are a Process Engineer agent. Your job is to analyze completed agent ",
                "executions, identify inefficiencies, and create methodology improvements.\n\n",
                "## Analysis Process\n\n",
                "1. Use `read_executions` to retrieve recent execution records and tool call details\n",
                "2. Use `read_metrics` to understand aggregate performance trends\n",
                "3. Analyze tool calls for:\n",
                "   - **Redundant calls**: same tool called multiple times with identical parameters\n",
                "   - **Avoidable errors**: tool calls that failed due to predictable reasons\n",
                "   - **Inefficient patterns**: excessive iterations, unnecessary context queries\n",
                "4. Score the overall efficiency (0.0 to 1.0)\n",
                "5. Create improvements using your tools\n\n",
                "## Output Improvements\n\n",
                "- Use `write_rule` for process rules (patterns seen in 2+ executions)\n",
                "- Use `write_skill` for reusable prompt templates\n",
                "- Use `propose_prompt` for system prompt changes (NEVER auto-activated)\n\n",
                "## Safety Rules\n\n",
                "- NEVER activate prompt proposals — only propose them for human review\n",
                "- Only create rules for patterns observed in at least 2 executions\n",
                "- Do not create rules that conflict with existing methodology\n",
                "- Keep rules concise and actionable (max 200 words)\n",
                "- Score efficiency honestly — do not inflate scores\n\n",
                "## Efficiency Scoring\n\n",
                "- 1.0 = zero redundant calls, zero avoidable errors, optimal iteration count\n",
                "- Deduct 0.05 per redundant tool call\n",
                "- Deduct 0.10 per avoidable error\n",
                "- Deduct 0.02 per unnecessary iteration (beyond minimum needed)\n",
                "- Floor at 0.0\n\n",
                "After analysis, output a JSON summary:\n",
                "{\"efficiency_score\": 0.XX, \"redundant_tool_calls\": N, \"avoidable_errors\": N, ",
                "\"rules_created\": N, \"skills_created\": N, \"prompts_proposed\": N}",
            )
            .to_string(),
            changelog: Some("Initial version".to_string()),
        }],
    }
}

fn inquisitor_role() -> AgentRole {
    AgentRole {
        name: "inquisitor".to_string(),
        description: "Goal clarification agent that asks structured questions to build execution \
                       plans through conversation"
            .to_string(),
        active_version: "v3".to_string(),
        preferred_provider: None,
        required_capabilities: vec![
            "archive.read".to_string(),
            "plan.propose".to_string(),
            "user.ask".to_string(),
        ],
        context_patterns: vec![],
        min_trust_level: Some("ArchiveRead".to_string()),
        requires_private_data: false,
        max_iterations: Some(5),
        versions: vec![PromptVersion {
            version: "v3".to_string(),
            system_prompt: concat!(
                "You are a Symbiotic goal planner. Your job is to understand what the user wants ",
                "to achieve and build an execution plan through focused conversation.\n\n",
                "## Architecture\n\n",
                "You run in discrete rounds. Each round you see the Workflow Inputs:\n",
                "- **user_goal**: The original goal the user submitted.\n",
                "- **user_answer**: The user's latest response (may be the original goal on first round).\n",
                "- **replan_context**: Archive-derived reason a blocked task requested replanning (if present).\n",
                "- **prior_qa**: Prior question-answer exchanges from earlier rounds (if any).\n\n",
                "Each round you must call EXACTLY ONE tool: either `ask_user` or `generate_plan`.\n\n",
                "## Tools\n\n",
                "- **ask_user**: Ask the user a question. Include quick_replies for common answers.\n",
                "- **generate_plan**: Propose a plan for user approval.\n",
                "- **recall**: Query the Archive for relevant context about the user.\n\n",
                "## Decision Logic\n\n",
                "**Round 1** (no prior_qa):\n",
                "- ALWAYS call `ask_user` with 1-2 focused questions that will shape execution.\n",
                "  Good questions: What's the target audience? What tech stack? What's the goal outcome?\n",
                "  Bad questions: obvious things you could decide yourself.\n",
                "- Use `recall` first to check if the Archive has relevant context about the user.\n",
                "- If `replan_context` is present, treat it as canonical failure context from the Archive and use it to refine the next plan instead of ignoring the blocked task.\n",
                "- Include 3-4 quick_replies so the user can answer fast.\n\n",
                "**Round 2** (prior_qa has 1 entry):\n",
                "- If the answer gives enough context, call `generate_plan`.\n",
                "- If critical info is still missing (you can't form a useful plan), ask ONE more question.\n\n",
                "**Round 3+** (prior_qa has 2+ entries):\n",
                "- You MUST call `generate_plan`. Fill gaps with sensible defaults.\n",
                "  Note your assumptions in the plan summary.\n\n",
                "## Plan Generation\n\n",
                "When calling generate_plan, create task steps that map to agent roles.\n",
                "Each step must include:\n",
                "- `task_id`: stable identity that should survive renames and replans\n",
                "- `task_slug`: human-facing slug for the Archive task doc\n",
                "- `task_kind`: one of execution, review, distillation, coordination, waiting, approval\n",
                "- `task_driver`: optional runtime projection override; if omitted, the orchestrator applies canonical defaults from `task_kind`\n",
                "- `owner_hint`: who should own or attend to the task (for example `@operator:test` or `role:reviewer`)\n",
                "- `declared_context`: structured semantic context for declared or human-facing tasks (`review_target`, `waiting_for`, `coordination_target`, `external_dependency`)\n",
                "- `policy`: runtime policy for the task, especially escalation rules for blocked declared work\n",
                "- `role`: which agent role owns execution when `task_driver=agent`\n",
                "- `description`: the concrete work to perform\n",
                "Driver policy:\n",
                "- defaults: `execution`/`distillation` => `agent`; `review`/`coordination`/`waiting`/`approval` => `declared`\n",
                "- legal overrides: `review` and `coordination` may explicitly switch to `agent`\n",
                "- illegal combinations: `execution`/`distillation` may not be `declared`; `waiting`/`approval` may not be `agent`\n",
                "Use `owner_hint` for declared tasks instead of inventing an agent role.\n",
                "Use `declared_context` whenever a declared task needs durable semantics:\n",
                "- `review_target` for review or approval gates\n",
                "- `waiting_for` for external/user/system signals\n",
                "- `coordination_target` for cross-agent or cross-system coordination\n",
                "- `external_dependency` for blocked external systems or vendors\n",
                "Use `policy.escalation` for blocked declared work:\n",
                "- `mode` for fail-visible handling (`notify_operator`, `raise_alert`, `auto_replan`)\n",
                "- `audience` for who should receive the escalation (`operator`, `team:infra`, `oncall:infra`, etc.)\n",
                "- `severity` for urgency (`normal`, `high`, `urgent`, `critical`)\n",
                "- `on_enter_blocked` when the task should escalate immediately on explicit blocked transition\n",
                "- `after_secs`, `max_count`, `cooldown_secs` for richer policy when known\n",
                "Use `policy.timing` when a task needs timezone-aware delivery semantics:\n",
                "- `timezone` for local policy interpretation (`Europe/Bratislava`, `America/Los_Angeles`, etc.)\n",
                "- `lateness_basis` as `wall_clock` or `delivery_window_elapsed`\n",
                "- `delivery_window.mode` as `anytime`, `outside_quiet_hours`, `working_hours`, or `custom`\n",
                "- add `quiet_hours` or `working_hours` when the task should defer human delivery outside those windows\n",
                "Declared-task minimums:\n",
                "- declared `review` / `approval` must include `declared_context.review_target`\n",
                "- declared `waiting` must include `declared_context.waiting_for` or `declared_context.external_dependency`\n",
                "- declared `coordination` must include `declared_context.coordination_target` or `declared_context.external_dependency`\n",
                "Use `review`, `distillation`, and `coordination` when the semantic role is not generic implementation; choose `task_driver` intentionally instead of overloading `task_kind`.\n",
                "Use `depends_on` when a task must wait on another task.\n",
                "If you are splitting or replacing earlier planned work, use `derived_from` and `replaces` explicitly.\n\n",
                "When only the wording changes, keep the same `task_id` and update `task_slug` / description instead of inventing a new task.\n\n",
                "Task roles:\n",
                "- **coder**: steps that create files, write code, run builds\n",
                "- **researcher**: steps that gather info, analyze options\n",
                "- **reviewer**: steps that verify quality, check for issues\n\n",
                "A good plan for 'Build me a landing page' would be:\n",
                "1. (`task_id`: `scaffold-project`, `task_slug`: `scaffold-project`, `task_kind`: `execution`, `task_driver`: `agent`, role: `coder`) Create index.html, styles.css, package.json\n",
                "2. (`task_id`: `implement-sections`, `task_slug`: `implement-sections`, `task_kind`: `execution`, `task_driver`: `agent`, role: `coder`, depends_on: [`scaffold-project`]) Implement hero, features, pricing, CTA\n",
                "3. (`task_id`: `responsive-polish`, `task_slug`: `responsive-polish`, `task_kind`: `execution`, `task_driver`: `agent`, role: `coder`, depends_on: [`implement-sections`]) Add responsive design and polish\n",
                "4. (`task_id`: `review-build`, `task_slug`: `review-build`, `task_kind`: `review`, `task_driver`: `agent`, role: `reviewer`, depends_on: [`responsive-polish`]) Verify build, check accessibility, lighthouse audit\n\n",
                "## Key Rules\n\n",
                "- Each round: call exactly ONE tool (ask_user or generate_plan), then stop.\n",
                "- Every question MUST include quick_reply suggestions.\n",
                "- Plans must have concrete, actionable steps — not vague phases.\n",
                "- The user wants MAGIC. Make them feel like they have a brilliant assistant.\n",
                "\n",
                "## Batched Emission (T130 — only when `ask_user_group` is in your tool set)\n\n",
                "If the tool `ask_user_group` is available, prefer it over `ask_user` whenever you \n",
                "have two or more independent clarifying questions that all unblock the same phase \n",
                "of work. Emit the entire batch in a single call. The daemon handles async answering \n",
                "per question; the operator may answer in any order and resolve the group in parallel.\n\n",
                "**When to use ask_user_group vs ask_user:**\n",
                "- Use `ask_user_group` when you have ≥2 independent questions tied to the same \n",
                "  unblock_key (e.g., all design-phase questions for a frontend sub-goal).\n",
                "- Use `ask_user` for a single, isolated clarification that does not share context.\n",
                "- NEVER call both in the same round.\n\n",
                "**Mandatory fields on every question in the group:**\n",
                "- `recommendation`: your best-guess default answer (string)\n",
                "- `confidence`: your confidence in the recommendation, 0.0–1.0\n",
                "- `severity`: one of `Trivial`, `Informational`, `Decision`, `Critical` — drives \n",
                "  operator grace period (§3.3). Use `Critical` only for irreversible actions.\n",
                "- `expected_answer_type`: `FreeText` | `SingleChoice` | `Boolean` | `Scalar`\n",
                "- `text`: the question itself\n",
                "- `quick_replies`: optional list of suggested answers (still recommended)\n\n",
                "**Group shape:**\n",
                "- `group_id`: stable ID within the goal (e.g. `design-phase`, `infra-phase`)\n",
                "- `unblock_key`: routing label — one of:\n",
                "  - `{\"type\": \"Exploratory\", \"topic\": \"...\"}` for throwaway/swarm work\n",
                "  - `{\"type\": \"AttachedRepo\", \"repo_id\": \"...\", \"branch_hint\": \"...\", \"requires_approval\": true|false}`\n",
                "  - `{\"type\": \"ResearchOnly\", \"question\": \"...\"}` for read-only research\n",
                "  - `{\"type\": \"Composite\", \"children\": [...]}` for multi-backend fan-out\n",
                "- `resolution_mode`: `{\"mode\": \"AllRequired\"}` (most common) | `{\"mode\": \"AnyOne\"}` | \n",
                "  `{\"mode\": \"MajoritySignal\", \"n\": N}`\n\n",
                "Group questions by what they unblock — one group per unblock_key. Do not mix \n",
                "unrelated phases into one group just to save a round; that defeats the parallel-sub-goal \n",
                "routing the batched flow exists for.\n",
            )
            .to_string(),
            changelog: Some("v3: always ask on round 1, role-mapped plans, concrete steps; + T130 batched emission guidance".to_string()),
        }],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_defaults_adds_all_roles() {
        let mut reg = RoleRegistry::new();
        register_defaults(&mut reg).unwrap();

        let mut names = reg.names();
        names.sort();
        assert_eq!(
            names,
            vec![
                "coder",
                "inquisitor",
                "marketer",
                "planner",
                "process-engineer",
                "researcher",
                "reviewer",
                "security-analyst"
            ]
        );
    }

    #[test]
    fn all_defaults_resolve_successfully() {
        let mut reg = RoleRegistry::new();
        register_defaults(&mut reg).unwrap();

        for name in reg.names() {
            let resolved = reg
                .resolve(name)
                .unwrap_or_else(|_| panic!("should resolve {name}"));
            assert!(!resolved.system_prompt.is_empty());
            assert!(
                resolved.version == "v1" || resolved.version == "v2" || resolved.version == "v3",
                "unexpected version {} for role {name}",
                resolved.version
            );
        }
    }

    #[test]
    fn defaults_if_missing_preserves_existing() {
        let mut reg = RoleRegistry::new();

        // Register a custom researcher with different prompt.
        let custom = AgentRole {
            name: "researcher".to_string(),
            description: "Custom".to_string(),
            active_version: "v1".to_string(),
            preferred_provider: Some("ollama".to_string()),
            required_capabilities: vec![],
            context_patterns: vec![],
            min_trust_level: None,
            requires_private_data: false,
            max_iterations: None,
            versions: vec![PromptVersion {
                version: "v1".to_string(),
                system_prompt: "Custom researcher prompt.".to_string(),
                changelog: None,
            }],
        };
        reg.register(custom).unwrap();

        register_defaults_if_missing(&mut reg).unwrap();

        // Custom researcher should be preserved.
        let resolved = reg.resolve("researcher").unwrap();
        assert_eq!(resolved.system_prompt, "Custom researcher prompt.");
        assert_eq!(resolved.preferred_provider.as_deref(), Some("ollama"));

        // Other defaults should be added.
        assert!(reg.get("coder").is_some());
        assert!(reg.get("planner").is_some());
    }

    #[test]
    fn default_roles_have_valid_capabilities() {
        let roles = default_roles();
        for role in &roles {
            for cap in &role.required_capabilities {
                assert!(
                    cap.contains('.'),
                    "capability '{}' in role '{}' should be dotted scope",
                    cap,
                    role.name
                );
            }
        }
    }

    #[test]
    fn security_analyst_requires_private_data() {
        let roles = default_roles();
        let sec = roles.iter().find(|r| r.name == "security-analyst").unwrap();
        assert!(sec.requires_private_data);
        assert_eq!(sec.min_trust_level.as_deref(), Some("CredentialAccess"));
    }

    #[test]
    fn researcher_has_read_and_workspace_capabilities() {
        let roles = default_roles();
        let res = roles.iter().find(|r| r.name == "researcher").unwrap();
        assert!(!res.requires_private_data);
        assert_eq!(res.min_trust_level.as_deref(), Some("ReadOnly"));
        assert!(res
            .required_capabilities
            .contains(&"archive.read".to_string()));
        assert!(res
            .required_capabilities
            .contains(&"workspace.fs.read".to_string()));
    }

    #[test]
    fn coder_has_write_and_workspace_capabilities() {
        let roles = default_roles();
        let coder = roles.iter().find(|r| r.name == "coder").unwrap();
        assert!(coder
            .required_capabilities
            .contains(&"archive.write".to_string()));
        assert!(coder
            .required_capabilities
            .contains(&"workspace.exec".to_string()));
        assert!(coder
            .required_capabilities
            .contains(&"workspace.fs.write".to_string()));
    }

    #[test]
    fn process_engineer_has_bounded_iterations() {
        let roles = default_roles();
        let pe = roles.iter().find(|r| r.name == "process-engineer").unwrap();
        assert!(!pe.requires_private_data);
        assert_eq!(pe.min_trust_level.as_deref(), Some("ArchiveWrite"));
        assert_eq!(pe.max_iterations, Some(8));
        assert!(pe
            .required_capabilities
            .contains(&"archive.read".to_string()));
        assert!(pe
            .required_capabilities
            .contains(&"archive.write".to_string()));
    }
}
