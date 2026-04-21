//! Lightweight Axum HTTP API for serving archive entries and thread memory docs
//! to the mobile app.
//!
//! Runs on a separate port (default 8090) alongside the Matrix event loop.
//! Auth: validates Matrix access tokens against the homeserver's `/account/whoami`.

use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use symbiotic_archive::FileArchiveStore;
use symbiotic_git_swarm::types::{PushAuthRequest, PushAuthResponse};
use symbiotic_memory::recall_probes::{
    RecallProbeResult, RecallProbeRunOutcome, RecallProbeStore, RecallProbeTargetKind,
    RecallRemediationFlag,
};
use symbiotic_memory::self_improvement::MemoryIntegritySnapshot;
use symbiotic_memory::sqlite::SqliteMemoryStore;
use symbiotic_memory::store::MemoryStore;
use symbiotic_memory::types::{Entity, EntityType};
use symbiotic_memory::vault_layout::find_canonical_entity_file;
use tokio::net::TcpListener;
use tracing::{info, warn};

use crate::entity_profiles::{
    preserved_referenced_in_titles, EntityProfileArchivedFactView, EntityProfileFactView,
    EntityProfileGenerator, EntityProfileRelationshipHistoryView, EntityProfileRelationshipView,
    EntityProfileViewData,
};
use crate::memory_docs::compute_content_hash;

/// Shared state for all HTTP handlers.
#[derive(Clone)]
pub struct HttpApiState {
    pub archive_store: Arc<FileArchiveStore>,
    pub memory_store: Arc<SqliteMemoryStore>,
    pub probe_store_path: PathBuf,
    pub homeserver_url: String,
    /// Path to the knowledge-base root (for serving thread memory docs).
    /// When `None`, thread memory doc endpoints return 404.
    pub kb_root: Option<PathBuf>,
    /// Optional swarm subsystem used by git pre-receive authorization.
    pub swarm: Option<Arc<crate::swarm_server::SwarmServer>>,
}

/// Query parameters for the entries list endpoint.
#[derive(Debug, Deserialize)]
pub struct EntriesQuery {
    /// Only return entries updated at or after this Unix timestamp.
    #[serde(default)]
    pub since: u64,
    /// Maximum number of entries to return (default 100, max 500).
    #[serde(default = "default_limit")]
    pub limit: usize,
    /// Offset for pagination (default 0).
    #[serde(default)]
    pub offset: usize,
}

fn default_limit() -> usize {
    100
}

/// Query parameters for the count endpoint.
#[derive(Debug, Deserialize)]
pub struct CountQuery {
    #[serde(default)]
    pub since: u64,
}

/// Response for the count endpoint.
#[derive(Serialize)]
pub struct CountResponse {
    pub total: usize,
    pub since: usize,
}

#[derive(Debug, Deserialize)]
pub struct EntityProfileQuery {
    pub query: String,
}

/// An archive entry serialized for the HTTP API.
/// Adds `content_hash` (SHA-256 of content) for change detection.
#[derive(Serialize)]
pub struct ArchiveEntryResponse {
    pub record_id: String,
    pub title: String,
    pub source_url: Option<String>,
    pub tags: Vec<String>,
    pub sensitivity: String,
    pub updated_at: u64,
    pub content: String,
    pub content_hash: String,
}

#[derive(Serialize)]
pub struct EntityProfileResponse {
    pub entity_id: String,
    pub title: String,
    pub content: String,
    pub content_hash: String,
    pub summary: Option<String>,
    pub facts: Vec<EntityProfileFactView>,
    pub archived_facts: Vec<EntityProfileArchivedFactView>,
    pub relationships: Vec<EntityProfileRelationshipView>,
    pub relationship_history: Vec<EntityProfileRelationshipHistoryView>,
    pub referenced_in: Vec<String>,
}

#[derive(Serialize)]
pub struct EntityProfileHashResponse {
    pub entity_id: String,
    pub title: String,
    pub content_hash: String,
}

#[derive(Debug, Deserialize)]
pub struct EntityHistoryDiffQuery {
    pub query: String,
    pub commit: String,
}

#[derive(Serialize)]
pub struct EntityGitHistoryEntryResponse {
    pub commit_hash: String,
    pub timestamp: i64,
    pub author: String,
    pub summary: String,
}

#[derive(Serialize)]
pub struct EntityGitHistoryResponse {
    pub entity_id: String,
    pub title: String,
    pub file_path: String,
    pub entries: Vec<EntityGitHistoryEntryResponse>,
}

#[derive(Serialize)]
pub struct EntityGitDiffResponse {
    pub entity_id: String,
    pub title: String,
    pub file_path: String,
    pub commit_hash: String,
    pub diff: String,
}

#[derive(Debug, Deserialize)]
pub struct RecallProbeHealthQuery {
    #[serde(default = "default_recall_probe_limit")]
    pub limit: usize,
}

#[derive(Debug, Deserialize)]
pub struct RecallProbeSummaryQuery {
    pub target_kind: String,
    pub target_id: String,
}

#[derive(Debug, Deserialize)]
pub struct RecallProbeRegressionsQuery {
    pub run_id: String,
    #[serde(default = "default_recall_probe_limit")]
    pub limit: usize,
}

fn default_recall_probe_limit() -> usize {
    50
}

#[derive(Serialize)]
pub struct RecallProbeHealthRowResponse {
    pub target_kind: String,
    pub target_id: String,
    pub status: String,
    pub success_rate: f64,
    pub consecutive_failures: u32,
    pub last_checked_at: u64,
    pub last_run_id: String,
    pub remediation_flags: Vec<String>,
    pub failed_queries: Vec<String>,
}

#[derive(Serialize)]
pub struct RecallProbeHealthResponse {
    pub limit: usize,
    pub tracked_target_count: usize,
    pub status_counts: std::collections::BTreeMap<String, usize>,
    pub summaries: Vec<RecallProbeHealthRowResponse>,
}

#[derive(Debug, Deserialize)]
pub struct MemoryIntegrityQuery {
    #[serde(default = "default_recall_probe_limit")]
    pub limit: usize,
}

#[derive(Serialize)]
pub struct MemoryIntegrityEvidenceResponse {
    pub source_label: String,
    pub source_url: Option<String>,
    pub evidence_quote: Option<String>,
}

#[derive(Serialize)]
pub struct MemoryIntegrityContradictionResponse {
    pub entity_id: String,
    pub entity_name: String,
    pub entity_type: String,
    pub memory_a_id: String,
    pub memory_a_fact: String,
    pub memory_b_id: String,
    pub memory_b_fact: String,
    pub description: String,
    pub suggestion: String,
    pub needs_review: bool,
    pub resolution_confidence_percent: u8,
    pub preferred_memory_id: Option<String>,
    pub preferred_fact: Option<String>,
    pub preferred_reason: Option<String>,
    pub investigation_summary: String,
    pub memory_a_evidence: Vec<MemoryIntegrityEvidenceResponse>,
    pub memory_b_evidence: Vec<MemoryIntegrityEvidenceResponse>,
}

#[derive(Serialize)]
pub struct MemoryIntegrityResponse {
    pub limit: usize,
    pub tracked_entity_count: usize,
    pub entities_with_contradictions: usize,
    pub contradiction_count: usize,
    pub review_count: usize,
    pub contradictions: Vec<MemoryIntegrityContradictionResponse>,
}

#[derive(Serialize)]
pub struct RecallProbeSummaryResponse {
    pub target_kind: String,
    pub target_id: String,
    pub status: String,
    pub success_rate: f64,
    pub consecutive_failures: u32,
    pub last_checked_at: u64,
    pub last_run_id: String,
    pub query_count: usize,
    pub matched_query_count: usize,
    pub remediation_flags: Vec<String>,
    pub failed_queries: Vec<String>,
}

#[derive(Serialize)]
pub struct RecallProbeRunStatusResponse {
    pub run_id: String,
    pub cohort: Option<String>,
    pub started_at: u64,
    pub finished_at: Option<u64>,
    pub top_k: usize,
    pub subject_count: usize,
    pub matched_subject_count: usize,
    pub query_count: usize,
    pub unmatched_subject_count: usize,
    pub unmatched_target_ids: Vec<String>,
}

#[derive(Serialize)]
pub struct RecallProbeOutcomeDeltaResponse {
    pub target_kind: String,
    pub target_id: String,
    pub previous_matched: bool,
    pub current_matched: bool,
    pub previous_best_rank: Option<u32>,
    pub current_best_rank: Option<u32>,
}

#[derive(Serialize)]
pub struct RecallProbeRegressionsResponse {
    pub current_run_id: String,
    pub baseline_run_id: String,
    pub limit: usize,
    pub current_subject_count: usize,
    pub baseline_subject_count: usize,
    pub regression_count: usize,
    pub improvement_count: usize,
    pub stable_reachable_count: usize,
    pub stable_unreachable_count: usize,
    pub newly_tracked_count: usize,
    pub dropped_target_count: usize,
    pub regressions: Vec<RecallProbeOutcomeDeltaResponse>,
    pub improvements: Vec<RecallProbeOutcomeDeltaResponse>,
}

