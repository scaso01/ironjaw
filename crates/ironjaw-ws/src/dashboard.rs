//! Dashboard module: static file serving + REST API endpoints.
//!
//! Static file endpoints serve the embedded dashboard UI.
//! REST API endpoints (`/api/dashboard/*`) provide JSON data wired to live AppState.
//! Command Center endpoints (`/api/command/*`) provide unified session aggregation.

use crate::state::AppState;
use axum::{
    extract::{Path, Query, State},
    http::{header, HeaderValue, StatusCode},
    response::{IntoResponse, Json, Redirect, Response},
    routing::{get, post},
    Router,
};
use rust_embed::Embed;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::atomic::Ordering;

#[derive(Embed)]
#[folder = "static/"]
struct StaticAssets;

/// Serve the dashboard root -- redirect to index.html
pub async fn dashboard_root() -> Redirect {
    Redirect::temporary("/dashboard/index.html")
}

/// Serve embedded static files from the `static/` directory
pub async fn dashboard_file(Path(path): Path<String>) -> Response {
    // Defense in depth: reject path traversal attempts
    if path.contains("..") {
        return StatusCode::BAD_REQUEST.into_response();
    }

    // Serve index.html for empty path (trailing-slash requests)
    let path = if path.is_empty() {
        "index.html".to_string()
    } else {
        path
    };

    match StaticAssets::get(&path) {
        Some(file) => {
            let mime = mime_guess::from_path(&path).first_or_octet_stream();
            let is_html = mime.subtype().as_str() == "html";
            let cache_control = if is_html {
                "no-cache"
            } else {
                "public, max-age=3600"
            };
            (
                StatusCode::OK,
                [
                    (
                        header::CONTENT_TYPE,
                        HeaderValue::from_str(mime.as_ref()).unwrap_or_else(|_| {
                            HeaderValue::from_static("application/octet-stream")
                        }),
                    ),
                    (
                        header::HeaderName::from_static("x-content-type-options"),
                        HeaderValue::from_static("nosniff"),
                    ),
                    (
                        header::HeaderName::from_static("x-frame-options"),
                        HeaderValue::from_static("DENY"),
                    ),
                    (
                        header::CACHE_CONTROL,
                        HeaderValue::from_static(cache_control),
                    ),
                ],
                file.data,
            )
                .into_response()
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

// ---------------------------------------------------------------------------
// Dashboard REST API -- Data Types
// ---------------------------------------------------------------------------

/// High-level overview of the gateway state.
#[derive(Debug, Serialize, Deserialize)]
pub struct DashboardOverview {
    pub uptime_secs: u64,
    pub active_sessions: u64,
    pub total_messages: u64,
    pub connected_devices: u64,
    pub memory_count: u64,
    pub active_tools: usize,
    pub llm_provider_count: usize,
    pub version: String,
    pub llm_status: String,
}

/// Summary of a single chat session.
#[derive(Debug, Serialize, Deserialize)]
pub struct SessionSummary {
    pub id: String,
    pub name: Option<String>,
    pub message_count: u64,
    pub created_at: String,
    pub updated_at: String,
}

/// Summary of a registered tool.
#[derive(Debug, Serialize, Deserialize)]
pub struct ToolSummary {
    pub name: String,
    pub description: String,
    pub tier: String,
    pub category: String,
}

/// Point-in-time metrics snapshot.
#[derive(Debug, Serialize, Deserialize)]
pub struct MetricsSnapshot {
    pub requests_total: u64,
    pub errors_total: u64,
    pub active_connections: u64,
    pub llm_tokens_used: u64,
    pub avg_response_ms: f64,
    pub uptime_secs: u64,
}

/// Summary of the RAG brain subsystem.
#[derive(Debug, Serialize, Deserialize)]
pub struct BrainSummary {
    pub total_memories: u64,
    pub memory_types: HashMap<String, u64>,
    pub total_entities: u64,
    pub total_relations: u64,
    pub last_memory_at: Option<String>,
}

/// Enhanced brain status with counts, breakdowns, and recent activity.
#[derive(Debug, Serialize, Deserialize)]
pub struct BrainStatus {
    pub total_memories: i64,
    pub embedded_count: i64,
    pub unembedded_count: i64,
    pub by_source_type: HashMap<String, i64>,
    pub by_memory_type: HashMap<String, i64>,
    pub graph: GraphCounts,
    pub indexers: IndexerStatus,
    pub recent_activity: RecentActivity,
}

/// Entity and relation counts from the knowledge graph.
#[derive(Debug, Serialize, Deserialize)]
pub struct GraphCounts {
    pub entities: i64,
    pub relations: i64,
}

/// Sync status for each indexer spoke.
#[derive(Debug, Serialize, Deserialize)]
pub struct IndexerStatus {
    pub chronicle: ChronicleIndexerInfo,
    pub markdown: MarkdownIndexerInfo,
}

/// Chronicle indexer state.
#[derive(Debug, Serialize, Deserialize)]
pub struct ChronicleIndexerInfo {
    pub enabled: bool,
    pub last_sync: Option<String>,
    pub synced_count: i64,
}

/// Markdown indexer state.
#[derive(Debug, Serialize, Deserialize)]
pub struct MarkdownIndexerInfo {
    pub enabled: bool,
    pub watched_files: i64,
    pub indexed_chunks: i64,
}

/// Recent memory creation activity.
#[derive(Debug, Serialize, Deserialize)]
pub struct RecentActivity {
    pub last_24h: i64,
    pub last_7d: i64,
}

/// Paginated browse response.
#[derive(Debug, Serialize, Deserialize)]
pub struct BrowseResponse {
    pub memories: Vec<BrowseMemory>,
    pub total: i64,
    pub page: u32,
    pub per_page: u32,
}

/// Single memory in a browse listing.
#[derive(Debug, Serialize, Deserialize)]
pub struct BrowseMemory {
    pub id: i64,
    pub content: String,
    pub memory_type: String,
    pub source_type: String,
    pub importance: f64,
    pub created_at: String,
    pub accessed_at: String,
    pub access_count: i64,
}

/// Operational brain metrics.
#[derive(Debug, Serialize, Deserialize)]
pub struct BrainMetrics {
    pub embedding_backend: String,
    pub vec_table_count: i64,
    pub source_health: SourceHealth,
    pub consolidation: ConsolidationInfo,
}

/// Source health tracking.
#[derive(Debug, Serialize, Deserialize)]
pub struct SourceHealth {
    pub chronicle: SourceSyncInfo,
    pub markdown: SourceSyncInfo,
}

/// Sync info for a single source.
#[derive(Debug, Serialize, Deserialize)]
pub struct SourceSyncInfo {
    pub last_sync_at: Option<String>,
    pub items_synced: i64,
}

/// Consolidation scheduling info.
#[derive(Debug, Serialize, Deserialize)]
pub struct ConsolidationInfo {
    pub last_run: Option<String>,
    pub next_run_hours: Option<f64>,
}

/// Summary of a connected device.
#[derive(Debug, Serialize, Deserialize)]
pub struct DeviceSummary {
    pub id: String,
    pub name: Option<String>,
    pub connected: bool,
    pub last_seen: String,
}

/// Health check result for the dashboard API.
#[derive(Debug, Serialize, Deserialize)]
pub struct HealthCheck {
    pub status: String,
    pub checks: HealthChecks,
    pub uptime_secs: u64,
}

/// Individual subsystem health flags.
#[derive(Debug, Serialize, Deserialize)]
pub struct HealthChecks {
    pub database: bool,
    pub llm: bool,
    pub brain: bool,
    pub tools: bool,
}

// ---------------------------------------------------------------------------
// Command Center -- Data Types
// ---------------------------------------------------------------------------

/// Unified session spanning Ironjaw native and Claude bridged sessions.
#[derive(Debug, Serialize, Deserialize)]
pub struct UnifiedSession {
    pub id: String,
    pub source: String,
    pub title: Option<String>,
    pub project: Option<String>,
    pub model: Option<String>,
    pub role: Option<String>,
    pub status: String,
    pub total_tokens: u64,
    pub cost_usd: f64,
    pub context_pct: Option<f32>,
}

/// Aggregated usage summary.
#[derive(Debug, Serialize, Deserialize)]
pub struct UsageSummaryResponse {
    pub total_tokens: u64,
    pub total_cost_usd: f64,
    pub request_count: u64,
    pub by_model: Vec<ModelUsage>,
}

/// Per-model usage breakdown.
#[derive(Debug, Serialize, Deserialize)]
pub struct ModelUsage {
    pub model: String,
    pub tokens: u64,
    pub cost_usd: f64,
    pub requests: u64,
}

/// Action to perform on a session.
#[derive(Debug, Deserialize)]
pub struct SessionAction {
    pub action: String,
    #[serde(default)]
    pub params: Option<serde_json::Value>,
}

// ---------------------------------------------------------------------------
// Dashboard REST API -- Handlers (wired to AppState)
// ---------------------------------------------------------------------------

/// GET /api/dashboard/overview -- gateway overview from live state.
async fn get_overview(State(state): State<AppState>) -> Json<DashboardOverview> {
    let session_count = state.session.store.count_sessions().await.unwrap_or(0) as u64;
    let tool_count = state.tools.count();
    let memory_count = state
        .brain
        .engine
        .store()
        .count_memories()
        .await
        .unwrap_or(0) as u64;
    let device_count = state.device.registry.count().await as u64;
    let uptime = chrono::Utc::now()
        .signed_duration_since(state.started_at)
        .num_seconds() as u64;

    let llama = state.llm.llama_state.read().await;
    let llm_status = format!("{:?}", llama.status);

    Json(DashboardOverview {
        uptime_secs: uptime,
        active_sessions: session_count,
        total_messages: 0,
        connected_devices: device_count,
        memory_count,
        active_tools: tool_count,
        llm_provider_count: 1,
        version: env!("CARGO_PKG_VERSION").to_string(),
        llm_status,
    })
}

/// GET /api/dashboard/sessions -- list session summaries from SessionStore.
async fn get_sessions(State(state): State<AppState>) -> Json<Vec<SessionSummary>> {
    let sessions = state
        .session
        .store
        .list_sessions(50, 0)
        .await
        .unwrap_or_default();
    let summaries = sessions
        .into_iter()
        .map(|s| SessionSummary {
            id: s.id,
            name: s.title,
            message_count: s.message_count.unwrap_or(0) as u64,
            created_at: s.created_at,
            updated_at: s.updated_at,
        })
        .collect();
    Json(summaries)
}

/// GET /api/dashboard/tools -- list registered tools from ToolRegistry.
async fn get_tools(State(state): State<AppState>) -> Json<Vec<ToolSummary>> {
    let tools = state.tools.list();
    let summaries = tools
        .into_iter()
        .map(|t| {
            let tier_name = match t.tier {
                0 => "Auto",
                1 => "Notify",
                2 => "Confirm",
                3 => "MultiConfirm",
                _ => "Unknown",
            };
            ToolSummary {
                name: t.name,
                description: t.description,
                tier: tier_name.to_string(),
                category: "general".to_string(),
            }
        })
        .collect();
    Json(summaries)
}

/// GET /api/dashboard/metrics -- point-in-time metrics from Metrics atomics.
async fn get_metrics(State(state): State<AppState>) -> Json<MetricsSnapshot> {
    let uptime = chrono::Utc::now()
        .signed_duration_since(state.started_at)
        .num_seconds() as u64;

    Json(MetricsSnapshot {
        requests_total: state
            .observability
            .metrics
            .requests_total
            .load(Ordering::Relaxed),
        errors_total: state
            .observability
            .metrics
            .errors_total
            .load(Ordering::Relaxed),
        active_connections: state
            .observability
            .metrics
            .active_connections
            .load(Ordering::Relaxed),
        llm_tokens_used: state
            .observability
            .metrics
            .llm_calls_total
            .load(Ordering::Relaxed),
        avg_response_ms: 0.0,
        uptime_secs: uptime,
    })
}

/// GET /api/dashboard/brain -- RAG brain summary from BrainStore.
async fn get_brain(State(state): State<AppState>) -> Json<BrainSummary> {
    let count = state
        .brain
        .engine
        .store()
        .count_memories()
        .await
        .unwrap_or(0) as u64;

    let mut memory_types = HashMap::new();
    memory_types.insert("identity".to_string(), 0);
    memory_types.insert("long_term".to_string(), count);
    memory_types.insert("working".to_string(), 0);

    Json(BrainSummary {
        total_memories: count,
        memory_types,
        total_entities: 0,
        total_relations: 0,
        last_memory_at: None,
    })
}

/// GET /api/dashboard/devices -- list connected devices from DeviceRegistry.
async fn get_devices(State(state): State<AppState>) -> Json<Vec<DeviceSummary>> {
    let devices = state.device.registry.list().await;
    let summaries = devices
        .into_iter()
        .map(|d| DeviceSummary {
            id: d.id,
            name: Some(d.name),
            connected: true, // all listed devices are connected
            last_seen: d.last_seen.to_rfc3339(),
        })
        .collect();
    Json(summaries)
}

/// GET /api/dashboard/health -- dashboard-specific health check from live state.
async fn get_health(State(state): State<AppState>) -> Json<HealthCheck> {
    let llama = state.llm.llama_state.read().await;
    let llm_ok = matches!(llama.status, ironjaw_llm::LlamaHealth::Healthy);
    let uptime = chrono::Utc::now()
        .signed_duration_since(state.started_at)
        .num_seconds() as u64;

    Json(HealthCheck {
        status: "ok".into(),
        checks: HealthChecks {
            database: true,
            llm: llm_ok,
            brain: true,
            tools: true,
        },
        uptime_secs: uptime,
    })
}

// ---------------------------------------------------------------------------
// Command Center -- Handlers
// ---------------------------------------------------------------------------

/// GET /api/command/sessions -- unified session list (Ironjaw + Claude bridged).
async fn command_sessions(State(state): State<AppState>) -> Json<Vec<UnifiedSession>> {
    let mut unified = Vec::new();

    // 1. Native Ironjaw sessions
    let sessions = state
        .session
        .store
        .list_sessions(100, 0)
        .await
        .unwrap_or_default();
    for s in sessions {
        unified.push(UnifiedSession {
            id: s.id,
            source: "ironjaw".into(),
            title: s.title,
            project: None,
            model: None,
            role: None,
            status: "active".into(),
            total_tokens: s.context_tokens as u64,
            cost_usd: 0.0,
            context_pct: None,
        });
    }

    // 2. Active CC subprocess sessions
    let active = state.claude.tracker.list_active().await;
    for a in active {
        unified.push(UnifiedSession {
            id: a.session_id,
            source: "claude".into(),
            title: None,
            project: None,
            model: None,
            role: None,
            status: "running".into(),
            total_tokens: 0,
            cost_usd: 0.0,
            context_pct: None,
        });
    }

    Json(unified)
}

/// GET /api/command/usage -- aggregated usage summary.
async fn command_usage(State(_state): State<AppState>) -> Json<UsageSummaryResponse> {
    // Stub: usage tracking aggregation would need a dedicated query
    Json(UsageSummaryResponse {
        total_tokens: 0,
        total_cost_usd: 0.0,
        request_count: 0,
        by_model: vec![],
    })
}

/// POST /api/command/sessions/:id/action -- perform action on a session.
async fn command_session_action(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    Json(action): Json<SessionAction>,
) -> Json<serde_json::Value> {
    match action.action.as_str() {
        "abort" | "destroy" | "interrupt" => {
            let result = state.claude.tracker.abort(&session_id).await;
            match result {
                Ok(_) => Json(serde_json::json!({"ok": true, "action": action.action})),
                Err(e) => Json(serde_json::json!({"ok": false, "error": e.to_string()})),
            }
        }
        other => Json(serde_json::json!({
            "ok": false,
            "error": format!("unknown action: {other}"),
        })),
    }
}

// ---------------------------------------------------------------------------
// Brain REST API
// ---------------------------------------------------------------------------

/// POST /api/brain/remember -- store a memory via REST (for hooks and external callers).
///
/// Body: `{ "content": "...", "memory_type": "fact", "importance": 0.7, "source": "...", "tags": "..." }`
async fn handle_brain_remember(
    State(state): State<AppState>,
    Json(body): Json<serde_json::Value>,
) -> Json<serde_json::Value> {
    let content = body["content"].as_str().unwrap_or("");
    if content.is_empty() {
        return Json(serde_json::json!({"ok": false, "error": "content required"}));
    }
    let source = body.get("source").and_then(|v| v.as_str());
    let tags = body.get("tags").and_then(|v| v.as_str());

    // Parse optional metadata fields from dashboard request
    let memory_type_str = body.get("memory_type").and_then(|v| v.as_str());
    let confidence = body.get("confidence").and_then(|v| v.as_f64());
    let trust_level_raw = body.get("trust_level").and_then(|v| v.as_i64());
    let importance_override = body.get("importance").and_then(|v| v.as_f64());
    let evidence_session_id = body
        .get("evidence_session_id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let evidence_message_id = body
        .get("evidence_message_id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let scope = body
        .get("scope")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let supersedes = body.get("supersedes").and_then(|v| v.as_i64());

    let importance = importance_override.or_else(|| {
        confidence.or_else(|| {
            memory_type_str.and_then(|mt| {
                use ironjaw_brain::types::MemoryType;
                mt.parse::<MemoryType>().ok().map(|t| t.base_importance())
            })
        })
    });

    let trust_level = trust_level_raw.map(|v| ironjaw_brain::types::TrustLevel::from(v as i32));

    let mut new_mem = ironjaw_brain::types::NewMemory {
        content: content.to_string(),
        source: source.map(|s| s.to_string()),
        tags: tags.map(|t| t.to_string()),
        embedding: None,
        memory_type: memory_type_str.map(|s| s.to_string()),
        importance,
        strength: None,
        supersedes,
        content_hash: None,
        valid_from: None,
        valid_to: None,
        source_type: None,
        reflection: None,
        trust_level,
        evidence_session_id,
        evidence_message_id,
        scope,
        procedure: None,
        quality_score: None,
        quality_flag: None,
    };

    match state.brain.engine.remember_full(&mut new_mem).await {
        Ok((memory, _embedded)) => Json(serde_json::json!({"ok": true, "id": memory.id})),
        Err(e) => Json(serde_json::json!({"ok": false, "error": e.to_string()})),
    }
}

/// GET /api/brain/pending?limit=N&offset=N -- list pending memories awaiting approval.
async fn handle_brain_pending_list(
    State(state): State<AppState>,
    Query(params): Query<PendingListParams>,
) -> Response {
    let limit = params.limit.unwrap_or(50).min(200) as i64;
    let offset = params.offset.unwrap_or(0) as i64;
    match state
        .brain
        .engine
        .store()
        .list_pending_memories(limit, offset)
        .await
    {
        Ok(items) => {
            let count = items.len();
            Json(serde_json::json!({"items": items, "count": count})).into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"ok": false, "error": e.to_string()})),
        )
            .into_response(),
    }
}

/// POST /api/brain/pending/:id/approve -- approve a pending memory, moving it to memories.
async fn handle_brain_pending_approve(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Response {
    match state.brain.engine.store().approve_pending_memory(id).await {
        Ok(memory_id) => {
            Json(serde_json::json!({"ok": true, "memory_id": memory_id})).into_response()
        }
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"ok": false, "error": e.to_string()})),
        )
            .into_response(),
    }
}

