use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::Path;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Workflow {
    pub id: String,
    pub name: String,
    pub version: String,
    pub inputs: HashMap<String, String>,
    pub policy: WorkflowPolicy,
    pub steps: Vec<WorkflowStep>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowPolicy {
    pub sensitivity_max: String,
    pub model_class: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowStep {
    pub id: String,
    pub step_type: String,
    pub config: HashMap<String, String>,
    /// Optional agent role name (e.g., "researcher", "coder"). When set,
    /// the executor resolves the role from the `RoleRegistry` to customize
    /// the system prompt and execution parameters for this step.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_role: Option<String>,
}

#[derive(Debug, Clone)]
pub struct WorkflowContext {
    pub run_id: String,
    pub workflow_id: String,
    pub inputs: HashMap<String, String>,
    pub outputs: HashMap<String, String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepStatus {
    Success,
    Failed,
}

#[derive(Debug, Clone)]
pub struct StepResult {
    pub step_id: String,
    pub status: StepStatus,
    pub outputs: HashMap<String, String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkflowStatus {
    Success,
    Failed,
}

#[derive(Debug, Clone)]
pub struct WorkflowRunResult {
    pub run_id: String,
    pub workflow_id: String,
    pub status: WorkflowStatus,
    pub step_results: Vec<StepResult>,
}

pub trait StepExecutor: Send + Sync {
    fn execute(&self, step: &WorkflowStep, ctx: &WorkflowContext) -> Result<StepResult>;
}

pub struct WorkflowRunner {
    executors: HashMap<String, Arc<dyn StepExecutor>>,
}

#[derive(Default)]
pub struct WorkflowRegistry {
    templates: HashMap<String, Workflow>,
}

impl WorkflowRegistry {
    pub fn new() -> Self {
        Self {
            templates: HashMap::new(),
        }
    }

    pub fn register(&mut self, workflow: Workflow) -> Result<()> {
        validate_workflow(&workflow)?;
        self.templates.insert(workflow.name.clone(), workflow);
        Ok(())
    }

    pub fn get(&self, name: &str) -> Option<Workflow> {
        self.templates.get(name).cloned()
    }

    pub fn load_from_dir(dir: impl AsRef<Path>) -> Result<Self> {
        Self::load_from_dirs([dir])
    }

    pub fn load_from_dirs<I, P>(dirs: I) -> Result<Self>
    where
        I: IntoIterator<Item = P>,
        P: AsRef<Path>,
    {
        let mut registry = Self::new();
        for dir in dirs {
            registry.load_into(dir.as_ref())?;
        }
        Ok(registry)
    }

    fn load_into(&mut self, dir: &Path) -> Result<()> {
        if !dir.exists() {
            return Ok(());
        }

        let mut files = fs::read_dir(dir)?
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.extension()
                    .and_then(|ext| ext.to_str())
                    .map(|ext| ext.eq_ignore_ascii_case("json"))
                    .unwrap_or(false)
            })
            .collect::<Vec<_>>();
        files.sort();

        for path in files {
            let payload = fs::read_to_string(&path)?;
            let raw: serde_json::Value = serde_json::from_str(&payload)
                .map_err(|err| anyhow!("invalid workflow JSON {}: {err}", path.display()))?;
            validate_workflow_schema_value(&raw)
                .map_err(|err| anyhow!("invalid workflow schema {}: {err}", path.display()))?;
            let workflow: Workflow = serde_json::from_value(raw)
                .map_err(|err| anyhow!("invalid workflow JSON {}: {err}", path.display()))?;
            self.register(workflow)?;
        }
        Ok(())
    }

    pub fn with_mvp_templates() -> Self {
        // Load shipped templates first, then apply user overrides.
        // User definitions intentionally replace shipped names.
        if let Ok(registry) =
            Self::load_from_dirs(["crates/symbiotic-workflows/templates", "data/workflows"])
        {
            if !registry.templates.is_empty() {
                return registry;
            }
        }
        let mut registry = Self::new();
        for workflow in builtin_mvp_templates() {
            let _ = registry.register(workflow);
        }
        registry
    }
}

impl WorkflowRunner {
    pub fn new() -> Self {
        Self {
            executors: HashMap::new(),
        }
    }

    pub fn with_executor(mut self, step_type: &str, executor: Arc<dyn StepExecutor>) -> Self {
        self.executors.insert(step_type.to_string(), executor);
        self
    }

    /// Run the workflow, invoking `on_step` before and after each step.
    ///
    /// The callback receives:
    /// - `step_index`: 0-based index of the step
    /// - `step`: reference to the `WorkflowStep` being executed
    /// - `total_steps`: total number of steps in the workflow
    /// - `result`: `None` when the step is about to start, `Some(&StepResult)` when it finished
    pub fn run_with_progress<F>(
        &self,
        workflow: &Workflow,
        mut on_step: F,
    ) -> Result<WorkflowRunResult>
    where
        F: FnMut(usize, &WorkflowStep, usize, Option<&StepResult>),
    {
        validate_workflow(workflow)?;

        let run_id = generate_run_id(workflow);
        let mut ctx = WorkflowContext {
            run_id: run_id.clone(),
            workflow_id: workflow.id.clone(),
            inputs: workflow.inputs.clone(),
            outputs: HashMap::new(),
        };
        let total_steps = workflow.steps.len();

        let mut step_results = Vec::new();
        for (step_index, step) in workflow.steps.iter().enumerate() {
            let Some(executor) = self.executors.get(&step.step_type) else {
                let result = StepResult {
                    step_id: step.id.clone(),
                    status: StepStatus::Failed,
                    outputs: HashMap::new(),
                    error: Some(format!("missing executor for step type {}", step.step_type)),
                };
                on_step(step_index, step, total_steps, Some(&result));
                step_results.push(result);
                return Ok(WorkflowRunResult {
                    run_id,
                    workflow_id: workflow.id.clone(),
                    status: WorkflowStatus::Failed,
                    step_results,
                });
            };

            on_step(step_index, step, total_steps, None);

            let result = match executor.execute(step, &ctx) {
                Ok(result) => result,
                Err(err) => StepResult {
                    step_id: step.id.clone(),
                    status: StepStatus::Failed,
                    outputs: HashMap::new(),
                    error: Some(err.to_string()),
                },
            };

            on_step(step_index, step, total_steps, Some(&result));

            if result.status == StepStatus::Success {
                for (key, value) in &result.outputs {
                    ctx.outputs.insert(key.clone(), value.clone());
                }
                step_results.push(result);
                continue;
            }

            step_results.push(result);
            return Ok(WorkflowRunResult {
                run_id,
                workflow_id: workflow.id.clone(),
                status: WorkflowStatus::Failed,
                step_results,
            });
        }

        Ok(WorkflowRunResult {
            run_id,
            workflow_id: workflow.id.clone(),
            status: WorkflowStatus::Success,
            step_results,
        })
    }

    pub fn run(&self, workflow: &Workflow) -> Result<WorkflowRunResult> {
        validate_workflow(workflow)?;

        let run_id = generate_run_id(workflow);
        let mut ctx = WorkflowContext {
            run_id: run_id.clone(),
            workflow_id: workflow.id.clone(),
            inputs: workflow.inputs.clone(),
            outputs: HashMap::new(),
        };

        let mut step_results = Vec::new();
        for step in &workflow.steps {
            let Some(executor) = self.executors.get(&step.step_type) else {
                step_results.push(StepResult {
                    step_id: step.id.clone(),
                    status: StepStatus::Failed,
                    outputs: HashMap::new(),
                    error: Some(format!("missing executor for step type {}", step.step_type)),
                });
                return Ok(WorkflowRunResult {
                    run_id,
                    workflow_id: workflow.id.clone(),
                    status: WorkflowStatus::Failed,
                    step_results,
                });
            };

            let result = match executor.execute(step, &ctx) {
                Ok(result) => result,
                Err(err) => StepResult {
                    step_id: step.id.clone(),
                    status: StepStatus::Failed,
                    outputs: HashMap::new(),
                    error: Some(err.to_string()),
                },
            };

            if result.status == StepStatus::Success {
                for (key, value) in &result.outputs {
                    ctx.outputs.insert(key.clone(), value.clone());
                }
                step_results.push(result);
                continue;
            }

            step_results.push(result);
            return Ok(WorkflowRunResult {
                run_id,
                workflow_id: workflow.id.clone(),
                status: WorkflowStatus::Failed,
                step_results,
            });
        }

        Ok(WorkflowRunResult {
            run_id,
            workflow_id: workflow.id.clone(),
            status: WorkflowStatus::Success,
            step_results,
        })
    }
}

impl Default for WorkflowRunner {
    fn default() -> Self {
        Self::new()
    }
}

pub struct NoopExecutor;

impl StepExecutor for NoopExecutor {
    fn execute(&self, step: &WorkflowStep, _ctx: &WorkflowContext) -> Result<StepResult> {
        Ok(StepResult {
            step_id: step.id.clone(),
            status: StepStatus::Success,
            outputs: HashMap::new(),
            error: None,
        })
    }
}

fn builtin_mvp_templates() -> Vec<Workflow> {
    vec![Workflow {
        id: "wf-intake".to_string(),
        name: "intake-url".to_string(),
        version: "1.0".to_string(),
        inputs: HashMap::new(),
        policy: WorkflowPolicy {
            sensitivity_max: "shareable".to_string(),
            model_class: "local".to_string(),
        },
        steps: vec![
            WorkflowStep {
                id: "normalize".to_string(),
                step_type: "intake.normalize".to_string(),
                config: HashMap::new(),
                agent_role: None,
            },
            WorkflowStep {
                id: "dedupe".to_string(),
                step_type: "intake.dedupe".to_string(),
                config: HashMap::new(),
                agent_role: None,
            },
            WorkflowStep {
                id: "fetch".to_string(),
                step_type: "ingest.fetch".to_string(),
                config: HashMap::new(),
                agent_role: None,
            },
            WorkflowStep {
                id: "store".to_string(),
                step_type: "archive.store".to_string(),
                config: HashMap::new(),
                agent_role: None,
            },
            WorkflowStep {
                id: "review".to_string(),
                step_type: "archive.review.enqueue".to_string(),
                config: HashMap::new(),
                agent_role: None,
            },
        ],
    }]
}

fn generate_run_id(workflow: &Workflow) -> String {
    let mut hasher = DefaultHasher::new();
    workflow.id.hash(&mut hasher);
    workflow.version.hash(&mut hasher);
    workflow.steps.len().hash(&mut hasher);
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock must be after epoch")
        .as_secs();
    timestamp.hash(&mut hasher);
    format!("wf_{:x}", hasher.finish())
}

pub fn validate_workflow(workflow: &Workflow) -> Result<()> {
    if workflow.id.trim().is_empty() {
        return Err(anyhow!("workflow id cannot be empty"));
    }
    if workflow.name.trim().is_empty() {
        return Err(anyhow!("workflow name cannot be empty"));
    }
    if workflow.version.trim().is_empty() {
        return Err(anyhow!("workflow version cannot be empty"));
    }
    if workflow.steps.is_empty() {
        return Err(anyhow!("workflow {} has no steps", workflow.id));
    }
    if !matches!(
        workflow.policy.sensitivity_max.as_str(),
        "shareable" | "restricted" | "private"
    ) {
        return Err(anyhow!("invalid sensitivity_max policy"));
    }
    if !matches!(
        workflow.policy.model_class.as_str(),
        "local" | "hybrid" | "cloud"
    ) {
        return Err(anyhow!("invalid model_class policy"));
    }

    let mut step_ids = HashMap::new();
    for step in &workflow.steps {
        if step.id.trim().is_empty() {
            return Err(anyhow!("workflow contains step with empty id"));
        }
        if step.step_type.trim().is_empty() {
            return Err(anyhow!("workflow step {} has empty step_type", step.id));
        }
        if step_ids.insert(step.id.clone(), true).is_some() {
            return Err(anyhow!("workflow has duplicate step id {}", step.id));
        }
    }
    Ok(())
}

fn validate_workflow_schema_value(raw: &serde_json::Value) -> Result<()> {
    let Some(root) = raw.as_object() else {
        return Err(anyhow!("root must be an object"));
    };
    ensure_only_allowed_fields(
        root,
        &["id", "name", "version", "inputs", "policy", "steps"],
        "workflow",
    )?;

    let Some(id) = root.get("id").and_then(|value| value.as_str()) else {
        return Err(anyhow!("workflow.id must be a string"));
    };
    if id.trim().is_empty() {
        return Err(anyhow!("workflow.id cannot be empty"));
    }

    let Some(name) = root.get("name").and_then(|value| value.as_str()) else {
        return Err(anyhow!("workflow.name must be a string"));
    };
    if name.trim().is_empty() {
        return Err(anyhow!("workflow.name cannot be empty"));
    }

    let Some(version) = root.get("version").and_then(|value| value.as_str()) else {
        return Err(anyhow!("workflow.version must be a string"));
    };
    if version.trim().is_empty() {
        return Err(anyhow!("workflow.version cannot be empty"));
    }

    let Some(inputs) = root.get("inputs").and_then(|value| value.as_object()) else {
        return Err(anyhow!("workflow.inputs must be an object"));
    };
    for (key, value) in inputs {
        if !value.is_string() {
            return Err(anyhow!("workflow.inputs.{key} must be a string"));
        }
    }

    let Some(policy) = root.get("policy").and_then(|value| value.as_object()) else {
        return Err(anyhow!("workflow.policy must be an object"));
    };
    ensure_only_allowed_fields(
        policy,
        &["sensitivity_max", "model_class"],
        "workflow.policy",
    )?;
    let Some(sensitivity_max) = policy
        .get("sensitivity_max")
        .and_then(|value| value.as_str())
    else {
        return Err(anyhow!("workflow.policy.sensitivity_max must be a string"));
    };
    if !matches!(sensitivity_max, "shareable" | "restricted" | "private") {
        return Err(anyhow!(
            "workflow.policy.sensitivity_max must be one of shareable|restricted|private"
        ));
    }
    let Some(model_class) = policy.get("model_class").and_then(|value| value.as_str()) else {
        return Err(anyhow!("workflow.policy.model_class must be a string"));
    };
    if !matches!(model_class, "local" | "hybrid" | "cloud") {
        return Err(anyhow!(
            "workflow.policy.model_class must be one of local|hybrid|cloud"
        ));
    }

    let Some(steps) = root.get("steps").and_then(|value| value.as_array()) else {
        return Err(anyhow!("workflow.steps must be an array"));
    };
    if steps.is_empty() {
        return Err(anyhow!("workflow.steps cannot be empty"));
    }
    for (index, step) in steps.iter().enumerate() {
        let Some(step_obj) = step.as_object() else {
            return Err(anyhow!("workflow.steps[{index}] must be an object"));
        };
        ensure_fields(
            step_obj,
            &["id", "step_type", "config"],
            &["agent_role"],
            &format!("workflow.steps[{index}]"),
        )?;

        let Some(step_id) = step_obj.get("id").and_then(|value| value.as_str()) else {
            return Err(anyhow!("workflow.steps[{index}].id must be a string"));
        };
        if step_id.trim().is_empty() {
            return Err(anyhow!("workflow.steps[{index}].id cannot be empty"));
        }

        let Some(step_type) = step_obj.get("step_type").and_then(|value| value.as_str()) else {
            return Err(anyhow!(
                "workflow.steps[{index}].step_type must be a string"
            ));
        };
        if step_type.trim().is_empty() {
            return Err(anyhow!("workflow.steps[{index}].step_type cannot be empty"));
        }

        let Some(config) = step_obj.get("config").and_then(|value| value.as_object()) else {
            return Err(anyhow!("workflow.steps[{index}].config must be an object"));
        };
        for (key, value) in config {
            if !value.is_string() {
                return Err(anyhow!(
                    "workflow.steps[{index}].config.{key} must be a string"
                ));
            }
        }
    }

    Ok(())
}

fn ensure_only_allowed_fields(
    object: &serde_json::Map<String, serde_json::Value>,
    required: &[&str],
    context: &str,
) -> Result<()> {
    ensure_fields(object, required, &[], context)
}

fn ensure_fields(
    object: &serde_json::Map<String, serde_json::Value>,
    required: &[&str],
    optional: &[&str],
    context: &str,
) -> Result<()> {
    for field in required {
        if !object.contains_key(*field) {
            return Err(anyhow!("{context} missing required field `{field}`"));
        }
    }

    for key in object.keys() {
        if !required.iter().any(|field| field == key) && !optional.iter().any(|field| field == key)
        {
            return Err(anyhow!("{context} has unsupported field `{key}`"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    struct OutputExecutor;
    impl StepExecutor for OutputExecutor {
        fn execute(&self, step: &WorkflowStep, ctx: &WorkflowContext) -> Result<StepResult> {
            let mut outputs = HashMap::new();
            outputs.insert(format!("{}_done", step.id), "1".to_string());
            outputs.insert("run_id".to_string(), ctx.run_id.clone());
            Ok(StepResult {
                step_id: step.id.clone(),
                status: StepStatus::Success,
                outputs,
                error: None,
            })
        }
    }

    struct FailingExecutor;
    impl StepExecutor for FailingExecutor {
        fn execute(&self, step: &WorkflowStep, _ctx: &WorkflowContext) -> Result<StepResult> {
            Ok(StepResult {
                step_id: step.id.clone(),
                status: StepStatus::Failed,
                outputs: HashMap::new(),
                error: Some("simulated failure".to_string()),
            })
        }
    }

    fn sample_workflow() -> Workflow {
        Workflow {
            id: "wf-intake".to_string(),
            name: "Intake Workflow".to_string(),
            version: "1.0".to_string(),
            inputs: HashMap::new(),
            policy: WorkflowPolicy {
                sensitivity_max: "shareable".to_string(),
                model_class: "local".to_string(),
            },
            steps: vec![
                WorkflowStep {
                    id: "normalize".to_string(),
                    step_type: "intake.normalize".to_string(),
                    config: HashMap::new(),
                    agent_role: None,
                },
                WorkflowStep {
                    id: "store".to_string(),
                    step_type: "archive.store".to_string(),
                    config: HashMap::new(),
                    agent_role: None,
                },
            ],
        }
    }

    fn temp_dir(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "symbiotic_workflow_runner_{name}_{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("time")
                .as_nanos()
        ))
    }

    #[test]
    fn run_succeeds_with_registered_executors() {
        let runner = WorkflowRunner::new()
            .with_executor("intake.normalize", Arc::new(OutputExecutor))
            .with_executor("archive.store", Arc::new(OutputExecutor));
        let result = runner.run(&sample_workflow()).expect("run should succeed");
        assert_eq!(result.status, WorkflowStatus::Success);
        assert_eq!(result.step_results.len(), 2);
        assert_eq!(result.step_results[0].status, StepStatus::Success);
    }

    #[test]
    fn run_fails_when_executor_missing() {
        let runner =
            WorkflowRunner::new().with_executor("intake.normalize", Arc::new(OutputExecutor));
        let result = runner
            .run(&sample_workflow())
            .expect("run should return result");
        assert_eq!(result.status, WorkflowStatus::Failed);
        assert_eq!(result.step_results.len(), 2);
        assert!(result.step_results[1]
            .error
            .as_deref()
            .unwrap_or("")
            .contains("missing executor"));
    }

    #[test]
    fn run_stops_after_failed_step() {
        let runner = WorkflowRunner::new()
            .with_executor("intake.normalize", Arc::new(FailingExecutor))
            .with_executor("archive.store", Arc::new(OutputExecutor));
        let result = runner
            .run(&sample_workflow())
            .expect("run should return result");
        assert_eq!(result.status, WorkflowStatus::Failed);
        assert_eq!(result.step_results.len(), 1);
        assert_eq!(result.step_results[0].status, StepStatus::Failed);
    }

    #[test]
    fn validate_rejects_duplicate_step_ids() {
        let mut wf = sample_workflow();
        wf.steps.push(WorkflowStep {
            id: "store".to_string(),
            step_type: "extra.step".to_string(),
            config: HashMap::new(),
            agent_role: None,
        });
        let err = validate_workflow(&wf).expect_err("must reject duplicate ids");
        assert!(err.to_string().contains("duplicate step id"));
    }

    #[test]
    fn registry_loads_mvp_template() {
        let registry = WorkflowRegistry::with_mvp_templates();
        let workflow = registry
            .get("intake-url")
            .expect("template should be present");
        assert_eq!(workflow.steps.len(), 5);
    }

    #[test]
    fn registry_loads_json_templates_from_directory() {
        let dir = temp_dir("json-load");
        fs::create_dir_all(&dir).expect("dir create");
        let payload = r#"{
  "id": "wf-custom",
  "name": "custom-workflow",
  "version": "1.0",
  "inputs": {},
  "policy": { "sensitivity_max": "restricted", "model_class": "hybrid" },
  "steps": [
    { "id": "step-1", "step_type": "custom.step", "config": {} }
  ]
}"#;
        fs::write(dir.join("custom.json"), payload).expect("template write");

        let registry = WorkflowRegistry::load_from_dir(&dir).expect("registry load");
        let loaded = registry
            .get("custom-workflow")
            .expect("custom template should be present");
        assert_eq!(loaded.id, "wf-custom");
        assert_eq!(loaded.steps.len(), 1);
    }

    #[test]
    fn registry_rejects_invalid_json_template() {
        let dir = temp_dir("invalid-json");
        fs::create_dir_all(&dir).expect("dir create");
        fs::write(dir.join("broken.json"), "{not valid").expect("template write");

        let result = WorkflowRegistry::load_from_dir(&dir);
        assert!(result.is_err(), "invalid json should fail");
        let err = result.err().expect("error should be present");
        assert!(err.to_string().contains("invalid workflow JSON"));
    }

    #[test]
    fn registry_rejects_unknown_fields() {
        let dir = temp_dir("unknown-fields");
        fs::create_dir_all(&dir).expect("dir create");
        let payload = r#"{
  "id": "wf-custom",
  "name": "custom-workflow",
  "version": "1.0",
  "inputs": {},
  "policy": { "sensitivity_max": "restricted", "model_class": "hybrid", "extra": "x" },
  "steps": [
    { "id": "step-1", "step_type": "custom.step", "config": {}, "unexpected": "value" }
  ],
  "extra_root": "x"
}"#;
        fs::write(dir.join("custom.json"), payload).expect("template write");

        let result = WorkflowRegistry::load_from_dir(&dir);
        assert!(result.is_err(), "unknown fields should fail");
        let err = result.err().expect("error should be present");
        assert!(
            err.to_string().contains("invalid workflow schema")
                || err.to_string().contains("invalid workflow JSON")
        );
    }

    #[test]
    fn registry_load_from_dirs_applies_later_override() {
        let shipped = temp_dir("shipped");
        let overrides = temp_dir("overrides");
        fs::create_dir_all(&shipped).expect("shipped create");
        fs::create_dir_all(&overrides).expect("overrides create");

        let shipped_payload = r#"{
  "id": "wf-intake",
  "name": "intake-url",
  "version": "1.0",
  "inputs": {},
  "policy": { "sensitivity_max": "shareable", "model_class": "local" },
  "steps": [ { "id": "s1", "step_type": "intake.normalize", "config": {} } ]
}"#;
        let override_payload = r#"{
  "id": "wf-intake-override",
  "name": "intake-url",
  "version": "2.0",
  "inputs": {},
  "policy": { "sensitivity_max": "restricted", "model_class": "hybrid" },
  "steps": [ { "id": "s2", "step_type": "intake.dedupe", "config": {} } ]
}"#;
        fs::write(shipped.join("intake-url.json"), shipped_payload).expect("write shipped");
        fs::write(overrides.join("intake-url.json"), override_payload).expect("write override");

        let registry = WorkflowRegistry::load_from_dirs([shipped.as_path(), overrides.as_path()])
            .expect("load dirs");
        let workflow = registry.get("intake-url").expect("workflow present");
        assert_eq!(workflow.id, "wf-intake-override");
        assert_eq!(workflow.version, "2.0");
        assert_eq!(workflow.steps[0].id, "s2");
    }

    #[test]
    fn registry_rejects_non_string_step_config_values() {
        let dir = temp_dir("invalid-config-values");
        fs::create_dir_all(&dir).expect("dir create");
        let payload = r#"{
  "id": "wf-custom",
  "name": "custom-workflow",
  "version": "1.0",
  "inputs": {},
  "policy": { "sensitivity_max": "restricted", "model_class": "hybrid" },
  "steps": [
    { "id": "step-1", "step_type": "custom.step", "config": { "limit": 10 } }
  ]
}"#;
        fs::write(dir.join("custom.json"), payload).expect("template write");

        let result = WorkflowRegistry::load_from_dir(&dir);
        assert!(result.is_err(), "non-string config values should fail");
        let err = result.err().expect("error should be present");
        assert!(
            err.to_string().contains("invalid workflow schema"),
            "unexpected error: {err}"
        );
    }
}