/// Build the Axum router with all API routes.
pub fn router(state: HttpApiState) -> Router {
    Router::new()
        .route("/api/archive/entries", get(list_entries))
        .route("/api/archive/entry/{record_id}", get(get_entry))
        .route("/api/archive/count", get(count_entries))
        .route("/api/threads/{thread_id}/memory", get(get_thread_memory))
        .route(
            "/api/threads/{thread_id}/memory/hash",
            get(get_thread_memory_hash),
        )
        .route(
            "/api/threads/{thread_id}/memory/history",
            get(get_thread_git_history),
        )
        .route(
            "/api/threads/{thread_id}/memory/diff",
            get(get_thread_git_diff),
        )
        .route("/api/entities/profile", get(get_entity_profile))
        .route("/api/entities/profile/hash", get(get_entity_profile_hash))
        .route("/api/entities/history", get(get_entity_git_history))
        .route("/api/entities/history/diff", get(get_entity_git_diff))
        .route("/api/recall-probes/health", get(get_recall_probe_health))
        .route(
            "/api/memory-integrity/contradictions",
            get(get_memory_integrity),
        )
        .route("/api/recall-probes/summary", get(get_recall_probe_summary))
        .route(
            "/api/recall-probes/runs/{run_id}",
            get(get_recall_probe_run_status),
        )
        .route(
            "/api/recall-probes/regressions",
            get(get_recall_probe_regressions),
        )
        .route("/api/git/authorize", post(authorize_git_push))
        .route("/api/health", get(health))
        .with_state(state)
}

/// Start the HTTP API server on the given port.
/// This function runs forever — call it from `tokio::spawn`.
pub async fn serve(state: HttpApiState, port: u16) -> anyhow::Result<()> {
    let app = router(state);
    let listener = TcpListener::bind(format!("0.0.0.0:{port}")).await?;
    info!("http_api: listening on 0.0.0.0:{port}");
    axum::serve(listener, app).await?;
    Ok(())
}

/// Extract and validate the Matrix access token from the Authorization header.
/// Returns the Matrix user ID on success, or an error response on failure.
async fn validate_token(
    headers: &HeaderMap,
    homeserver_url: &str,
) -> Result<String, (StatusCode, String)> {
    let auth_header = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .ok_or((
            StatusCode::UNAUTHORIZED,
            "Missing Authorization header".to_string(),
        ))?;

    let token = auth_header.strip_prefix("Bearer ").ok_or((
        StatusCode::UNAUTHORIZED,
        "Authorization header must be: Bearer <token>".to_string(),
    ))?;

    // Validate token against homeserver's whoami endpoint.
    let client = reqwest::Client::new();
    let whoami_url = format!("{}/_matrix/client/v3/account/whoami", homeserver_url);
    let resp = client
        .get(&whoami_url)
        .bearer_auth(token)
        .send()
        .await
        .map_err(|e| {
            warn!("http_api: whoami request failed: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Token validation failed".to_string(),
            )
        })?;

    if !resp.status().is_success() {
        return Err((
            StatusCode::UNAUTHORIZED,
            "Invalid or expired access token".to_string(),
        ));
    }

    #[derive(Deserialize)]
    struct WhoAmI {
        user_id: String,
    }

    let whoami: WhoAmI = resp.json().await.map_err(|e| {
        warn!("http_api: whoami parse failed: {e}");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Token validation failed".to_string(),
        )
    })?;

    Ok(whoami.user_id)
}

fn content_hash(content: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    format!("{:x}", hasher.finalize())
}

// --- Handlers ---

async fn health() -> &'static str {
    "ok"
}

async fn authorize_git_push(
    State(state): State<HttpApiState>,
    Json(request): Json<PushAuthRequest>,
) -> (StatusCode, Json<PushAuthResponse>) {
    let Some(swarm) = state.swarm.as_ref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(PushAuthResponse {
                allowed: false,
                reason: Some("swarm server unavailable".to_string()),
            }),
        );
    };

    (StatusCode::OK, Json(swarm.authorize_push(request).await))
}

async fn list_entries(
    State(state): State<HttpApiState>,
    headers: HeaderMap,
    Query(params): Query<EntriesQuery>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let _user_id = validate_token(&headers, &state.homeserver_url).await?;

    let limit = params.limit.min(500);
    let docs = state
        .archive_store
        .list_since(params.since, limit, params.offset)
        .map_err(|e| {
            warn!("http_api: list_since failed: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to list entries".to_string(),
            )
        })?;

    let entries: Vec<ArchiveEntryResponse> = docs
        .into_iter()
        .map(|doc| ArchiveEntryResponse {
            content_hash: content_hash(&doc.content),
            record_id: doc.record_id,
            title: doc.title,
            source_url: doc.source_url,
            tags: doc.tags,
            sensitivity: sensitivity_str(&doc.sensitivity),
            updated_at: doc.updated_at,
            content: doc.content,
        })
        .collect();

    Ok(Json(entries))
}

async fn get_entry(
    State(state): State<HttpApiState>,
    headers: HeaderMap,
    Path(record_id): Path<String>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let _user_id = validate_token(&headers, &state.homeserver_url).await?;

    let doc = state.archive_store.get(&record_id).map_err(|e| {
        warn!("http_api: get entry failed: {e}");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to get entry".to_string(),
        )
    })?;

    match doc {
        Some(doc) => {
            let entry = ArchiveEntryResponse {
                content_hash: content_hash(&doc.content),
                record_id: doc.record_id,
                title: doc.title,
                source_url: doc.source_url,
                tags: doc.tags,
                sensitivity: sensitivity_str(&doc.sensitivity),
                updated_at: doc.updated_at,
                content: doc.content,
            };
            Ok(Json(entry).into_response())
        }
        None => Err((StatusCode::NOT_FOUND, "Entry not found".to_string())),
    }
}

async fn count_entries(
    State(state): State<HttpApiState>,
    headers: HeaderMap,
    Query(params): Query<CountQuery>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let _user_id = validate_token(&headers, &state.homeserver_url).await?;

    let total = state.archive_store.count().map_err(|e| {
        warn!("http_api: count failed: {e}");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to count entries".to_string(),
        )
    })?;

    let since = if params.since > 0 {
        state.archive_store.count_since(params.since).map_err(|e| {
            warn!("http_api: count_since failed: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to count entries".to_string(),
            )
        })?
    } else {
        total
    };

    Ok(Json(CountResponse { total, since }))
}

/// Response for the memory hash endpoint.
#[derive(Serialize)]
pub struct MemoryHashResponse {
    pub thread_id: String,
    pub content_hash: String,
}

#[derive(Serialize)]
pub struct ThreadGitHistoryEntryResponse {
    pub commit_hash: String,
    pub timestamp: i64,
    pub author: String,
    pub summary: String,
}

#[derive(Serialize)]
pub struct ThreadGitHistoryResponse {
    pub thread_id: String,
    pub file_path: String,
    pub entries: Vec<ThreadGitHistoryEntryResponse>,
}

#[derive(Serialize)]
pub struct ThreadGitDiffResponse {
    pub thread_id: String,
    pub file_path: String,
    pub commit_hash: String,
    pub diff: String,
}

#[derive(Debug, Deserialize)]
pub struct ThreadHistoryDiffQuery {
    pub commit: String,
}

/// Read the thread memory doc from disk. Returns `(content, content_hash)`.
fn read_thread_memory_doc(
    kb_root: &std::path::Path,
    thread_id: &str,
) -> Result<(String, String), (StatusCode, String)> {
    let gen = crate::memory_docs::ThreadMemoryDocGenerator::new(kb_root);
    let doc_path = gen.doc_path(thread_id);

    if !doc_path.exists() {
        return Err((
            StatusCode::NOT_FOUND,
            format!("Thread memory doc not found for {thread_id}"),
        ));
    }

    let content = std::fs::read_to_string(&doc_path).map_err(|e| {
        warn!("http_api: failed to read thread memory doc: {e}");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to read thread memory doc".to_string(),
        )
    })?;

    let hash = compute_content_hash(&content);
    Ok((content, hash))
}

fn thread_git_file_path(
    kb_root: &std::path::Path,
    thread_id: &str,
) -> Result<String, (StatusCode, String)> {
    let generator = crate::memory_docs::ThreadMemoryDocGenerator::new(kb_root);
    let path = generator.doc_path(thread_id);
    if !path.exists() {
        return Err((
            StatusCode::NOT_FOUND,
            format!("Thread memory doc not found for {thread_id}"),
        ));
    }
    let repo_root = resolve_git_repo_root(kb_root);
    Ok(path
        .strip_prefix(&repo_root)
        .unwrap_or(&path)
        .to_string_lossy()
        .to_string())
}

/// GET /api/threads/{thread_id}/memory — returns the Thread Memory Doc as Markdown.
async fn get_thread_memory(
    State(state): State<HttpApiState>,
    headers: HeaderMap,
    Path(thread_id): Path<String>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let _user_id = validate_token(&headers, &state.homeserver_url).await?;

    let kb_root = state.kb_root.as_ref().ok_or((
        StatusCode::NOT_FOUND,
        "Knowledge base not configured".to_string(),
    ))?;

    let (content, hash) = read_thread_memory_doc(kb_root, &thread_id)?;

    Ok((
        StatusCode::OK,
        [
            ("content-type", "text/markdown; charset=utf-8"),
            // Leak the hash string into a &'static str for the header value.
            // This is safe because the string is small and short-lived in the
            // response pipeline.
        ],
        [(axum::http::header::ETAG, hash)],
        content,
    ))
}