/// POST /api/brain/pending/:id/reject -- reject a pending memory.
async fn handle_brain_pending_reject(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Response {
    match state.brain.engine.store().reject_pending_memory(id).await {
        Ok(()) => Json(serde_json::json!({"ok": true})).into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"ok": false, "error": e.to_string()})),
        )
            .into_response(),
    }
}

/// POST /api/brain/reembed -- re-embed all memories with NULL embeddings.
async fn handle_brain_reembed(State(state): State<AppState>) -> Json<serde_json::Value> {
    match state.brain.engine.reembed_missing().await {
        Ok((processed, failed)) => {
            Json(serde_json::json!({"ok": true, "processed": processed, "failed": failed}))
        }
        Err(e) => Json(serde_json::json!({"ok": false, "error": e.to_string()})),
    }
}

// ---------------------------------------------------------------------------
// Brain Observability API -- Handlers
// ---------------------------------------------------------------------------

/// GET /api/brain/status -- enhanced brain status with breakdowns and activity.
async fn handle_brain_status(State(state): State<AppState>) -> Response {
    let pool = state.brain.engine.store().pool();

    // Run all count queries in parallel
    let (
        total_res,
        embedded_res,
        by_source_res,
        by_type_res,
        entity_res,
        relation_res,
        last_24h_res,
        last_7d_res,
        chronicle_sync_res,
        chronicle_count_res,
        md_files_res,
        md_chunks_res,
    ) = tokio::join!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM memories WHERE deleted = 0")
            .fetch_one(pool),
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM memories WHERE deleted = 0 AND embedding IS NOT NULL"
        )
        .fetch_one(pool),
        sqlx::query_as::<_, (String, i64)>(
            "SELECT COALESCE(source_type, 'brain') as st, COUNT(*) as cnt \
             FROM memories WHERE deleted = 0 GROUP BY st"
        )
        .fetch_all(pool),
        sqlx::query_as::<_, (String, i64)>(
            "SELECT memory_type, COUNT(*) as cnt \
             FROM memories WHERE deleted = 0 GROUP BY memory_type"
        )
        .fetch_all(pool),
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM entities").fetch_one(pool),
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM relations").fetch_one(pool),
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM memories WHERE deleted = 0 \
             AND created_at >= strftime('%Y-%m-%dT%H:%M:%f', 'now', '-1 day')"
        )
        .fetch_one(pool),
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM memories WHERE deleted = 0 \
             AND created_at >= strftime('%Y-%m-%dT%H:%M:%f', 'now', '-7 days')"
        )
        .fetch_one(pool),
        sqlx::query_scalar::<_, String>(
            "SELECT last_synced_at FROM sync_state WHERE source = 'chronicle'"
        )
        .fetch_optional(pool),
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM memories WHERE deleted = 0 \
             AND source_type = 'cc_chronicle'"
        )
        .fetch_one(pool),
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM markdown_index").fetch_one(pool),
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM memories WHERE deleted = 0 \
             AND source_type = 'markdown'"
        )
        .fetch_one(pool),
    );

    let total = total_res.unwrap_or(0);
    let embedded = embedded_res.unwrap_or(0);

    let by_source_type: HashMap<String, i64> =
        by_source_res.unwrap_or_default().into_iter().collect();

    let by_memory_type: HashMap<String, i64> =
        by_type_res.unwrap_or_default().into_iter().collect();

    let chronicle_query_ok = chronicle_sync_res.is_ok();
    let chronicle_last_sync = chronicle_sync_res.ok().flatten();
    let chronicle_count = chronicle_count_res.unwrap_or(0);
    let md_files_count = md_files_res.unwrap_or(0);
    let md_chunks_count = md_chunks_res.unwrap_or(0);

    let status = BrainStatus {
        total_memories: total,
        embedded_count: embedded,
        unembedded_count: total - embedded,
        by_source_type,
        by_memory_type,
        graph: GraphCounts {
            entities: entity_res.unwrap_or(0),
            relations: relation_res.unwrap_or(0),
        },
        indexers: IndexerStatus {
            chronicle: ChronicleIndexerInfo {
                enabled: chronicle_last_sync.is_some() || chronicle_count > 0 || chronicle_query_ok,
                last_sync: chronicle_last_sync,
                synced_count: chronicle_count,
            },
            markdown: MarkdownIndexerInfo {
                enabled: md_files_count > 0 || md_chunks_count > 0,
                watched_files: md_files_count,
                indexed_chunks: md_chunks_count,
            },
        },
        recent_activity: RecentActivity {
            last_24h: last_24h_res.unwrap_or(0),
            last_7d: last_7d_res.unwrap_or(0),
        },
    };

    Json(status).into_response()
}

