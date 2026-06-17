//! API response types and handler functions.

use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde::{Deserialize, Serialize};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use tokio::sync::Mutex;

use stint_core::models::entry::EntryFilter;
use stint_core::service::StintService;
use stint_core::storage::sqlite::SqliteStorage;
use stint_core::storage::Storage;

/// Shared application state.
pub type AppState = Arc<Mutex<StintService<SqliteStorage>>>;

/// Maximum number of entries the API will return in a single request.
/// Prevents memory exhaustion on large databases.
const MAX_LIMIT: usize = 10_000;

// --- Response DTOs ---

/// Response for GET /api/status.
#[derive(Serialize)]
pub struct StatusResponse {
    pub tracking: bool,
    pub project: Option<String>,
    pub elapsed_secs: Option<i64>,
    pub entry_id: Option<String>,
    pub started_at: Option<String>,
}

/// Response for GET /api/entries.
#[derive(Serialize)]
pub struct EntryResponse {
    pub id: String,
    pub project: String,
    pub start: String,
    pub end: Option<String>,
    pub duration_secs: Option<i64>,
    pub source: String,
    pub notes: Option<String>,
    pub tags: Vec<String>,
    pub running: bool,
}

/// Response for GET /api/projects.
#[derive(Serialize)]
pub struct ProjectResponse {
    pub id: String,
    pub name: String,
    pub paths: Vec<String>,
    pub tags: Vec<String>,
    pub hourly_rate_cents: Option<i64>,
    pub status: String,
    pub source: String,
}

/// Query parameters for GET /api/entries.
#[derive(Deserialize)]
pub struct EntriesQuery {
    pub from: Option<String>,
    pub to: Option<String>,
    pub project: Option<String>,
    pub limit: Option<usize>,
}

/// Request body for POST /api/start.
#[derive(Deserialize)]
pub struct StartRequest {
    pub project: String,
}

/// Generic error response.
#[derive(Serialize)]
pub struct ErrorResponse {
    pub error: String,
}

// --- Helpers ---

/// Formats an OffsetDateTime as RFC 3339 string.
fn fmt_ts(ts: &OffsetDateTime) -> String {
    ts.format(&Rfc3339).unwrap_or_default()
}

/// Returns an error JSON response.
fn error_response(status: StatusCode, msg: &str) -> impl IntoResponse {
    (
        status,
        Json(ErrorResponse {
            error: msg.to_string(),
        }),
    )
}

// --- Handlers ---

/// GET /api/health
pub async fn health() -> impl IntoResponse {
    Json(serde_json::json!({"ok": true}))
}

/// GET /api/status
pub async fn status(State(state): State<AppState>) -> impl IntoResponse {
    let service = state.lock().await;
    match service.get_status() {
        Ok(Some((entry, project))) => {
            let elapsed = (OffsetDateTime::now_utc() - entry.start).whole_seconds();
            Json(StatusResponse {
                tracking: true,
                project: Some(project.name),
                elapsed_secs: Some(elapsed),
                entry_id: Some(entry.id.to_string()),
                started_at: Some(fmt_ts(&entry.start)),
            })
            .into_response()
        }
        Ok(None) => Json(StatusResponse {
            tracking: false,
            project: None,
            elapsed_secs: None,
            entry_id: None,
            started_at: None,
        })
        .into_response(),
        Err(e) => error_response(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()).into_response(),
    }
}