/// GET /api/threads/{thread_id}/memory/hash — returns just the content hash.
async fn get_thread_memory_hash(
    State(state): State<HttpApiState>,
    headers: HeaderMap,
    Path(thread_id): Path<String>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let _user_id = validate_token(&headers, &state.homeserver_url).await?;

    let kb_root = state.kb_root.as_ref().ok_or((
        StatusCode::NOT_FOUND,
        "Knowledge base not configured".to_string(),
    ))?;

    let (_content, hash) = read_thread_memory_doc(kb_root, &thread_id)?;

    Ok(Json(MemoryHashResponse {
        thread_id,
        content_hash: hash,
    }))
}

fn parse_thread_git_file_history(stdout: &str) -> Vec<ThreadGitHistoryEntryResponse> {
    stdout
        .lines()
        .filter_map(|line| {
            let mut parts = line.splitn(4, '|');
            let commit_hash = parts.next()?.trim().to_string();
            let timestamp = parts.next()?.trim().parse::<i64>().ok()?;
            let author = parts.next()?.trim().to_string();
            let summary = parts.next()?.trim().to_string();
            Some(ThreadGitHistoryEntryResponse {
                commit_hash,
                timestamp,
                author,
                summary,
            })
        })
        .collect()
}

fn read_thread_git_history(
    repo_root: &std::path::Path,
    file_path: &str,
) -> Result<Vec<ThreadGitHistoryEntryResponse>, (StatusCode, String)> {
    let output = Command::new("git")
        .args(["log", "--format=%H|%at|%an|%s", "--", file_path])
        .current_dir(repo_root)
        .output()
        .map_err(|e| {
            warn!("http_api: git log failed for {file_path}: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to read thread history".to_string(),
            )
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        warn!("http_api: git log failed for {file_path}: {stderr}");
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to read thread history".to_string(),
        ));
    }

    Ok(parse_thread_git_file_history(&String::from_utf8_lossy(
        &output.stdout,
    )))
}

fn read_thread_git_diff(
    repo_root: &std::path::Path,
    file_path: &str,
    commit_hash: &str,
) -> Result<String, (StatusCode, String)> {
    if !valid_commit_hash(commit_hash) {
        return Err((StatusCode::BAD_REQUEST, "Invalid commit hash".to_string()));
    }

    let output = Command::new("git")
        .args([
            "show",
            "--format=medium",
            "--stat",
            "--patch",
            commit_hash,
            "--",
            file_path,
        ])
        .current_dir(repo_root)
        .output()
        .map_err(|e| {
            warn!("http_api: git show failed for {commit_hash} {file_path}: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to read thread diff".to_string(),
            )
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        warn!("http_api: git show failed for {commit_hash} {file_path}: {stderr}");
        return Err((StatusCode::NOT_FOUND, "Thread diff not found".to_string()));
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

async fn get_thread_git_history(
    State(state): State<HttpApiState>,
    headers: HeaderMap,
    Path(thread_id): Path<String>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let _user_id = validate_token(&headers, &state.homeserver_url).await?;

    let kb_root = state.kb_root.as_ref().ok_or((
        StatusCode::NOT_FOUND,
        "Knowledge base not configured".to_string(),
    ))?;

    let file_path = thread_git_file_path(kb_root, &thread_id)?;
    let repo_root = resolve_git_repo_root(kb_root);
    let entries = read_thread_git_history(&repo_root, &file_path)?;

    Ok(Json(ThreadGitHistoryResponse {
        thread_id,
        file_path,
        entries,
    }))
}

async fn get_thread_git_diff(
    State(state): State<HttpApiState>,
    headers: HeaderMap,
    Path(thread_id): Path<String>,
    Query(params): Query<ThreadHistoryDiffQuery>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let _user_id = validate_token(&headers, &state.homeserver_url).await?;

    let kb_root = state.kb_root.as_ref().ok_or((
        StatusCode::NOT_FOUND,
        "Knowledge base not configured".to_string(),
    ))?;

    let file_path = thread_git_file_path(kb_root, &thread_id)?;
    let repo_root = resolve_git_repo_root(kb_root);
    let diff = read_thread_git_diff(&repo_root, &file_path, &params.commit)?;

    Ok(Json(ThreadGitDiffResponse {
        thread_id,
        file_path,
        commit_hash: params.commit,
        diff,
    }))
}

async fn resolve_entity_query(
    memory_store: &SqliteMemoryStore,
    query: &str,
) -> Result<Option<Entity>, (StatusCode, String)> {
    if query.trim().is_empty() {
        return Ok(None);
    }

    if let Ok(entity) = memory_store.get_entity(query).await {
        return Ok(Some(entity));
    }

    let candidates = memory_store.find_entities(query, 10).await.map_err(|e| {
        warn!("http_api: entity lookup failed: {e}");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to resolve entity".to_string(),
        )
    })?;

    if candidates.is_empty() {
        return Ok(None);
    }

    let query_lower = query.trim().to_lowercase();
    if let Some(exact) = candidates.iter().find(|entity| {
        entity.name.to_lowercase() == query_lower
            || entity
                .attributes
                .get("aliases")
                .and_then(|value| value.as_array())
                .map(|aliases| {
                    aliases.iter().any(|alias| {
                        alias
                            .as_str()
                            .map(|value| value.to_lowercase() == query_lower)
                            .unwrap_or(false)
                    })
                })
                .unwrap_or(false)
    }) {
        return Ok(Some(exact.clone()));
    }

    Ok(candidates.into_iter().next())
}

fn read_entity_profile_doc(
    kb_root: &std::path::Path,
    entity: &Entity,
) -> Result<(String, String), (StatusCode, String)> {
    let generator = EntityProfileGenerator::new(kb_root);
    let path = generator.profile_path(entity);
    if !path.exists() {
        return Err((
            StatusCode::NOT_FOUND,
            "Entity profile not found".to_string(),
        ));
    }
    let content = std::fs::read_to_string(&path).map_err(|e| {
        warn!(
            "http_api: failed to read entity profile {}: {e}",
            path.display()
        );
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to read entity profile".to_string(),
        )
    })?;
    Ok((content_hash(&content), content))
}

async fn build_entity_profile_view(
    state: &HttpApiState,
    kb_root: &std::path::Path,
    entity: &Entity,
) -> Result<EntityProfileViewData, (StatusCode, String)> {
    let generator = EntityProfileGenerator::new(kb_root);
    let profile_path = generator.profile_path(entity);
    let memories = state
        .memory_store
        .get_memories(&entity.id, None)
        .await
        .map_err(|e| {
            warn!(
                "http_api: failed to load entity memories {}: {e}",
                entity.id
            );
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to load entity profile".to_string(),
            )
        })?;
    let relationships = state
        .memory_store
        .get_relationships(&entity.id)
        .await
        .map_err(|e| {
            warn!(
                "http_api: failed to load entity relationships {}: {e}",
                entity.id
            );
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to load entity profile".to_string(),
            )
        })?;
    let referenced_in = preserved_referenced_in_titles(&profile_path).map_err(|e| {
        warn!(
            "http_api: failed to load referenced-in titles for {}: {e}",
            entity.id
        );
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to load entity profile".to_string(),
        )
    })?;
    let history = EntityProfileGenerator::load_history_data(kb_root, &entity.id).map_err(|e| {
        warn!(
            "http_api: failed to load entity history for {}: {e}",
            entity.id
        );
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to load entity profile".to_string(),
        )
    })?;
    Ok(EntityProfileGenerator::build_view(
        entity,
        &memories,
        &relationships,
        &history,
        &referenced_in,
    ))
}

async fn get_entity_profile(
    State(state): State<HttpApiState>,
    headers: HeaderMap,
    Query(params): Query<EntityProfileQuery>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let _user_id = validate_token(&headers, &state.homeserver_url).await?;
    let kb_root = state.kb_root.as_ref().ok_or((
        StatusCode::NOT_FOUND,
        "Knowledge base not configured".to_string(),
    ))?;
    let entity = resolve_entity_query(&state.memory_store, &params.query)
        .await?
        .ok_or((StatusCode::NOT_FOUND, "Entity not found".to_string()))?;
    let (content_hash, content) = read_entity_profile_doc(kb_root, &entity)?;
    let view = build_entity_profile_view(&state, kb_root, &entity).await?;
    Ok(Json(EntityProfileResponse {
        entity_id: entity.id,
        title: entity.name,
        content,
        content_hash,
        summary: view.summary,
        facts: view.facts,
        archived_facts: view.archived_facts,
        relationships: view.relationships,
        relationship_history: view.relationship_history,
        referenced_in: view.referenced_in,
    }))
}

async fn get_entity_profile_hash(
    State(state): State<HttpApiState>,
    headers: HeaderMap,
    Query(params): Query<EntityProfileQuery>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let _user_id = validate_token(&headers, &state.homeserver_url).await?;
    let kb_root = state.kb_root.as_ref().ok_or((
        StatusCode::NOT_FOUND,
        "Knowledge base not configured".to_string(),
    ))?;
    let entity = resolve_entity_query(&state.memory_store, &params.query)
        .await?
        .ok_or((StatusCode::NOT_FOUND, "Entity not found".to_string()))?;
    let (content_hash, _content) = read_entity_profile_doc(kb_root, &entity)?;
    Ok(Json(EntityProfileHashResponse {
        entity_id: entity.id,
        title: entity.name,
        content_hash,
    }))
}