/// GET /api/brain/browse?page=1&per_page=20&source={type}&type={memory_type}&sort={recent|importance}
///
/// Paginated memory browsing with filters.
async fn handle_brain_browse(
    State(state): State<AppState>,
    Query(params): Query<BrowseParams>,
) -> Response {
    let page = params.page.unwrap_or(1).max(1);
    let per_page = params.per_page.unwrap_or(20).clamp(1, 100);
    let offset = (page - 1) * per_page;

    let sort_clause = match params.sort.as_deref() {
        Some("importance") => "ORDER BY importance DESC, created_at DESC",
        _ => "ORDER BY created_at DESC",
    };

    // Build WHERE clause with parameterized queries to prevent SQL injection.
    // Optional filters are handled with (? IS NULL OR column = ?) pattern.
    let pool = state.brain.engine.store().pool();

    let count_sql = "SELECT COUNT(*) FROM memories WHERE deleted = 0 \
         AND (? IS NULL OR source_type = ?) \
         AND (? IS NULL OR memory_type = ?) \
         AND (? IS NULL OR scope LIKE ? || '%') \
         AND (? IS NULL OR trust_level >= ?)"
        .to_string();
    let source_val = params.source.as_deref();
    let mtype_val = params.memory_type.as_deref();
    let scope_val = params.scope_prefix.as_deref();
    let trust_val = params.trust_level_min;

    let total = sqlx::query_scalar::<_, i64>(&count_sql)
        .bind(source_val)
        .bind(source_val)
        .bind(mtype_val)
        .bind(mtype_val)
        .bind(scope_val)
        .bind(scope_val)
        .bind(trust_val)
        .bind(trust_val)
        .fetch_one(pool)
        .await
        .unwrap_or(0);

    // Fetch page — sort_clause is a compile-time constant, not user input
    let select_sql = format!(
        "SELECT id, content, memory_type, COALESCE(source_type, 'brain') as source_type, \
         importance, created_at, accessed_at, access_count \
         FROM memories WHERE deleted = 0 \
         AND (? IS NULL OR source_type = ?) \
         AND (? IS NULL OR memory_type = ?) \
         AND (? IS NULL OR scope LIKE ? || '%') \
         AND (? IS NULL OR trust_level >= ?) \
         {sort_clause} LIMIT ? OFFSET ?"
    );

    let rows =
        sqlx::query_as::<_, (i64, String, String, String, f64, String, String, i64)>(&select_sql)
            .bind(source_val)
            .bind(source_val)
            .bind(mtype_val)
            .bind(mtype_val)
            .bind(scope_val)
            .bind(scope_val)
            .bind(trust_val)
            .bind(trust_val)
            .bind(per_page as i64)
            .bind(offset as i64)
            .fetch_all(pool)
            .await
            .unwrap_or_default();

    let memories: Vec<BrowseMemory> = rows
        .into_iter()
        .map(
            |(
                id,
                content,
                memory_type,
                source_type,
                importance,
                created_at,
                accessed_at,
                access_count,
            )| {
                BrowseMemory {
                    id,
                    content,
                    memory_type,
                    source_type,
                    importance,
                    created_at,
                    accessed_at,
                    access_count,
                }
            },
        )
        .collect();

    Json(BrowseResponse {
        memories,
        total,
        page,
        per_page,
    })
    .into_response()
}