/// GET /api/entries
pub async fn entries(
    State(state): State<AppState>,
    Query(query): Query<EntriesQuery>,
) -> impl IntoResponse {
    let service = state.lock().await;

    let mut filter = EntryFilter::default();

    if let Some(ref name) = query.project {
        match service.resolve_project_id(name) {
            Ok(id) => filter.project_id = Some(id),
            Err(e) => {
                return error_response(StatusCode::BAD_REQUEST, &e.to_string()).into_response()
            }
        }
    }

    if let Some(ref from) = query.from {
        match OffsetDateTime::parse(from, &Rfc3339) {
            Ok(dt) => filter.from = Some(dt),
            Err(_) => {
                return error_response(
                    StatusCode::BAD_REQUEST,
                    &format!("invalid 'from' date: '{from}' (expected RFC 3339)"),
                )
                .into_response()
            }
        }
    }

    if let Some(ref to) = query.to {
        match OffsetDateTime::parse(to, &Rfc3339) {
            Ok(dt) => filter.to = Some(dt),
            Err(_) => {
                return error_response(
                    StatusCode::BAD_REQUEST,
                    &format!("invalid 'to' date: '{to}' (expected RFC 3339)"),
                )
                .into_response()
            }
        }
    }

    match service.get_entries(&filter) {
        Ok(entries) => {
            let limit = query.limit.unwrap_or(entries.len()).min(MAX_LIMIT);
            let responses: Vec<EntryResponse> = entries
                .into_iter()
                .take(limit)
                .map(|(entry, project)| {
                    let running = entry.is_running();
                    let duration = if running {
                        Some((OffsetDateTime::now_utc() - entry.start).whole_seconds())
                    } else {
                        entry.computed_duration_secs()
                    };
                    EntryResponse {
                        id: entry.id.to_string(),
                        project: project.name,
                        start: fmt_ts(&entry.start),
                        end: entry.end.as_ref().map(fmt_ts),
                        duration_secs: duration,
                        source: entry.source.as_str().to_string(),
                        notes: entry.notes,
                        tags: entry.tags,
                        running,
                    }
                })
                .collect();
            Json(responses).into_response()
        }
        Err(e) => error_response(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()).into_response(),
    }
}

/// GET /api/projects
pub async fn projects(State(state): State<AppState>) -> impl IntoResponse {
    let service = state.lock().await;
    let result = service.storage().list_projects(None);
    drop(service);
    match result {
        Ok(projects) => {
            let responses: Vec<ProjectResponse> = projects
                .into_iter()
                .map(|p| {
                    let paths: Vec<String> = p
                        .paths
                        .iter()
                        .map(|path| path.to_string_lossy().to_string())
                        .collect();
                    ProjectResponse {
                        id: p.id.to_string(),
                        name: p.name,
                        paths,
                        tags: p.tags,
                        hourly_rate_cents: p.hourly_rate_cents,
                        status: p.status.as_str().to_string(),
                        source: p.source.as_str().to_string(),
                    }
                })
                .collect();
            Json(responses).into_response()
        }
        Err(e) => error_response(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()).into_response(),
    }
}

/// POST /api/start
pub async fn start(
    State(state): State<AppState>,
    Json(body): Json<StartRequest>,
) -> impl IntoResponse {
    let service = state.lock().await;
    match service.start_timer(&body.project) {
        Ok((entry, project)) => {
            let response = EntryResponse {
                id: entry.id.to_string(),
                project: project.name,
                start: fmt_ts(&entry.start),
                end: None,
                duration_secs: Some(0),
                source: entry.source.as_str().to_string(),
                notes: entry.notes,
                tags: entry.tags,
                running: true,
            };
            (StatusCode::CREATED, Json(response)).into_response()
        }
        Err(e) => error_response(StatusCode::BAD_REQUEST, &e.to_string()).into_response(),
    }
}

/// POST /api/stop
pub async fn stop(State(state): State<AppState>) -> impl IntoResponse {
    let service = state.lock().await;
    match service.stop_timer() {
        Ok((entry, project)) => {
            let response = EntryResponse {
                id: entry.id.to_string(),
                project: project.name,
                start: fmt_ts(&entry.start),
                end: entry.end.as_ref().map(fmt_ts),
                duration_secs: entry.duration_secs,
                source: entry.source.as_str().to_string(),
                notes: entry.notes.clone(),
                tags: entry.tags.clone(),
                running: false,
            };
            Json(response).into_response()
        }
        Err(e) => error_response(StatusCode::BAD_REQUEST, &e.to_string()).into_response(),
    }
}