fn entity_git_file_path(
    kb_root: &std::path::Path,
    entity: &Entity,
) -> Result<String, (StatusCode, String)> {
    let canonical = find_canonical_entity_file(kb_root, &entity.id).map_err(|e| {
        warn!(
            "http_api: failed to resolve canonical entity file for {}: {e}",
            entity.id
        );
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to resolve entity history".to_string(),
        )
    })?;
    let canonical = canonical.ok_or((
        StatusCode::NOT_FOUND,
        "Canonical entity record not found".to_string(),
    ))?;
    let repo_root = resolve_git_repo_root(kb_root);
    let rel_path = canonical
        .strip_prefix(&repo_root)
        .unwrap_or(&canonical)
        .to_string_lossy()
        .to_string();
    Ok(rel_path)
}

fn parse_git_file_history(stdout: &str) -> Vec<EntityGitHistoryEntryResponse> {
    stdout
        .lines()
        .filter_map(|line| {
            let mut parts = line.splitn(4, '|');
            let commit_hash = parts.next()?.trim().to_string();
            let timestamp = parts.next()?.trim().parse::<i64>().ok()?;
            let author = parts.next()?.trim().to_string();
            let summary = parts.next()?.trim().to_string();
            Some(EntityGitHistoryEntryResponse {
                commit_hash,
                timestamp,
                author,
                summary,
            })
        })
        .collect()
}

fn read_entity_git_history(
    repo_root: &std::path::Path,
    file_path: &str,
) -> Result<Vec<EntityGitHistoryEntryResponse>, (StatusCode, String)> {
    let output = Command::new("git")
        .args(["log", "--format=%H|%at|%an|%s", "--", file_path])
        .current_dir(repo_root)
        .output()
        .map_err(|e| {
            warn!("http_api: git log failed for {file_path}: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to read entity history".to_string(),
            )
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        warn!("http_api: git log failed for {file_path}: {stderr}");
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to read entity history".to_string(),
        ));
    }

    Ok(parse_git_file_history(&String::from_utf8_lossy(
        &output.stdout,
    )))
}

fn valid_commit_hash(commit: &str) -> bool {
    let trimmed = commit.trim();
    (7..=64).contains(&trimmed.len()) && trimmed.chars().all(|c| c.is_ascii_hexdigit())
}

fn resolve_git_repo_root(kb_root: &std::path::Path) -> std::path::PathBuf {
    let output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(kb_root)
        .output();
    if let Ok(output) = output {
        if output.status.success() {
            let root = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !root.is_empty() {
                return std::path::PathBuf::from(root);
            }
        }
    }
    kb_root.parent().unwrap_or(kb_root).to_path_buf()
}

fn read_entity_git_diff(
    repo_root: &std::path::Path,
    file_path: &str,
    commit_hash: &str,
) -> Result<String, (StatusCode, String)> {
    if !valid_commit_hash(commit_hash) {
        return Err((StatusCode::BAD_REQUEST, "Invalid commit hash".to_string()));
    }

    let output = Command::new("git")
        .args([
            "show",
            "--format=medium",
            "--stat",
            "--patch",
            commit_hash,
            "--",
            file_path,
        ])
        .current_dir(repo_root)
        .output()
        .map_err(|e| {
            warn!("http_api: git show failed for {commit_hash} {file_path}: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to read entity diff".to_string(),
            )
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        warn!("http_api: git show failed for {commit_hash} {file_path}: {stderr}");
        return Err((StatusCode::NOT_FOUND, "Entity diff not found".to_string()));
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

async fn get_entity_git_history(
    State(state): State<HttpApiState>,
    headers: HeaderMap,
    Query(params): Query<EntityProfileQuery>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let _user_id = validate_token(&headers, &state.homeserver_url).await?;
    let kb_root = state.kb_root.as_ref().ok_or((
        StatusCode::NOT_FOUND,
        "Knowledge base not configured".to_string(),
    ))?;
    let entity = resolve_entity_query(&state.memory_store, &params.query)
        .await?
        .ok_or((StatusCode::NOT_FOUND, "Entity not found".to_string()))?;
    let file_path = entity_git_file_path(kb_root, &entity)?;
    let repo_root = resolve_git_repo_root(kb_root);
    let entries = read_entity_git_history(&repo_root, &file_path)?;

    Ok(Json(EntityGitHistoryResponse {
        entity_id: entity.id,
        title: entity.name,
        file_path,
        entries,
    }))
}

async fn get_entity_git_diff(
    State(state): State<HttpApiState>,
    headers: HeaderMap,
    Query(params): Query<EntityHistoryDiffQuery>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let _user_id = validate_token(&headers, &state.homeserver_url).await?;
    let kb_root = state.kb_root.as_ref().ok_or((
        StatusCode::NOT_FOUND,
        "Knowledge base not configured".to_string(),
    ))?;
    let entity = resolve_entity_query(&state.memory_store, &params.query)
        .await?
        .ok_or((StatusCode::NOT_FOUND, "Entity not found".to_string()))?;
    let file_path = entity_git_file_path(kb_root, &entity)?;
    let repo_root = resolve_git_repo_root(kb_root);
    let diff = read_entity_git_diff(&repo_root, &file_path, &params.commit)?;

    Ok(Json(EntityGitDiffResponse {
        entity_id: entity.id,
        title: entity.name,
        file_path,
        commit_hash: params.commit,
        diff,
    }))
}

async fn get_recall_probe_health(
    State(state): State<HttpApiState>,
    headers: HeaderMap,
    Query(params): Query<RecallProbeHealthQuery>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let _user_id = validate_token(&headers, &state.homeserver_url).await?;
    let probe_store = RecallProbeStore::open(&state.probe_store_path).map_err(|e| {
        warn!("http_api: open recall probe store failed: {e}");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to open recall probe store".to_string(),
        )
    })?;
    let response = build_recall_probe_health_response(&probe_store, params.limit).map_err(|e| {
        warn!("http_api: build recall probe health failed: {e}");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to load recall probe health".to_string(),
        )
    })?;
    Ok(Json(response))
}

async fn get_memory_integrity(
    State(state): State<HttpApiState>,
    headers: HeaderMap,
    Query(params): Query<MemoryIntegrityQuery>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let _user_id = validate_token(&headers, &state.homeserver_url).await?;
    let response = build_memory_integrity_response(&state.memory_store, params.limit)
        .await
        .map_err(|e| {
            warn!("http_api: build memory integrity failed: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to load memory integrity".to_string(),
            )
        })?;
    Ok(Json(response))
}

async fn get_recall_probe_summary(
    State(state): State<HttpApiState>,
    headers: HeaderMap,
    Query(params): Query<RecallProbeSummaryQuery>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let _user_id = validate_token(&headers, &state.homeserver_url).await?;
    let target_kind = params
        .target_kind
        .parse::<RecallProbeTargetKind>()
        .map_err(|_| {
            (
                StatusCode::BAD_REQUEST,
                "Invalid recall probe target_kind".to_string(),
            )
        })?;
    let probe_store = RecallProbeStore::open(&state.probe_store_path).map_err(|e| {
        warn!("http_api: open recall probe store failed: {e}");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to open recall probe store".to_string(),
        )
    })?;
    let response =
        build_recall_probe_summary_response(&probe_store, target_kind, &params.target_id).map_err(
            |e| {
                warn!("http_api: build recall probe summary failed: {e}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Failed to load recall probe summary".to_string(),
                )
            },
        )?;
    match response {
        Some(response) => Ok(Json(response).into_response()),
        None => Err((
            StatusCode::NOT_FOUND,
            "Recall probe summary not found".to_string(),
        )),
    }
}

async fn get_recall_probe_run_status(
    State(state): State<HttpApiState>,
    headers: HeaderMap,
    Path(run_id): Path<String>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let _user_id = validate_token(&headers, &state.homeserver_url).await?;
    let probe_store = RecallProbeStore::open(&state.probe_store_path).map_err(|e| {
        warn!("http_api: open recall probe store failed: {e}");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to open recall probe store".to_string(),
        )
    })?;
    let response = build_recall_probe_run_status_response(&probe_store, &run_id).map_err(|e| {
        warn!("http_api: build recall probe run status failed: {e}");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to load recall probe run status".to_string(),
        )
    })?;
    match response {
        Some(response) => Ok(Json(response).into_response()),
        None => Err((
            StatusCode::NOT_FOUND,
            "Recall probe run not found".to_string(),
        )),
    }
}

async fn get_recall_probe_regressions(
    State(state): State<HttpApiState>,
    headers: HeaderMap,
    Query(params): Query<RecallProbeRegressionsQuery>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let _user_id = validate_token(&headers, &state.homeserver_url).await?;
    let probe_store = RecallProbeStore::open(&state.probe_store_path).map_err(|e| {
        warn!("http_api: open recall probe store failed: {e}");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to open recall probe store".to_string(),
        )
    })?;
    let response =
        build_recall_probe_regressions_response(&probe_store, &params.run_id, params.limit)
            .map_err(|e| {
                warn!("http_api: build recall probe regressions failed: {e}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Failed to load recall probe regressions".to_string(),
                )
            })?;
    match response {
        RecallProbeRegressionsLookup::Ready(response) => Ok(Json(response).into_response()),
        RecallProbeRegressionsLookup::MissingRun => Err((
            StatusCode::NOT_FOUND,
            "Recall probe run not found".to_string(),
        )),
        RecallProbeRegressionsLookup::MissingBaseline => Err((
            StatusCode::CONFLICT,
            "No previous completed recall probe run available for comparison".to_string(),
        )),
    }
}