/// GET /api/brain/metrics -- operational brain metrics.
async fn handle_brain_metrics(State(state): State<AppState>) -> Response {
    let pool = state.brain.engine.store().pool();
    let config = state.brain.engine.config();

    // Run queries in parallel
    let (
        vec_count_res,
        chronicle_sync_at_res,
        chronicle_items_res,
        md_sync_at_res,
        md_files_res,
        last_consolidation_res,
    ) = tokio::join!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM vec_memories").fetch_one(pool),
        sqlx::query_scalar::<_, String>(
            "SELECT last_synced_at FROM sync_state WHERE source = 'chronicle'"
        )
        .fetch_optional(pool),
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM memories WHERE deleted = 0 \
             AND source_type = 'cc_chronicle'"
        )
        .fetch_one(pool),
        sqlx::query_scalar::<_, String>("SELECT MAX(last_indexed_at) FROM markdown_index")
            .fetch_optional(pool),
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM markdown_index").fetch_one(pool),
        sqlx::query_scalar::<_, String>(
            "SELECT last_synced_at FROM sync_state WHERE source = 'consolidation'"
        )
        .fetch_optional(pool),
    );

    let chronicle_last_sync = chronicle_sync_at_res.ok().flatten();
    let md_last_index = md_sync_at_res.ok().flatten();
    let last_consolidation = last_consolidation_res.ok().flatten();

    // Calculate next consolidation run estimate
    let interval_hours = config.consolidation_interval_hours as f64;
    let next_run_hours = if let Some(ref last_run) = last_consolidation {
        // Parse the timestamp and compute hours until next run
        chrono::NaiveDateTime::parse_from_str(last_run, "%Y-%m-%dT%H:%M:%S%.f")
            .ok()
            .map(|last| {
                let last_utc = last.and_utc();
                let now = chrono::Utc::now();
                let elapsed_hours = now.signed_duration_since(last_utc).num_minutes() as f64 / 60.0;
                let remaining = interval_hours - elapsed_hours;
                if remaining > 0.0 {
                    remaining
                } else {
                    0.0
                }
            })
    } else {
        None
    };

    let metrics = BrainMetrics {
        embedding_backend: "llama-server".to_string(),
        vec_table_count: vec_count_res.unwrap_or(0),
        source_health: SourceHealth {
            chronicle: SourceSyncInfo {
                last_sync_at: chronicle_last_sync,
                items_synced: chronicle_items_res.unwrap_or(0),
            },
            markdown: SourceSyncInfo {
                last_sync_at: md_last_index,
                items_synced: md_files_res.unwrap_or(0),
            },
        },
        consolidation: ConsolidationInfo {
            last_run: last_consolidation,
            next_run_hours,
        },
    };

    Json(metrics).into_response()
}

/// GET /api/brain/health -- comprehensive brain health metrics (cached 60s).
async fn handle_brain_health_rest(State(state): State<AppState>) -> Response {
    match state.brain.engine.store().health_metrics().await {
        Ok(metrics) => Json(metrics).into_response(),
        Err(e) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

// ---------------------------------------------------------------------------
// Brain Query API -- Data Types
// ---------------------------------------------------------------------------

/// Query params for GET /api/brain/recall
#[derive(Debug, Deserialize)]
struct RecallParams {
    #[serde(default)]
    q: String,
    query: Option<String>,
    limit: Option<usize>,
    detail: Option<String>,
    tag: Option<String>,
}

/// Query params for GET /api/brain/search
#[derive(Debug, Deserialize)]
struct SearchParams {
    q: String,
    source: Option<String>,
    since: Option<String>,
    limit: Option<usize>,
}

/// Query params for GET /api/brain/timeline
#[derive(Debug, Deserialize)]
struct TimelineParams {
    at: String,
    limit: Option<usize>,
}

/// Query params for GET /api/brain/summaries
#[derive(Debug, Deserialize)]
struct SummariesParams {
    session_id: Option<String>,
    limit: Option<usize>,
}

/// Query params for GET /api/brain/context
#[derive(Debug, Deserialize)]
struct ContextParams {
    project: Option<String>,
}

/// Query params for GET /api/brain/browse
#[derive(Debug, Deserialize)]
struct BrowseParams {
    page: Option<u32>,
    per_page: Option<u32>,
    source: Option<String>,
    #[serde(rename = "type")]
    memory_type: Option<String>,
    sort: Option<String>,
    scope_prefix: Option<String>,
    trust_level_min: Option<i32>,
}

/// Query params for GET /api/brain/pending
#[derive(Debug, Deserialize)]
struct PendingListParams {
    limit: Option<u32>,
    offset: Option<u32>,
}

/// Compact memory representation for API responses.
#[derive(Debug, Serialize)]
struct MemoryCompact {
    id: i64,
    content: String,
    memory_type: String,
    importance: f64,
    trust_level: i32,
    created_at: String,
}

/// Full memory representation for API responses.
#[derive(Debug, Serialize)]
struct MemoryFull {
    id: i64,
    content: String,
    memory_type: String,
    importance: f64,
    source: Option<String>,
    source_type: String,
    tags: Option<String>,
    strength: f64,
    created_at: String,
    accessed_at: String,
    access_count: i64,
    valid_from: Option<String>,
    valid_to: Option<String>,
}

/// Recall/search response envelope.
#[derive(Debug, Serialize)]
struct MemoryResponse {
    memories: serde_json::Value,
    count: usize,
}

// ---------------------------------------------------------------------------
// Brain Query API -- Handlers
// ---------------------------------------------------------------------------

/// GET /api/brain/recall?q={query}&limit={n}&detail={compact|full}
///
/// Semantic recall via Brain::recall(). Default limit 10, max 50.
async fn handle_brain_recall(
    State(state): State<AppState>,
    Query(params): Query<RecallParams>,
) -> Response {
    // Support `query` as an alias for `q` (both are accepted; `query` wins when `q` is empty).
    let q = if params.q.is_empty() {
        params.query.as_deref().unwrap_or("").to_string()
    } else {
        params.q.clone()
    };

    if q.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "q parameter required"})),
        )
            .into_response();
    }

    let limit = params.limit.unwrap_or(10).min(50);
    let detail = params.detail.as_deref().unwrap_or("compact");

    let memories = match state.brain.engine.recall(&q, Some(limit)).await {
        Ok(m) => m,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": e.to_string()})),
            )
                .into_response();
        }
    };

    // Apply tag filter if requested (server-side, post-recall).
    let memories = if let Some(ref tag) = params.tag {
        memories
            .into_iter()
            .filter(|m| {
                m.tags
                    .as_deref()
                    .map(|t| t.split(',').any(|s| s.trim() == tag.as_str()))
                    .unwrap_or(false)
            })
            .collect::<Vec<_>>()
    } else {
        memories
    };

    let count = memories.len();
    let memories_json = if detail == "full" {
        serde_json::to_value(
            memories
                .into_iter()
                .map(|m| MemoryFull {
                    id: m.id,
                    content: m.content,
                    memory_type: m.memory_type,
                    importance: m.importance,
                    source: m.source,
                    source_type: m.source_type,
                    tags: m.tags,
                    strength: m.strength,
                    created_at: m.created_at,
                    accessed_at: m.accessed_at,
                    access_count: m.access_count,
                    valid_from: m.valid_from,
                    valid_to: m.valid_to,
                })
                .collect::<Vec<_>>(),
        )
        .unwrap_or_default()
    } else {
        serde_json::to_value(
            memories
                .into_iter()
                .map(|m| MemoryCompact {
                    id: m.id,
                    content: m.content,
                    memory_type: m.memory_type,
                    importance: m.importance,
                    trust_level: m.trust_level as i32,
                    created_at: m.created_at,
                })
                .collect::<Vec<_>>(),
        )
        .unwrap_or_default()
    };

    Json(MemoryResponse {
        memories: memories_json,
        count,
    })
    .into_response()
}