fn build_recall_probe_health_response(
    probe_store: &RecallProbeStore,
    limit: usize,
) -> rusqlite::Result<RecallProbeHealthResponse> {
    let summaries = probe_store.list_summaries(limit)?;
    let status_counts = summaries.iter().fold(
        std::collections::BTreeMap::<String, usize>::new(),
        |mut counts, summary| {
            *counts
                .entry(summary.status.as_str().to_string())
                .or_insert(0) += 1;
            counts
        },
    );
    let rows = summaries
        .into_iter()
        .map(|summary| {
            let latest_results = probe_store.results_for_target_in_run(
                &summary.last_run_id,
                summary.target_kind,
                &summary.target_id,
            )?;
            Ok(RecallProbeHealthRowResponse {
                target_kind: summary.target_kind.as_str().to_string(),
                target_id: summary.target_id,
                status: summary.status.as_str().to_string(),
                success_rate: summary.success_rate,
                consecutive_failures: summary.consecutive_failures,
                last_checked_at: summary.last_checked_at,
                last_run_id: summary.last_run_id,
                remediation_flags: recall_probe_flag_labels_from_results(&latest_results),
                failed_queries: latest_results
                    .into_iter()
                    .filter(|result| !result.matched)
                    .map(|result| result.query)
                    .take(3)
                    .collect(),
            })
        })
        .collect::<rusqlite::Result<Vec<_>>>()?;

    Ok(RecallProbeHealthResponse {
        limit,
        tracked_target_count: rows.len(),
        status_counts,
        summaries: rows,
    })
}

async fn build_memory_integrity_response(
    memory_store: &SqliteMemoryStore,
    limit: usize,
) -> Result<MemoryIntegrityResponse, symbiotic_memory::types::MemoryStoreError> {
    let MemoryIntegritySnapshot {
        tracked_entity_count,
        entities_with_contradictions,
        contradiction_count,
        review_count,
        contradictions,
    } = memory_store.memory_integrity_snapshot(limit).await?;

    Ok(MemoryIntegrityResponse {
        limit,
        tracked_entity_count,
        entities_with_contradictions,
        contradiction_count,
        review_count,
        contradictions: contradictions
            .into_iter()
            .map(|row| MemoryIntegrityContradictionResponse {
                entity_id: row.entity_id,
                entity_name: row.entity_name,
                entity_type: entity_type_label(row.entity_type).to_string(),
                memory_a_id: row.memory_a_id,
                memory_a_fact: row.memory_a_fact,
                memory_b_id: row.memory_b_id,
                memory_b_fact: row.memory_b_fact,
                description: row.description,
                suggestion: row.suggestion,
                needs_review: row.needs_review,
                resolution_confidence_percent: row.resolution_confidence_percent,
                preferred_memory_id: row.preferred_memory_id,
                preferred_fact: row.preferred_fact,
                preferred_reason: row.preferred_reason,
                investigation_summary: row.investigation_summary,
                memory_a_evidence: row
                    .memory_a_evidence
                    .into_iter()
                    .map(|evidence| MemoryIntegrityEvidenceResponse {
                        source_label: evidence.source_label,
                        source_url: evidence.source_url,
                        evidence_quote: evidence.evidence_quote,
                    })
                    .collect(),
                memory_b_evidence: row
                    .memory_b_evidence
                    .into_iter()
                    .map(|evidence| MemoryIntegrityEvidenceResponse {
                        source_label: evidence.source_label,
                        source_url: evidence.source_url,
                        evidence_quote: evidence.evidence_quote,
                    })
                    .collect(),
            })
            .collect(),
    })
}

fn build_recall_probe_summary_response(
    probe_store: &RecallProbeStore,
    target_kind: RecallProbeTargetKind,
    target_id: &str,
) -> rusqlite::Result<Option<RecallProbeSummaryResponse>> {
    let Some(summary) = probe_store.summary_for(target_kind, target_id)? else {
        return Ok(None);
    };
    let latest_results =
        probe_store.results_for_target_in_run(&summary.last_run_id, target_kind, target_id)?;
    let matched_query_count = latest_results
        .iter()
        .filter(|result| result.matched)
        .count();

    Ok(Some(RecallProbeSummaryResponse {
        target_kind: summary.target_kind.as_str().to_string(),
        target_id: summary.target_id,
        status: summary.status.as_str().to_string(),
        success_rate: summary.success_rate,
        consecutive_failures: summary.consecutive_failures,
        last_checked_at: summary.last_checked_at,
        last_run_id: summary.last_run_id,
        query_count: latest_results.len(),
        matched_query_count,
        remediation_flags: recall_probe_flag_labels_from_results(&latest_results),
        failed_queries: latest_results
            .into_iter()
            .filter(|result| !result.matched)
            .map(|result| result.query)
            .take(3)
            .collect(),
    }))
}

fn build_recall_probe_run_status_response(
    probe_store: &RecallProbeStore,
    run_id: &str,
) -> rusqlite::Result<Option<RecallProbeRunStatusResponse>> {
    let Some(run) = probe_store.run(run_id)? else {
        return Ok(None);
    };
    let results = probe_store.results_for_run(run_id)?;
    let mut target_matches = std::collections::BTreeMap::<(String, String), bool>::new();
    for result in &results {
        let key = (
            result.target_kind.as_str().to_string(),
            result.target_id.clone(),
        );
        target_matches
            .entry(key)
            .and_modify(|matched| *matched |= result.matched)
            .or_insert(result.matched);
    }
    let unmatched_target_ids = target_matches
        .iter()
        .filter(|(_, matched)| !**matched)
        .map(|((kind, id), _)| format!("{kind}:{id}"))
        .take(10)
        .collect::<Vec<_>>();
    Ok(Some(RecallProbeRunStatusResponse {
        run_id: run.id,
        cohort: run.cohort,
        started_at: run.started_at,
        finished_at: run.finished_at,
        top_k: run.top_k,
        subject_count: run.subject_count,
        matched_subject_count: run.matched_count,
        query_count: results.len(),
        unmatched_subject_count: run.subject_count.saturating_sub(run.matched_count),
        unmatched_target_ids,
    }))
}

enum RecallProbeRegressionsLookup {
    Ready(RecallProbeRegressionsResponse),
    MissingRun,
    MissingBaseline,
}

fn build_recall_probe_regressions_response(
    probe_store: &RecallProbeStore,
    run_id: &str,
    limit: usize,
) -> rusqlite::Result<RecallProbeRegressionsLookup> {
    let Some(current_run) = probe_store.run(run_id)? else {
        return Ok(RecallProbeRegressionsLookup::MissingRun);
    };
    let Some(baseline_run_id) = probe_store.previous_completed_run_id(run_id)? else {
        return Ok(RecallProbeRegressionsLookup::MissingBaseline);
    };

    let baseline_outcomes = probe_store.outcomes_for_run(&baseline_run_id)?;
    let current_outcomes = probe_store.outcomes_for_run(run_id)?;
    let baseline_map = baseline_outcomes
        .into_iter()
        .map(|outcome| {
            (
                (
                    outcome.target_kind.as_str().to_string(),
                    outcome.target_id.clone(),
                ),
                outcome,
            )
        })
        .collect::<std::collections::BTreeMap<_, _>>();
    let current_map = current_outcomes
        .into_iter()
        .map(|outcome| {
            (
                (
                    outcome.target_kind.as_str().to_string(),
                    outcome.target_id.clone(),
                ),
                outcome,
            )
        })
        .collect::<std::collections::BTreeMap<_, _>>();

    let mut regressions = Vec::new();
    let mut improvements = Vec::new();
    let mut stable_unreachable_count = 0usize;
    let mut stable_reachable_count = 0usize;
    let mut newly_tracked_count = 0usize;
    let mut dropped_target_count = 0usize;

    for (key, current) in &current_map {
        let Some(previous) = baseline_map.get(key) else {
            newly_tracked_count += 1;
            continue;
        };
        if previous.matched && !current.matched {
            regressions.push(recall_probe_outcome_delta(previous, current));
        } else if !previous.matched && current.matched {
            improvements.push(recall_probe_outcome_delta(previous, current));
        } else if current.matched {
            stable_reachable_count += 1;
        } else {
            stable_unreachable_count += 1;
        }
    }

    for key in baseline_map.keys() {
        if !current_map.contains_key(key) {
            dropped_target_count += 1;
        }
    }

    Ok(RecallProbeRegressionsLookup::Ready(
        RecallProbeRegressionsResponse {
            current_run_id: run_id.to_string(),
            baseline_run_id,
            limit,
            current_subject_count: current_run.subject_count,
            baseline_subject_count: baseline_map.len(),
            regression_count: regressions.len(),
            improvement_count: improvements.len(),
            stable_reachable_count,
            stable_unreachable_count,
            newly_tracked_count,
            dropped_target_count,
            regressions: regressions.into_iter().take(limit).collect(),
            improvements: improvements.into_iter().take(limit).collect(),
        },
    ))
}

fn recall_probe_outcome_delta(
    previous: &RecallProbeRunOutcome,
    current: &RecallProbeRunOutcome,
) -> RecallProbeOutcomeDeltaResponse {
    RecallProbeOutcomeDeltaResponse {
        target_kind: current.target_kind.as_str().to_string(),
        target_id: current.target_id.clone(),
        previous_matched: previous.matched,
        current_matched: current.matched,
        previous_best_rank: previous.best_rank,
        current_best_rank: current.best_rank,
    }
}

fn recall_probe_flag_labels_from_results(results: &[RecallProbeResult]) -> Vec<String> {
    let mut labels = std::collections::BTreeSet::new();
    for result in results {
        for flag in &result.remediation_flags {
            labels.insert(
                match flag {
                    RecallRemediationFlag::ReembedCandidate => "reembed_candidate",
                    RecallRemediationFlag::KeywordAugmentationCandidate => {
                        "keyword_augmentation_candidate"
                    }
                    RecallRemediationFlag::DerivedEdgeCandidate => "derived_edge_candidate",
                    RecallRemediationFlag::ManualReviewRequired => "manual_review_required",
                }
                .to_string(),
            );
        }
    }
    labels.into_iter().collect()
}

fn entity_type_label(entity_type: EntityType) -> &'static str {
    entity_type.singular_label()
}

fn sensitivity_str(s: &symbiotic_archive::ArchiveSensitivity) -> String {
    match s {
        symbiotic_archive::ArchiveSensitivity::Shareable => "shareable".to_string(),
        symbiotic_archive::ArchiveSensitivity::Restricted => "restricted".to_string(),
        symbiotic_archive::ArchiveSensitivity::Private => "private".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use serde_json::json;
    use symbiotic_memory::types::{
        AllowedModels, EntityStatus, EntityType, MemorySpace, Sensitivity,
    };
    use tower::util::ServiceExt;

    fn test_state() -> HttpApiState {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Arc::new(FileArchiveStore::open(dir.path()).expect("store"));
        let memory_store = Arc::new(SqliteMemoryStore::open_in_memory().expect("memory"));
        // Store a test entry.
        store
            .store(symbiotic_archive::StoreRequest {
                title_hint: Some("Test Entry".to_string()),
                content: "# Test\n\nHello [[World]]".to_string(),
                source_url: Some("https://example.com".to_string()),
                tags: vec!["test".to_string()],
                sensitivity: symbiotic_archive::ArchiveSensitivity::Shareable,
                idempotency_key: "test-1".to_string(),
                firewall_verdict: Some(symbiotic_archive::trusted_skip_verdict()),
            })
            .expect("store entry");
        HttpApiState {
            archive_store: store,
            memory_store,
            probe_store_path: dir.path().join("runtime/recall-probes.db"),
            // Use a fake homeserver URL — auth will fail but we test the routes.
            homeserver_url: "http://localhost:0".to_string(),
            kb_root: None,
            swarm: None,
        }
    }

    /// Build a test state with a kb_root containing a pre-generated thread memory doc.
    fn test_state_with_thread_doc() -> (HttpApiState, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let kb_root = dir.path().to_path_buf();
        let store = Arc::new(FileArchiveStore::open(kb_root.join("archive")).expect("store"));
        let memory_store = Arc::new(SqliteMemoryStore::open_in_memory().expect("memory"));

        // Create a thread memory doc on disk.
        let threads_dir = kb_root.join("threads");
        std::fs::create_dir_all(&threads_dir).expect("create threads dir");
        let doc_content =
            "---\ntype: thread-memory\nthread_id: thread-test-project\ncontent_hash: abc123\n---\n\n# Test Project\n\n## Summary\n\nA test thread.\n";
        std::fs::write(threads_dir.join("test-project.md"), doc_content).expect("write doc");

        let state = HttpApiState {
            archive_store: store,
            memory_store,
            probe_store_path: dir.path().join("runtime/recall-probes.db"),
            homeserver_url: "http://localhost:0".to_string(),
            kb_root: Some(kb_root),
            swarm: None,
        };
        (state, dir)
    }

    #[tokio::test]
    async fn health_endpoint() {
        let state = test_state();
        let app = router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn entries_without_auth_returns_401() {
        let state = test_state();
        let app = router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/archive/entries")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn thread_memory_without_auth_returns_401() {
        let (state, _dir) = test_state_with_thread_doc();
        let app = router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/threads/thread-test-project/memory")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn thread_memory_hash_without_auth_returns_401() {
        let (state, _dir) = test_state_with_thread_doc();
        let app = router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/threads/thread-test-project/memory/hash")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn thread_memory_history_without_auth_returns_401() {
        let (state, _dir) = test_state_with_thread_doc();
        let app = router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/threads/thread-test-project/memory/history")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn thread_memory_diff_without_auth_returns_401() {
        let (state, _dir) = test_state_with_thread_doc();
        let app = router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/threads/thread-test-project/memory/diff?commit=abc1234")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn entity_profile_without_auth_returns_401() {
        let state = test_state();
        let app = router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/entities/profile?query=rust")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn entity_profile_hash_without_auth_returns_401() {
        let state = test_state();
        let app = router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/entities/profile/hash?query=rust")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn entity_history_without_auth_returns_401() {
        let state = test_state();
        let app = router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/entities/history?query=rust")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn entity_history_diff_without_auth_returns_401() {
        let state = test_state();
        let app = router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/entities/history/diff?query=rust&commit=abc1234")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn recall_probe_health_without_auth_returns_401() {
        let state = test_state();
        let app = router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/recall-probes/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn memory_integrity_without_auth_returns_401() {
        let state = test_state();
        let app = router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/memory-integrity/contradictions")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn recall_probe_summary_without_auth_returns_401() {
        let state = test_state();
        let app = router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/recall-probes/summary?target_kind=archive_entry&target_id=entry-1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn recall_probe_run_status_without_auth_returns_401() {
        let state = test_state();
        let app = router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/recall-probes/runs/run-1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn recall_probe_regressions_without_auth_returns_401() {
        let state = test_state();
        let app = router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/recall-probes/regressions?run_id=run-1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn build_recall_probe_health_response_includes_flags_and_failed_queries() {
        let store = RecallProbeStore::open_in_memory().expect("store");
        store
            .start_run(&symbiotic_memory::recall_probes::RecallProbeRun {
                id: "run-1".to_string(),
                started_at: 10,
                finished_at: None,
                cohort: Some("periodic_baseline".to_string()),
                top_k: 10,
                subject_count: 1,
                matched_count: 0,
            })
            .expect("start");
        store
            .record_result(&symbiotic_memory::recall_probes::RecallProbeResult {
                run_id: "run-1".to_string(),
                target_kind: RecallProbeTargetKind::ArchiveEntry,
                target_id: "entry-1".to_string(),
                query: "tokio runtime".to_string(),
                matched: false,
                rank: None,
                retrieval_mode: "hybrid".to_string(),
                top_item_ids: Vec::new(),
                remediation_flags: vec![
                    RecallRemediationFlag::ReembedCandidate,
                    RecallRemediationFlag::ManualReviewRequired,
                ],
                created_at: 11,
            })
            .expect("record");

        let response = build_recall_probe_health_response(&store, 10).expect("health response");
        assert_eq!(response.tracked_target_count, 1);
        assert_eq!(response.summaries.len(), 1);
        assert!(response.summaries[0]
            .remediation_flags
            .contains(&"reembed_candidate".to_string()));
        assert_eq!(response.summaries[0].failed_queries, vec!["tokio runtime"]);
    }

    #[test]
    fn build_recall_probe_summary_response_includes_latest_query_context() {
        let store = RecallProbeStore::open_in_memory().expect("store");
        store
            .start_run(&symbiotic_memory::recall_probes::RecallProbeRun {
                id: "run-1".to_string(),
                started_at: 10,
                finished_at: None,
                cohort: None,
                top_k: 10,
                subject_count: 1,
                matched_count: 0,
            })
            .expect("start");
        for (query, matched, created_at) in [("tokio", false, 11), ("runtime", true, 12)] {
            store
                .record_result(&symbiotic_memory::recall_probes::RecallProbeResult {
                    run_id: "run-1".to_string(),
                    target_kind: RecallProbeTargetKind::ArchiveEntry,
                    target_id: "entry-1".to_string(),
                    query: query.to_string(),
                    matched,
                    rank: matched.then_some(2),
                    retrieval_mode: "hybrid".to_string(),
                    top_item_ids: Vec::new(),
                    remediation_flags: if matched {
                        vec![RecallRemediationFlag::DerivedEdgeCandidate]
                    } else {
                        vec![RecallRemediationFlag::KeywordAugmentationCandidate]
                    },
                    created_at,
                })
                .expect("record");
        }

        let response = build_recall_probe_summary_response(
            &store,
            RecallProbeTargetKind::ArchiveEntry,
            "entry-1",
        )
        .expect("summary response")
        .expect("summary");
        assert_eq!(response.query_count, 2);
        assert_eq!(response.matched_query_count, 1);
        assert_eq!(response.failed_queries, vec!["tokio"]);
        assert!(response
            .remediation_flags
            .contains(&"derived_edge_candidate".to_string()));
    }

    #[tokio::test]
    async fn build_memory_integrity_response_includes_detected_contradictions() {
        let store = SqliteMemoryStore::open_in_memory().expect("memory");
        store.initialize().await.expect("schema");

        let entity = Entity {
            id: "vue".to_string(),
            entity_type: EntityType::Tool,
            name: "Vue".to_string(),
            attributes: json!({}),
            sensitivity: Sensitivity::Shareable,
            allowed_models: AllowedModels::Any,
            space: MemorySpace::Knowledge,
            status: EntityStatus::Active,
            merged_into: None,
            created_at: "2026-04-06T00:00:00Z".to_string(),
            updated_at: "2026-04-06T00:00:00Z".to_string(),
        };
        store.create_entity(&entity).await.expect("entity");

        for (id, fact, confidence, updated_at) in [
            (
                "mem-a",
                "Vue is no longer SSR-friendly.",
                0.61,
                "2026-04-05T00:00:00Z",
            ),
            (
                "mem-b",
                "Vue is SSR-friendly.",
                0.92,
                "2026-04-06T00:00:00Z",
            ),
        ] {
            store
                .create_memory(
                    &symbiotic_memory::types::Memory {
                        id: id.to_string(),
                        entity_id: entity.id.clone(),
                        fact: fact.to_string(),
                        confidence,
                        disposition: symbiotic_memory::types::FactDisposition::AutoStored,
                        sensitivity: Sensitivity::Shareable,
                        valid_from: "2026-04-06T00:00:00Z".to_string(),
                        valid_to: None,
                        status: symbiotic_memory::types::MemoryStatus::Active,
                        superseded_by: None,
                        created_at: "2026-04-06T00:00:00Z".to_string(),
                        updated_at: updated_at.to_string(),
                        fact_type: None,
                        authored_by: None,
                        supersedes: None,
                        depends_on: Vec::new(),
                        fsrs: None,
                    },
                    &[symbiotic_memory::types::Evidence {
                        id: format!("ev-{id}"),
                        memory_id: Some(id.to_string()),
                        relationship_id: None,
                        entity_id: None,
                        article_id: Some("article-1".to_string()),
                        source_url: Some("https://example.com".to_string()),
                        evidence_quote: Some("quote".to_string()),
                        observed_at: "2026-04-06T00:00:00Z".to_string(),
                        created_at: "2026-04-06T00:00:00Z".to_string(),
                    }],
                )
                .await
                .expect("memory");
        }

        let response = build_memory_integrity_response(&store, 10)
            .await
            .expect("integrity response");
        assert_eq!(response.tracked_entity_count, 1);
        assert_eq!(response.entities_with_contradictions, 1);
        assert_eq!(response.contradiction_count, 1);
        assert_eq!(response.review_count, 1);
        assert_eq!(response.contradictions.len(), 1);
        assert_eq!(response.contradictions[0].entity_name, "Vue");
        assert_eq!(response.contradictions[0].entity_type, "tool");
        assert!(response.contradictions[0].needs_review);
        assert_eq!(response.contradictions[0].resolution_confidence_percent, 68);
        assert_eq!(
            response.contradictions[0].preferred_memory_id.as_deref(),
            Some("mem-b")
        );
        assert_eq!(
            response.contradictions[0].preferred_fact.as_deref(),
            Some("Vue is SSR-friendly.")
        );
        assert!(response.contradictions[0]
            .preferred_reason
            .as_deref()
            .unwrap_or_default()
            .contains("higher confidence"));
        assert!(response.contradictions[0].suggestion.contains("Resolve"));
        assert!(response.contradictions[0]
            .investigation_summary
            .contains("Compared two active facts"));
        assert_eq!(response.contradictions[0].memory_a_evidence.len(), 1);
        assert_eq!(
            response.contradictions[0].memory_a_evidence[0].source_label,
            "example.com"
        );
    }

    #[test]
    fn build_recall_probe_run_status_response_includes_unmatched_targets() {
        let store = RecallProbeStore::open_in_memory().expect("store");
        store
            .start_run(&symbiotic_memory::recall_probes::RecallProbeRun {
                id: "run-1".to_string(),
                started_at: 10,
                finished_at: Some(20),
                cohort: Some("periodic_baseline".to_string()),
                top_k: 10,
                subject_count: 2,
                matched_count: 1,
            })
            .expect("start");
        for (target_id, matched, created_at) in [("entry-1", true, 11), ("entry-2", false, 12)] {
            store
                .record_result(&symbiotic_memory::recall_probes::RecallProbeResult {
                    run_id: "run-1".to_string(),
                    target_kind: RecallProbeTargetKind::ArchiveEntry,
                    target_id: target_id.to_string(),
                    query: format!("query-{target_id}"),
                    matched,
                    rank: matched.then_some(1),
                    retrieval_mode: "hybrid".to_string(),
                    top_item_ids: Vec::new(),
                    remediation_flags: Vec::new(),
                    created_at,
                })
                .expect("record");
        }

        let response = build_recall_probe_run_status_response(&store, "run-1")
            .expect("status response")
            .expect("status");
        assert_eq!(response.query_count, 2);
        assert_eq!(response.unmatched_subject_count, 1);
        assert_eq!(response.unmatched_target_ids, vec!["archive_entry:entry-2"]);
        assert_eq!(response.cohort.as_deref(), Some("periodic_baseline"));
    }

    #[test]
    fn build_recall_probe_regressions_response_tracks_deltas() {
        let store = RecallProbeStore::open_in_memory().expect("store");
        for run in [
            symbiotic_memory::recall_probes::RecallProbeRun {
                id: "run-1".to_string(),
                started_at: 10,
                finished_at: Some(20),
                cohort: Some("periodic_baseline".to_string()),
                top_k: 10,
                subject_count: 2,
                matched_count: 1,
            },
            symbiotic_memory::recall_probes::RecallProbeRun {
                id: "run-2".to_string(),
                started_at: 30,
                finished_at: Some(40),
                cohort: Some("periodic_baseline".to_string()),
                top_k: 10,
                subject_count: 2,
                matched_count: 1,
            },
        ] {
            store.start_run(&run).expect("start");
        }

        for result in [
            symbiotic_memory::recall_probes::RecallProbeResult {
                run_id: "run-1".to_string(),
                target_kind: RecallProbeTargetKind::ArchiveEntry,
                target_id: "entry-1".to_string(),
                query: "entry-1".to_string(),
                matched: true,
                rank: Some(1),
                retrieval_mode: "hybrid".to_string(),
                top_item_ids: Vec::new(),
                remediation_flags: Vec::new(),
                created_at: 11,
            },
            symbiotic_memory::recall_probes::RecallProbeResult {
                run_id: "run-1".to_string(),
                target_kind: RecallProbeTargetKind::ArchiveEntry,
                target_id: "entry-2".to_string(),
                query: "entry-2".to_string(),
                matched: false,
                rank: None,
                retrieval_mode: "hybrid".to_string(),
                top_item_ids: Vec::new(),
                remediation_flags: Vec::new(),
                created_at: 12,
            },
            symbiotic_memory::recall_probes::RecallProbeResult {
                run_id: "run-2".to_string(),
                target_kind: RecallProbeTargetKind::ArchiveEntry,
                target_id: "entry-1".to_string(),
                query: "entry-1".to_string(),
                matched: false,
                rank: None,
                retrieval_mode: "hybrid".to_string(),
                top_item_ids: Vec::new(),
                remediation_flags: Vec::new(),
                created_at: 31,
            },
            symbiotic_memory::recall_probes::RecallProbeResult {
                run_id: "run-2".to_string(),
                target_kind: RecallProbeTargetKind::ArchiveEntry,
                target_id: "entry-2".to_string(),
                query: "entry-2".to_string(),
                matched: true,
                rank: Some(2),
                retrieval_mode: "hybrid".to_string(),
                top_item_ids: Vec::new(),
                remediation_flags: Vec::new(),
                created_at: 32,
            },
        ] {
            store.record_result(&result).expect("record");
        }

        let response = build_recall_probe_regressions_response(&store, "run-2", 10)
            .expect("regressions response");
        let RecallProbeRegressionsLookup::Ready(response) = response else {
            panic!("expected ready regressions response");
        };
        assert_eq!(response.baseline_run_id, "run-1");
        assert_eq!(response.regression_count, 1);
        assert_eq!(response.improvement_count, 1);
        assert_eq!(response.regressions[0].target_id, "entry-1");
        assert_eq!(response.improvements[0].target_id, "entry-2");
    }

    #[tokio::test]
    async fn resolve_entity_query_matches_id_name_and_alias() {
        let store = SqliteMemoryStore::open_in_memory().expect("memory");
        store.initialize().await.expect("schema");
        let entity = Entity {
            id: "vue".to_string(),
            entity_type: EntityType::Tool,
            name: "Vue".to_string(),
            attributes: json!({"aliases": ["Vue.js", "VueJS"]}),
            sensitivity: Sensitivity::Shareable,
            allowed_models: AllowedModels::Any,
            space: MemorySpace::Knowledge,
            status: EntityStatus::Active,
            merged_into: None,
            created_at: "2026-04-06T00:00:00Z".to_string(),
            updated_at: "2026-04-06T00:00:00Z".to_string(),
        };
        store.create_entity(&entity).await.expect("entity");

        let by_id = resolve_entity_query(&store, "vue")
            .await
            .expect("lookup")
            .expect("entity");
        assert_eq!(by_id.id, "vue");

        let by_name = resolve_entity_query(&store, "Vue")
            .await
            .expect("lookup")
            .expect("entity");
        assert_eq!(by_name.id, "vue");

        let by_alias = resolve_entity_query(&store, "Vue.js")
            .await
            .expect("lookup")
            .expect("entity");
        assert_eq!(by_alias.id, "vue");
    }

    #[test]
    fn read_entity_profile_doc_reads_markdown_and_hash() {
        let dir = tempfile::tempdir().expect("tempdir");
        let kb_root = dir.path();
        let entities_dir = kb_root.join("ledger").join("tools").join("vue");
        std::fs::create_dir_all(&entities_dir).expect("entities dir");
        std::fs::write(
            entities_dir.join("vue.brief.md"),
            "# Vue\n\n## Facts\n\n- Fast.\n",
        )
        .expect("profile");

        let entity = Entity {
            id: "vue".to_string(),
            entity_type: EntityType::Tool,
            name: "Vue".to_string(),
            attributes: json!({}),
            sensitivity: Sensitivity::Shareable,
            allowed_models: AllowedModels::Any,
            space: MemorySpace::Knowledge,
            status: EntityStatus::Active,
            merged_into: None,
            created_at: "2026-04-06T00:00:00Z".to_string(),
            updated_at: "2026-04-06T00:00:00Z".to_string(),
        };

        let (hash, content) = read_entity_profile_doc(kb_root, &entity).expect("doc");
        assert_eq!(content, "# Vue\n\n## Facts\n\n- Fast.\n");
        assert_eq!(hash, content_hash(&content));
    }

    fn init_git_repo(repo_root: &std::path::Path) {
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(repo_root)
            .output()
            .expect("git init");
        std::process::Command::new("git")
            .args(["config", "user.email", "test@example.com"])
            .current_dir(repo_root)
            .output()
            .expect("git config email");
        std::process::Command::new("git")
            .args(["config", "user.name", "Symbiotic Test"])
            .current_dir(repo_root)
            .output()
            .expect("git config name");
    }

    #[test]
    fn read_entity_git_history_returns_file_commit_log() {
        let dir = tempfile::tempdir().expect("tempdir");
        init_git_repo(dir.path());
        let kb_root = dir.path().join("knowledge-base");
        let canonical = kb_root.join("ledger/tools/rust/rust.md");
        std::fs::create_dir_all(canonical.parent().unwrap()).expect("canonical dir");

        std::fs::write(&canonical, "# Rust\n\n## Facts\n\n- Fast.\n").expect("write v1");
        std::process::Command::new("git")
            .args(["add", "."])
            .current_dir(dir.path())
            .output()
            .expect("git add v1");
        std::process::Command::new("git")
            .args(["commit", "-m", "memory(rust): added 1 fact"])
            .current_dir(dir.path())
            .output()
            .expect("git commit v1");

        std::fs::write(&canonical, "# Rust\n\n## Facts\n\n- Fast.\n- Safe.\n").expect("write v2");
        std::process::Command::new("git")
            .args(["add", "."])
            .current_dir(dir.path())
            .output()
            .expect("git add v2");
        std::process::Command::new("git")
            .args([
                "commit",
                "-m",
                "memory(rust): added 1 fact, archived 0 facts",
            ])
            .current_dir(dir.path())
            .output()
            .expect("git commit v2");

        let history =
            read_entity_git_history(dir.path(), "knowledge-base/ledger/tools/rust/rust.md")
                .expect("history");
        assert_eq!(history.len(), 2);
        assert_eq!(
            history[0].summary,
            "memory(rust): added 1 fact, archived 0 facts"
        );
        assert_eq!(history[1].summary, "memory(rust): added 1 fact");
    }

    #[test]
    fn read_entity_git_diff_returns_patch_for_file_commit() {
        let dir = tempfile::tempdir().expect("tempdir");
        init_git_repo(dir.path());
        let kb_root = dir.path().join("knowledge-base");
        let canonical = kb_root.join("ledger/tools/rust/rust.md");
        std::fs::create_dir_all(canonical.parent().unwrap()).expect("canonical dir");

        std::fs::write(&canonical, "# Rust\n\n## Facts\n\n- Fast.\n").expect("write v1");
        std::process::Command::new("git")
            .args(["add", "."])
            .current_dir(dir.path())
            .output()
            .expect("git add");
        std::process::Command::new("git")
            .args(["commit", "-m", "memory(rust): added 1 fact"])
            .current_dir(dir.path())
            .output()
            .expect("git commit");

        let hash_output = std::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(dir.path())
            .output()
            .expect("rev-parse");
        let commit_hash = String::from_utf8_lossy(&hash_output.stdout)
            .trim()
            .to_string();

        let diff = read_entity_git_diff(
            dir.path(),
            "knowledge-base/ledger/tools/rust/rust.md",
            &commit_hash,
        )
        .expect("diff");
        assert!(diff.contains("memory(rust): added 1 fact"));
        assert!(diff.contains("+- Fast."));
    }

    #[test]
    fn read_thread_git_history_returns_file_commit_log() {
        let dir = tempfile::tempdir().expect("tempdir");
        init_git_repo(dir.path());
        let kb_root = dir.path().join("knowledge-base");
        let doc_path = kb_root.join("threads/test-project.md");
        std::fs::create_dir_all(doc_path.parent().unwrap()).expect("thread dir");

        std::fs::write(
            &doc_path,
            "# Test Project\n\n## Summary\n\nInitial summary.\n",
        )
        .expect("write v1");
        std::process::Command::new("git")
            .args(["add", "."])
            .current_dir(dir.path())
            .output()
            .expect("git add v1");
        std::process::Command::new("git")
            .args(["commit", "-m", "memory(thread-test-project): added summary"])
            .current_dir(dir.path())
            .output()
            .expect("git commit v1");

        std::fs::write(
            &doc_path,
            "# Test Project\n\n## Summary\n\nInitial summary.\n\n## Decisions\n\n- Use Rust.\n",
        )
        .expect("write v2");
        std::process::Command::new("git")
            .args(["add", "."])
            .current_dir(dir.path())
            .output()
            .expect("git add v2");
        std::process::Command::new("git")
            .args([
                "commit",
                "-m",
                "memory(thread-test-project): updated thread memory",
            ])
            .current_dir(dir.path())
            .output()
            .expect("git commit v2");

        let history = read_thread_git_history(dir.path(), "knowledge-base/threads/test-project.md")
            .expect("history");
        assert_eq!(history.len(), 2);
        assert_eq!(
            history[0].summary,
            "memory(thread-test-project): updated thread memory"
        );
        assert_eq!(
            history[1].summary,
            "memory(thread-test-project): added summary"
        );
    }

    #[test]
    fn read_thread_git_diff_returns_patch_for_file_commit() {
        let dir = tempfile::tempdir().expect("tempdir");
        init_git_repo(dir.path());
        let kb_root = dir.path().join("knowledge-base");
        let doc_path = kb_root.join("threads/test-project.md");
        std::fs::create_dir_all(doc_path.parent().unwrap()).expect("thread dir");

        std::fs::write(
            &doc_path,
            "# Test Project\n\n## Summary\n\nInitial summary.\n\n## Decisions\n\n- Use Rust.\n",
        )
        .expect("write v1");
        std::process::Command::new("git")
            .args(["add", "."])
            .current_dir(dir.path())
            .output()
            .expect("git add");
        std::process::Command::new("git")
            .args(["commit", "-m", "memory(thread-test-project): added summary"])
            .current_dir(dir.path())
            .output()
            .expect("git commit");

        let hash_output = std::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(dir.path())
            .output()
            .expect("rev-parse");
        let commit_hash = String::from_utf8_lossy(&hash_output.stdout)
            .trim()
            .to_string();

        let diff = read_thread_git_diff(
            dir.path(),
            "knowledge-base/threads/test-project.md",
            &commit_hash,
        )
        .expect("diff");
        assert!(diff.contains("memory(thread-test-project): added summary"));
        assert!(diff.contains("Use Rust."));
    }

    #[tokio::test]
    async fn thread_memory_404_when_no_kb_root() {
        // test_state() has kb_root = None
        let state = test_state();
        let app = router(state);
        // This will hit 401 first (no auth), but if we could bypass auth it would
        // return 404 due to no kb_root. The auth gate happens before the kb check.
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/threads/thread-nonexistent/memory")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        // Without auth, still 401
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn git_authorize_without_swarm_returns_503() {
        let state = test_state();
        let app = router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/git/authorize")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&PushAuthRequest {
                            repo_id: "repo-1".to_string(),
                            branch: "feature/x".to_string(),
                            old_sha: "0".repeat(40),
                            new_sha: "1".repeat(40),
                            push_session: "push-session".to_string(),
                        })
                        .expect("serialize request"),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    /// Test that read_thread_memory_doc returns content and hash for existing docs.
    #[test]
    fn read_thread_memory_doc_existing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let kb_root = dir.path();
        let threads_dir = kb_root.join("threads");
        std::fs::create_dir_all(&threads_dir).expect("create threads dir");

        let doc = "---\ntype: thread-memory\nthread_id: thread-abc\n---\n\n# ABC\n";
        std::fs::write(threads_dir.join("abc.md"), doc).expect("write doc");

        let (content, hash) = read_thread_memory_doc(kb_root, "thread-abc").unwrap();
        assert_eq!(content, doc);
        assert!(!hash.is_empty());
        assert_eq!(hash.len(), 64); // SHA-256 hex

        // Hash should be deterministic.
        let (_, hash2) = read_thread_memory_doc(kb_root, "thread-abc").unwrap();
        assert_eq!(hash, hash2);
    }

    /// Test that read_thread_memory_doc returns 404 for non-existent threads.
    #[test]
    fn read_thread_memory_doc_not_found() {
        let dir = tempfile::tempdir().expect("tempdir");
        let result = read_thread_memory_doc(dir.path(), "thread-nonexistent");
        assert!(result.is_err());
        let (status, _msg) = result.unwrap_err();
        assert_eq!(status, StatusCode::NOT_FOUND);
    }
}