/// GET /api/brain/search?q={query}&source={type}&since={date}&limit={n}
///
/// Filtered search: uses Brain::recall() for vector search, then filters
/// by source_type and date range client-side.
async fn handle_brain_search(
    State(state): State<AppState>,
    Query(params): Query<SearchParams>,
) -> Response {
    if params.q.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "q parameter required"})),
        )
            .into_response();
    }

    // Fetch more than needed so we can filter down
    let fetch_limit = params.limit.unwrap_or(10).min(50);
    let over_fetch = (fetch_limit * 3).min(150);

    let memories = match state.brain.engine.recall(&params.q, Some(over_fetch)).await {
        Ok(m) => m,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": e.to_string()})),
            )
                .into_response();
        }
    };

    // Apply filters
    let filtered: Vec<_> = memories
        .into_iter()
        .filter(|m| {
            if let Some(ref source) = params.source {
                if m.memory_type != *source {
                    return false;
                }
            }
            if let Some(ref since) = params.since {
                if m.created_at.as_str() < since.as_str() {
                    return false;
                }
            }
            true
        })
        .take(fetch_limit)
        .collect();

    let count = filtered.len();
    let memories_json = serde_json::to_value(
        filtered
            .into_iter()
            .map(|m| MemoryCompact {
                id: m.id,
                content: m.content,
                memory_type: m.memory_type,
                importance: m.importance,
                trust_level: m.trust_level as i32,
                created_at: m.created_at,
            })
            .collect::<Vec<_>>(),
    )
    .unwrap_or_default();

    Json(MemoryResponse {
        memories: memories_json,
        count,
    })
    .into_response()
}

/// GET /api/brain/timeline?at={iso8601}&limit={n}
///
/// Point-in-time temporal memory query.
async fn handle_brain_timeline(
    State(state): State<AppState>,
    Query(params): Query<TimelineParams>,
) -> Response {
    if params.at.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "at parameter required"})),
        )
            .into_response();
    }

    let limit = params.limit.unwrap_or(10).min(50);

    let memories = match state
        .brain
        .engine
        .store()
        .query_point_in_time(&params.at, limit)
        .await
    {
        Ok(m) => m,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": e.to_string()})),
            )
                .into_response();
        }
    };

    let count = memories.len();
    let memories_json = serde_json::to_value(
        memories
            .into_iter()
            .map(|m| MemoryFull {
                id: m.id,
                content: m.content,
                memory_type: m.memory_type,
                importance: m.importance,
                source: m.source,
                source_type: m.source_type,
                tags: m.tags,
                strength: m.strength,
                created_at: m.created_at,
                accessed_at: m.accessed_at,
                access_count: m.access_count,
                valid_from: m.valid_from,
                valid_to: m.valid_to,
            })
            .collect::<Vec<_>>(),
    )
    .unwrap_or_default();

    Json(MemoryResponse {
        memories: memories_json,
        count,
    })
    .into_response()
}

/// GET /api/brain/summaries?session_id={id}&limit={n}
///
/// Returns session summary memories, optionally filtered by session_id.
async fn handle_brain_summaries(
    State(state): State<AppState>,
    Query(params): Query<SummariesParams>,
) -> Response {
    let limit = params.limit.unwrap_or(10).min(50);

    let memories = match state
        .brain
        .engine
        .store()
        .get_summaries(params.session_id.as_deref(), limit)
        .await
    {
        Ok(m) => m,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": e.to_string()})),
            )
                .into_response();
        }
    };

    let count = memories.len();
    let memories_json = serde_json::to_value(
        memories
            .into_iter()
            .map(|m| MemoryFull {
                id: m.id,
                content: m.content,
                memory_type: m.memory_type,
                importance: m.importance,
                source: m.source,
                source_type: m.source_type,
                tags: m.tags,
                strength: m.strength,
                created_at: m.created_at,
                accessed_at: m.accessed_at,
                access_count: m.access_count,
                valid_from: m.valid_from,
                valid_to: m.valid_to,
            })
            .collect::<Vec<_>>(),
    )
    .unwrap_or_default();

    Json(MemoryResponse {
        memories: memories_json,
        count,
    })
    .into_response()
}

/// GET /api/brain/context?project={name}
///
/// THE KEY ENDPOINT for SessionStart integration.
/// Returns pre-formatted MARKDOWN suitable for direct injection.
/// Token-budgeted: max ~1000 tokens (~4000 chars).
async fn handle_brain_context(
    State(state): State<AppState>,
    Query(params): Query<ContextParams>,
) -> Response {
    const MAX_CHARS: usize = 4000;

    let query = params
        .project
        .as_deref()
        .unwrap_or("recent work sessions decisions");

    // Gather data from multiple brain queries in parallel-ish fashion
    let (recalled, summaries, decisions, corrections) = tokio::join!(
        state.brain.engine.recall(query, Some(15)),
        state.brain.engine.store().get_summaries(None, 5),
        state
            .brain
            .engine
            .recall("decision architecture choice", Some(5)),
        state
            .brain
            .engine
            .recall("correction mistake fix error", Some(5)),
    );

    let mut output = String::new();
    let mut chars_used: usize = 0;

    // Helper: append a section if budget allows
    let append_section =
        |output: &mut String, chars_used: &mut usize, header: &str, lines: Vec<String>| {
            if lines.is_empty() || *chars_used >= MAX_CHARS {
                return;
            }
            let section_header = format!("## {header}\n");
            let mut section_body = String::new();
            for line in &lines {
                let candidate = format!("- {line}\n");
                if *chars_used + section_header.len() + section_body.len() + candidate.len()
                    > MAX_CHARS
                {
                    break;
                }
                section_body.push_str(&candidate);
            }
            if !section_body.is_empty() {
                output.push_str(&section_header);
                output.push_str(&section_body);
                output.push('\n');
                *chars_used += section_header.len() + section_body.len() + 1;
            }
        };

    // Section 1: Recent Work (from session summaries)
    if let Ok(summaries) = summaries {
        let lines: Vec<String> = summaries
            .iter()
            .map(|m| {
                let date = m.created_at.get(..10).unwrap_or("unknown");
                // Truncate long content
                let content = if m.content.len() > 120 {
                    let end = m.content.floor_char_boundary(120);
                    format!("{}...", &m.content[..end])
                } else {
                    m.content.clone()
                };
                format!("[{date}] {content}")
            })
            .collect();
        append_section(&mut output, &mut chars_used, "Recent Work", lines);
    }

    // Section 2: Key Decisions (from recalled decision-type memories)
    if let Ok(decisions) = decisions {
        let lines: Vec<String> = decisions
            .iter()
            .filter(|m| m.memory_type == "decision" || m.importance >= 0.7)
            .map(|m| {
                if m.content.len() > 150 {
                    let end = m.content.floor_char_boundary(150);
                    format!("{}...", &m.content[..end])
                } else {
                    m.content.clone()
                }
            })
            .collect();
        append_section(&mut output, &mut chars_used, "Key Decisions", lines);
    }

    // Section 3: Corrections
    if let Ok(corrections) = corrections {
        let lines: Vec<String> = corrections
            .iter()
            .filter(|m| m.memory_type == "correction" || m.importance >= 0.7)
            .map(|m| {
                if m.content.len() > 150 {
                    let end = m.content.floor_char_boundary(150);
                    format!("{}...", &m.content[..end])
                } else {
                    m.content.clone()
                }
            })
            .collect();
        append_section(&mut output, &mut chars_used, "Corrections", lines);
    }

    // Section 4: Patterns (from general recall, excluding types already used)
    if let Ok(recalled) = recalled {
        let lines: Vec<String> = recalled
            .iter()
            .filter(|m| m.memory_type != "session_summary" && m.importance >= 0.5)
            .take(5)
            .map(|m| {
                if m.content.len() > 150 {
                    let end = m.content.floor_char_boundary(150);
                    format!("{}...", &m.content[..end])
                } else {
                    m.content.clone()
                }
            })
            .collect();
        append_section(&mut output, &mut chars_used, "Patterns", lines);
    }

    if output.is_empty() {
        output = "No brain context available.\n".to_string();
    }

    (
        StatusCode::OK,
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/markdown; charset=utf-8"),
        )],
        output,
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// Router builder
// ---------------------------------------------------------------------------

/// Build the dashboard REST API router with live state.
///
/// Merge into the main app via `app.merge(dashboard_routes(state.clone()))`.
pub fn dashboard_routes(state: AppState) -> Router {
    Router::new()
        .route("/api/dashboard/overview", get(get_overview))
        .route("/api/dashboard/sessions", get(get_sessions))
        .route("/api/dashboard/tools", get(get_tools))
        .route("/api/dashboard/metrics", get(get_metrics))
        .route("/api/dashboard/brain", get(get_brain))
        .route("/api/dashboard/devices", get(get_devices))
        .route("/api/dashboard/health", get(get_health))
        // Brain REST endpoints (for hooks and external callers)
        .route("/api/brain/remember", post(handle_brain_remember))
        .route("/api/brain/recall", get(handle_brain_recall))
        .route("/api/brain/search", get(handle_brain_search))
        .route("/api/brain/timeline", get(handle_brain_timeline))
        .route("/api/brain/summaries", get(handle_brain_summaries))
        .route("/api/brain/context", get(handle_brain_context))
        .route("/api/brain/reembed", post(handle_brain_reembed))
        .route("/api/brain/pending", get(handle_brain_pending_list))
        .route(
            "/api/brain/pending/{id}/approve",
            post(handle_brain_pending_approve),
        )
        .route(
            "/api/brain/pending/{id}/reject",
            post(handle_brain_pending_reject),
        )
        // Brain observability endpoints
        .route("/api/brain/status", get(handle_brain_status))
        .route("/api/brain/browse", get(handle_brain_browse))
        .route("/api/brain/metrics", get(handle_brain_metrics))
        .route("/api/brain/health", get(handle_brain_health_rest))
        // Command center endpoints
        .route("/api/command/sessions", get(command_sessions))
        .route("/api/command/usage", get(command_usage))
        .route(
            "/api/command/sessions/{id}/action",
            post(command_session_action),
        )
        .with_state(state)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- Type serialization unit tests (no router needed) --

    #[test]
    fn test_dashboard_overview_serialization() {
        let overview = DashboardOverview {
            uptime_secs: 3600,
            active_sessions: 5,
            total_messages: 100,
            connected_devices: 2,
            memory_count: 50,
            active_tools: 17,
            llm_provider_count: 2,
            version: "0.1.0".to_string(),
            llm_status: "Healthy".to_string(),
        };
        let json = serde_json::to_string(&overview).unwrap();
        assert!(json.contains("\"uptime_secs\":3600"));
        assert!(json.contains("\"active_sessions\":5"));
        assert!(json.contains("\"version\":\"0.1.0\""));
        assert!(json.contains("\"llm_status\":\"Healthy\""));
    }

    #[test]
    fn test_session_summary_serialization() {
        let session = SessionSummary {
            id: "sess-001".to_string(),
            name: Some("Test Session".to_string()),
            message_count: 42,
            created_at: "2026-03-12T00:00:00Z".to_string(),
            updated_at: "2026-03-12T01:00:00Z".to_string(),
        };
        let json = serde_json::to_string(&session).unwrap();
        assert!(json.contains("\"id\":\"sess-001\""));
        assert!(json.contains("\"name\":\"Test Session\""));
        assert!(json.contains("\"message_count\":42"));
    }

    #[test]
    fn test_session_summary_with_null_name() {
        let session = SessionSummary {
            id: "sess-002".to_string(),
            name: None,
            message_count: 0,
            created_at: "2026-03-12T00:00:00Z".to_string(),
            updated_at: "2026-03-12T00:00:00Z".to_string(),
        };
        let json = serde_json::to_string(&session).unwrap();
        assert!(json.contains("\"name\":null"));
    }

    #[test]
    fn test_tool_summary_serialization() {
        let tool = ToolSummary {
            name: "shell".to_string(),
            description: "Execute shell command".to_string(),
            tier: "Confirm".to_string(),
            category: "builtin".to_string(),
        };
        let json = serde_json::to_string(&tool).unwrap();
        assert!(json.contains("\"name\":\"shell\""));
        assert!(json.contains("\"tier\":\"Confirm\""));
        assert!(json.contains("\"category\":\"builtin\""));
    }

    #[test]
    fn test_metrics_snapshot_serialization() {
        let metrics = MetricsSnapshot {
            requests_total: 1000,
            errors_total: 5,
            active_connections: 3,
            llm_tokens_used: 50000,
            avg_response_ms: 123.456,
            uptime_secs: 7200,
        };
        let json = serde_json::to_string(&metrics).unwrap();
        assert!(json.contains("\"requests_total\":1000"));
        assert!(json.contains("\"avg_response_ms\":123.456"));
        assert!(json.contains("\"llm_tokens_used\":50000"));
    }

    #[test]
    fn test_brain_summary_serialization() {
        let mut memory_types = HashMap::new();
        memory_types.insert("identity".to_string(), 10);
        memory_types.insert("long_term".to_string(), 200);
        let brain = BrainSummary {
            total_memories: 210,
            memory_types,
            total_entities: 50,
            total_relations: 30,
            last_memory_at: Some("2026-03-12T12:00:00Z".to_string()),
        };
        let json = serde_json::to_string(&brain).unwrap();
        assert!(json.contains("\"total_memories\":210"));
        assert!(json.contains("\"total_entities\":50"));
        assert!(json.contains("\"total_relations\":30"));
        assert!(json.contains("2026-03-12T12:00:00Z"));
    }

    #[test]
    fn test_brain_summary_with_no_last_memory() {
        let brain = BrainSummary {
            total_memories: 0,
            memory_types: HashMap::new(),
            total_entities: 0,
            total_relations: 0,
            last_memory_at: None,
        };
        let json = serde_json::to_string(&brain).unwrap();
        assert!(json.contains("\"last_memory_at\":null"));
    }

    #[test]
    fn test_device_summary_serialization() {
        let device = DeviceSummary {
            id: "dev-001".to_string(),
            name: Some("Dr. CLAW App".to_string()),
            connected: true,
            last_seen: "2026-03-12T10:00:00Z".to_string(),
        };
        let json = serde_json::to_string(&device).unwrap();
        assert!(json.contains("\"id\":\"dev-001\""));
        assert!(json.contains("\"connected\":true"));
        assert!(json.contains("\"name\":\"Dr. CLAW App\""));
    }

    #[test]
    fn test_device_summary_disconnected() {
        let device = DeviceSummary {
            id: "dev-002".to_string(),
            name: None,
            connected: false,
            last_seen: "2026-03-11T23:00:00Z".to_string(),
        };
        let json = serde_json::to_string(&device).unwrap();
        assert!(json.contains("\"connected\":false"));
        assert!(json.contains("\"name\":null"));
    }

    #[test]
    fn test_health_check_serialization() {
        let health = HealthCheck {
            status: "ok".to_string(),
            checks: HealthChecks {
                database: true,
                llm: false,
                brain: true,
                tools: true,
            },
            uptime_secs: 3600,
        };
        let json = serde_json::to_string(&health).unwrap();
        assert!(json.contains("\"status\":\"ok\""));
        assert!(json.contains("\"database\":true"));
        assert!(json.contains("\"llm\":false"));
        assert!(json.contains("\"brain\":true"));
        assert!(json.contains("\"tools\":true"));
        assert!(json.contains("\"uptime_secs\":3600"));
    }

    // -- Command Center type tests --

    #[test]
    fn test_unified_session_serialization() {
        let session = UnifiedSession {
            id: "s1".to_string(),
            source: "ironjaw".to_string(),
            title: Some("Test".to_string()),
            project: None,
            model: Some("qwen3-coder".to_string()),
            role: None,
            status: "active".to_string(),
            total_tokens: 5000,
            cost_usd: 0.0,
            context_pct: Some(45.0),
        };
        let json = serde_json::to_string(&session).unwrap();
        assert!(json.contains("\"source\":\"ironjaw\""));
        assert!(json.contains("\"total_tokens\":5000"));
    }

    #[test]
    fn test_usage_summary_serialization() {
        let usage = UsageSummaryResponse {
            total_tokens: 10000,
            total_cost_usd: 1.50,
            request_count: 42,
            by_model: vec![ModelUsage {
                model: "claude-3-opus".to_string(),
                tokens: 10000,
                cost_usd: 1.50,
                requests: 42,
            }],
        };
        let json = serde_json::to_string(&usage).unwrap();
        assert!(json.contains("\"total_tokens\":10000"));
        assert!(json.contains("\"claude-3-opus\""));
    }

    #[test]
    fn test_session_action_deserialization() {
        let json = r#"{"action":"destroy"}"#;
        let action: SessionAction = serde_json::from_str(json).unwrap();
        assert_eq!(action.action, "destroy");
        assert!(action.params.is_none());
    }

    #[test]
    fn test_session_action_with_params() {
        let json = r#"{"action":"send","params":{"message":"hello"}}"#;
        let action: SessionAction = serde_json::from_str(json).unwrap();
        assert_eq!(action.action, "send");
        assert!(action.params.is_some());
    }

    // -- Brain Query API type tests --

    #[test]
    fn test_memory_compact_serialization() {
        let compact = MemoryCompact {
            id: 42,
            content: "Docker networking uses bridge mode".to_string(),
            memory_type: "fact".to_string(),
            importance: 0.7,
            trust_level: 3,
            created_at: "2026-01-01T00:00:00".to_string(),
        };
        let json = serde_json::to_string(&compact).unwrap();
        assert!(json.contains("\"id\":42"));
        assert!(json.contains("\"memory_type\":\"fact\""));
        assert!(json.contains("\"importance\":0.7"));
        assert!(json.contains("Docker networking"));
        assert!(json.contains("\"trust_level\":3"));
    }

    #[test]
    fn test_memory_full_serialization() {
        let full = MemoryFull {
            id: 15,
            content: "Always use --network host".to_string(),
            memory_type: "decision".to_string(),
            importance: 0.8,
            source: Some("user".to_string()),
            source_type: "conversation".to_string(),
            tags: Some("docker,networking".to_string()),
            strength: 0.95,
            created_at: "2026-03-28T10:00:00Z".to_string(),
            accessed_at: "2026-03-29T08:00:00Z".to_string(),
            access_count: 5,
            valid_from: Some("2026-03-28T10:00:00Z".to_string()),
            valid_to: None,
        };
        let json = serde_json::to_string(&full).unwrap();
        assert!(json.contains("\"id\":15"));
        assert!(json.contains("\"source\":\"user\""));
        assert!(json.contains("\"strength\":0.95"));
        assert!(json.contains("\"access_count\":5"));
        assert!(json.contains("\"valid_to\":null"));
    }

    #[test]
    fn test_memory_response_serialization() {
        let resp = MemoryResponse {
            memories: serde_json::json!([{"id": 1, "content": "test"}]),
            count: 1,
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"count\":1"));
        assert!(json.contains("\"memories\":["));
    }

    #[test]
    fn test_recall_params_deserialization() {
        let json = r#"{"q":"rust","limit":5,"detail":"full"}"#;
        let params: RecallParams = serde_json::from_str(json).unwrap();
        assert_eq!(params.q, "rust");
        assert_eq!(params.limit, Some(5));
        assert_eq!(params.detail.as_deref(), Some("full"));
    }

    #[test]
    fn test_recall_params_defaults() {
        let json = r#"{"q":"test"}"#;
        let params: RecallParams = serde_json::from_str(json).unwrap();
        assert_eq!(params.q, "test");
        assert!(params.limit.is_none());
        assert!(params.detail.is_none());
    }

    #[test]
    fn test_search_params_deserialization() {
        let json = r#"{"q":"docker","source":"decision","since":"2026-03-01","limit":20}"#;
        let params: SearchParams = serde_json::from_str(json).unwrap();
        assert_eq!(params.q, "docker");
        assert_eq!(params.source.as_deref(), Some("decision"));
        assert_eq!(params.since.as_deref(), Some("2026-03-01"));
        assert_eq!(params.limit, Some(20));
    }

    #[test]
    fn test_timeline_params_deserialization() {
        let json = r#"{"at":"2026-03-28T10:00:00Z","limit":15}"#;
        let params: TimelineParams = serde_json::from_str(json).unwrap();
        assert_eq!(params.at, "2026-03-28T10:00:00Z");
        assert_eq!(params.limit, Some(15));
    }

    #[test]
    fn test_summaries_params_deserialization() {
        let json = r#"{"session_id":"sess-123","limit":5}"#;
        let params: SummariesParams = serde_json::from_str(json).unwrap();
        assert_eq!(params.session_id.as_deref(), Some("sess-123"));
        assert_eq!(params.limit, Some(5));
    }

    #[test]
    fn test_summaries_params_defaults() {
        let json = r#"{}"#;
        let params: SummariesParams = serde_json::from_str(json).unwrap();
        assert!(params.session_id.is_none());
        assert!(params.limit.is_none());
    }

    #[test]
    fn test_context_params_deserialization() {
        let json = r#"{"project":"ironjaw"}"#;
        let params: ContextParams = serde_json::from_str(json).unwrap();
        assert_eq!(params.project.as_deref(), Some("ironjaw"));
    }

    #[test]
    fn test_context_params_defaults() {
        let json = r#"{}"#;
        let params: ContextParams = serde_json::from_str(json).unwrap();
        assert!(params.project.is_none());
    }

    // -- Brain Observability type tests --

    #[test]
    fn test_brain_status_serialization() {
        let mut by_source = HashMap::new();
        by_source.insert("brain".to_string(), 3800);
        by_source.insert("cc_chronicle".to_string(), 200);
        let mut by_type = HashMap::new();
        by_type.insert("fact".to_string(), 3000);
        by_type.insert("decision".to_string(), 300);

        let status = BrainStatus {
            total_memories: 4100,
            embedded_count: 4100,
            unembedded_count: 0,
            by_source_type: by_source,
            by_memory_type: by_type,
            graph: GraphCounts {
                entities: 150,
                relations: 320,
            },
            indexers: IndexerStatus {
                chronicle: ChronicleIndexerInfo {
                    enabled: true,
                    last_sync: Some("2026-03-29T10:00:00Z".to_string()),
                    synced_count: 200,
                },
                markdown: MarkdownIndexerInfo {
                    enabled: true,
                    watched_files: 15,
                    indexed_chunks: 50,
                },
            },
            recent_activity: RecentActivity {
                last_24h: 42,
                last_7d: 280,
            },
        };
        let json = serde_json::to_string(&status).unwrap();
        assert!(json.contains("\"total_memories\":4100"));
        assert!(json.contains("\"embedded_count\":4100"));
        assert!(json.contains("\"unembedded_count\":0"));
        assert!(json.contains("\"entities\":150"));
        assert!(json.contains("\"relations\":320"));
        assert!(json.contains("\"last_24h\":42"));
        assert!(json.contains("\"last_7d\":280"));
        assert!(json.contains("\"watched_files\":15"));
    }

    #[test]
    fn test_brain_status_empty() {
        let status = BrainStatus {
            total_memories: 0,
            embedded_count: 0,
            unembedded_count: 0,
            by_source_type: HashMap::new(),
            by_memory_type: HashMap::new(),
            graph: GraphCounts {
                entities: 0,
                relations: 0,
            },
            indexers: IndexerStatus {
                chronicle: ChronicleIndexerInfo {
                    enabled: false,
                    last_sync: None,
                    synced_count: 0,
                },
                markdown: MarkdownIndexerInfo {
                    enabled: false,
                    watched_files: 0,
                    indexed_chunks: 0,
                },
            },
            recent_activity: RecentActivity {
                last_24h: 0,
                last_7d: 0,
            },
        };
        let json = serde_json::to_string(&status).unwrap();
        assert!(json.contains("\"total_memories\":0"));
        assert!(json.contains("\"enabled\":false"));
        assert!(json.contains("\"last_sync\":null"));
    }

    #[test]
    fn test_browse_response_serialization() {
        let resp = BrowseResponse {
            memories: vec![BrowseMemory {
                id: 42,
                content: "Docker uses bridge mode".to_string(),
                memory_type: "fact".to_string(),
                source_type: "brain".to_string(),
                importance: 0.7,
                created_at: "2026-03-28T10:00:00Z".to_string(),
                accessed_at: "2026-03-29T08:00:00Z".to_string(),
                access_count: 5,
            }],
            total: 4104,
            page: 1,
            per_page: 20,
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"total\":4104"));
        assert!(json.contains("\"page\":1"));
        assert!(json.contains("\"per_page\":20"));
        assert!(json.contains("\"id\":42"));
        assert!(json.contains("\"access_count\":5"));
        assert!(json.contains("\"source_type\":\"brain\""));
    }

    #[test]
    fn test_browse_response_empty() {
        let resp = BrowseResponse {
            memories: vec![],
            total: 0,
            page: 1,
            per_page: 20,
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"memories\":[]"));
        assert!(json.contains("\"total\":0"));
    }

    #[test]
    fn test_browse_params_deserialization() {
        let json = r#"{"page":2,"per_page":50,"source":"brain","type":"fact","sort":"importance"}"#;
        let params: BrowseParams = serde_json::from_str(json).unwrap();
        assert_eq!(params.page, Some(2));
        assert_eq!(params.per_page, Some(50));
        assert_eq!(params.source.as_deref(), Some("brain"));
        assert_eq!(params.memory_type.as_deref(), Some("fact"));
        assert_eq!(params.sort.as_deref(), Some("importance"));
        // New filters default to None for backward compatibility
        assert!(params.scope_prefix.is_none());
        assert!(params.trust_level_min.is_none());
    }

    #[test]
    fn test_browse_params_defaults() {
        let json = r#"{}"#;
        let params: BrowseParams = serde_json::from_str(json).unwrap();
        assert!(params.page.is_none());
        assert!(params.per_page.is_none());
        assert!(params.source.is_none());
        assert!(params.memory_type.is_none());
        assert!(params.sort.is_none());
        assert!(params.scope_prefix.is_none());
        assert!(params.trust_level_min.is_none());
    }

    #[test]
    fn test_browse_params_with_scope_and_trust() {
        let json = r#"{"scope_prefix":"/wiki/research-notes/","trust_level_min":2,"page":1,"per_page":20}"#;
        let params: BrowseParams = serde_json::from_str(json).unwrap();
        assert_eq!(
            params.scope_prefix.as_deref(),
            Some("/wiki/research-notes/")
        );
        assert_eq!(params.trust_level_min, Some(2));
        assert_eq!(params.page, Some(1));
    }

    #[test]
    fn test_pending_list_params_deserialization() {
        let json = r#"{"limit":25,"offset":10}"#;
        let params: PendingListParams = serde_json::from_str(json).unwrap();
        assert_eq!(params.limit, Some(25));
        assert_eq!(params.offset, Some(10));
    }

    #[test]
    fn test_pending_list_params_defaults() {
        let json = r#"{}"#;
        let params: PendingListParams = serde_json::from_str(json).unwrap();
        assert!(params.limit.is_none());
        assert!(params.offset.is_none());
    }

    #[test]
    fn test_brain_metrics_serialization() {
        let metrics = BrainMetrics {
            embedding_backend: "llama-server".to_string(),
            vec_table_count: 4104,
            source_health: SourceHealth {
                chronicle: SourceSyncInfo {
                    last_sync_at: Some("2026-03-29T10:00:00Z".to_string()),
                    items_synced: 200,
                },
                markdown: SourceSyncInfo {
                    last_sync_at: Some("2026-03-29T09:00:00Z".to_string()),
                    items_synced: 15,
                },
            },
            consolidation: ConsolidationInfo {
                last_run: Some("2026-03-29T06:00:00Z".to_string()),
                next_run_hours: Some(4.5),
            },
        };
        let json = serde_json::to_string(&metrics).unwrap();
        assert!(json.contains("\"embedding_backend\":\"llama-server\""));
        assert!(json.contains("\"vec_table_count\":4104"));
        assert!(json.contains("\"items_synced\":200"));
        assert!(json.contains("\"next_run_hours\":4.5"));
    }

    #[test]
    fn test_brain_metrics_no_consolidation() {
        let metrics = BrainMetrics {
            embedding_backend: "llama-server".to_string(),
            vec_table_count: 0,
            source_health: SourceHealth {
                chronicle: SourceSyncInfo {
                    last_sync_at: None,
                    items_synced: 0,
                },
                markdown: SourceSyncInfo {
                    last_sync_at: None,
                    items_synced: 0,
                },
            },
            consolidation: ConsolidationInfo {
                last_run: None,
                next_run_hours: None,
            },
        };
        let json = serde_json::to_string(&metrics).unwrap();
        assert!(json.contains("\"last_run\":null"));
        assert!(json.contains("\"next_run_hours\":null"));
        assert!(json.contains("\"last_sync_at\":null"));
    }

    #[test]
    fn test_graph_counts_serialization() {
        let g = GraphCounts {
            entities: 50,
            relations: 120,
        };
        let json = serde_json::to_string(&g).unwrap();
        assert!(json.contains("\"entities\":50"));
        assert!(json.contains("\"relations\":120"));
    }

    #[test]
    fn test_consolidation_info_serialization() {
        let info = ConsolidationInfo {
            last_run: Some("2026-03-29T06:00:00Z".to_string()),
            next_run_hours: Some(2.3),
        };
        let json = serde_json::to_string(&info).unwrap();
        assert!(json.contains("\"last_run\":\"2026-03-29T06:00:00Z\""));
        assert!(json.contains("\"next_run_hours\":2.3"));
    }

    /// Regression test for UTF-8 byte-boundary panic in memory summary truncation.
    ///
    /// The box-drawing character '─' (U+2500) is 3 bytes (0xE2 0x94 0x80).  A
    /// string of 119 ASCII bytes followed by '─' has its char boundary for the
    /// second byte of '─' at offset 120 — exactly the truncation limit that
    /// caused the gateway crash on 2026-04-21.  `floor_char_boundary` must
    /// retreat to offset 119 (the last ASCII byte boundary) so that slicing
    /// never panics and the result is valid UTF-8.
    #[test]
    fn test_memory_truncation_multibyte_char_boundary() {
        // Build a string: 119 ASCII 'a' bytes + '─' (3 bytes) + " suffix"
        // Total > 120 bytes, with a multi-byte char spanning the 120-byte mark.
        let prefix = "a".repeat(119);
        let input = format!("{prefix}─ suffix that should be cut");

        // Sanity: byte 120 is inside '─', not a char boundary.
        assert!(
            !input.is_char_boundary(120),
            "test setup: byte 120 must not be a char boundary"
        );

        // Apply the same logic used in the fixed dashboard code.
        let truncated = if input.len() > 120 {
            let end = input.floor_char_boundary(120);
            format!("{}...", &input[..end])
        } else {
            input
        };

        // Must not panic (we got here), must be valid UTF-8, and must end with "...".
        assert!(
            std::str::from_utf8(truncated.as_bytes()).is_ok(),
            "result must be valid UTF-8"
        );
        assert!(
            truncated.ends_with("..."),
            "truncated string must end with ellipsis"
        );
        // The prefix of 119 ASCII bytes must be preserved intact.
        assert!(
            truncated.starts_with(&prefix),
            "ASCII prefix must be preserved"
        );
        // The '─' char itself must NOT appear (it was cut at the boundary before it).
        assert!(
            !truncated.contains('─'),
            "multi-byte char that straddles boundary must be excluded"
        );

        // Also verify the 150-byte variant (used by Key Decisions / Corrections / Patterns).
        let prefix150 = "b".repeat(149);
        let input150 = format!("{prefix150}─ more text here");
        assert!(
            !input150.is_char_boundary(150),
            "test setup: byte 150 must not be a char boundary"
        );

        let truncated150 = if input150.len() > 150 {
            let end = input150.floor_char_boundary(150);
            format!("{}...", &input150[..end])
        } else {
            input150
        };
        assert!(std::str::from_utf8(truncated150.as_bytes()).is_ok());
        assert!(truncated150.ends_with("..."));
        assert!(!truncated150.contains('─'));
    }
}
