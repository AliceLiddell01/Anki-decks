//! Явно запускаемая локальная HTTP-граница. Зависимость направлена к core;
//! ни одна штатная команда `anki-repo` не знает о сервере наблюдения.
pub mod catalog;
pub mod dto;
pub mod files;
#[cfg(test)]
mod tests;

use axum::{
    Json, Router,
    extract::{Path as HttpPath, Query as HttpQuery, Request, State},
    http::{StatusCode, header},
    middleware::{self, Next},
    response::{
        IntoResponse, Response,
        sse::{Event, KeepAlive, Sse},
    },
    routing::get,
};
use catalog::{Catalog, ReviewData, unit_summary, value};
use dto::{Availability, Envelope, Query};
use files::SafeRoot;
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    convert::Infallible,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, RwLock},
    time::Duration,
};
use tokio::sync::{broadcast, watch};

type ApiResult = Result<Json<Envelope<Value>>, (StatusCode, Json<Envelope<Value>>)>;
fn failure(status: StatusCode, message: impl Into<String>) -> (StatusCode, Json<Envelope<Value>>) {
    (
        status,
        Json(Envelope::absent(Availability::Unavailable, message)),
    )
}
fn json_response<T: Serialize>(envelope: Envelope<T>) -> Json<Envelope<Value>> {
    Json(Envelope {
        api_version: envelope.api_version,
        state: envelope.state,
        data: envelope.data.map(value),
        provenance: envelope.provenance,
        diagnostics: envelope.diagnostics,
    })
}

#[derive(Clone, Debug, Serialize)]
pub struct LiveEvent {
    pub revision: u64,
    pub observed_unix_ms: u64,
}
#[derive(Clone)]
pub struct AppState {
    pub root: Arc<SafeRoot>,
    pub catalog: Arc<RwLock<Arc<Catalog>>>,
    pub address: SocketAddr,
    events: broadcast::Sender<LiveEvent>,
    replay: Arc<Mutex<VecDeque<LiveEvent>>>,
    event_stop: watch::Sender<bool>,
}
impl AppState {
    pub fn new(repository: &Path, address: SocketAddr) -> Result<Self, String> {
        if !address.ip().is_loopback() {
            return Err("Review Observatory принимает только loopback адрес".into());
        }
        let root = Arc::new(SafeRoot::new(repository)?);
        let initial = catalog::refresh(&root, &Catalog::default());
        let (events, _) = broadcast::channel(64);
        let (event_stop, _) = watch::channel(false);
        Ok(Self {
            root,
            catalog: Arc::new(RwLock::new(Arc::new(initial))),
            address,
            events,
            replay: Arc::new(Mutex::new(VecDeque::new())),
            event_stop,
        })
    }
    pub fn snapshot(&self) -> Arc<Catalog> {
        self.catalog
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
    pub fn shutdown(&self) {
        let _ = self.event_stop.send(true);
    }
    fn publish(&self, updated: Catalog) {
        let event = LiveEvent {
            revision: updated.revision,
            observed_unix_ms: updated.observed_unix_ms,
        };
        let changed = {
            let mut catalog = self.catalog.write().unwrap_or_else(|e| e.into_inner());
            let changed = updated.revision != catalog.revision;
            *catalog = Arc::new(updated);
            changed
        };
        if changed {
            let mut replay = self.replay.lock().unwrap_or_else(|e| e.into_inner());
            if replay.len() == 64 {
                replay.pop_front();
            }
            replay.push_back(event.clone());
            let _ = self.events.send(event);
        }
    }
    /// Короткий polling известного allowlist. Закрытие сервера останавливает только наблюдение.
    pub async fn observe(self, mut shutdown: watch::Receiver<bool>) {
        let mut interval = tokio::time::interval(Duration::from_secs(2));
        interval.tick().await;
        loop {
            tokio::select! {
                _=shutdown.changed()=>break,
                _=interval.tick()=>{
                    let root=self.root.clone();let old=self.snapshot();
                    match tokio::task::spawn_blocking(move||catalog::refresh(&root,&old)).await {Ok(updated)=>self.publish(updated),Err(_)=>{ /* Следующий tick повторит наблюдение; HTTP сохраняет последний durable снимок. */ }}
                }
            }
        }
    }
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/api/v1/snapshot", get(snapshot))
        .route("/api/v1/health", get(health))
        .route("/api/v1/reviews", get(reviews))
        .route("/api/v1/reviews/{review_id}", get(review_detail))
        .route("/api/v1/reviews/{review_id}/candidates", get(candidates))
        .route(
            "/api/v1/reviews/{review_id}/candidates/{candidate_id}",
            get(candidate_detail),
        )
        .route("/api/v1/reviews/{review_id}/units", get(units))
        .route(
            "/api/v1/reviews/{review_id}/units/{unit_id}",
            get(unit_detail),
        )
        .route("/api/v1/reviews/{review_id}/triage", get(triage))
        .route(
            "/api/v1/reviews/{review_id}/triage/groups/{group_id}",
            get(group_detail),
        )
        .route(
            "/api/v1/reviews/{review_id}/triage/findings/{finding_id}",
            get(finding_detail),
        )
        .route("/api/v1/reviews/{review_id}/report", get(report))
        .route("/api/v1/execution", get(jobs))
        .route("/api/v1/execution/{job_id}", get(job_detail))
        .route("/api/v1/learning", get(learning_summary))
        .route("/api/v1/learning/{collection}", get(learning_collection))
        .route("/api/v1/events", get(events))
        .fallback(static_file)
        .layer(middleware::from_fn_with_state(
            state.clone(),
            local_boundary,
        ))
        .with_state(state)
}

async fn local_boundary(State(state): State<AppState>, request: Request, next: Next) -> Response {
    let address = state.address.to_string();
    let localhost = format!("localhost:{}", state.address.port());
    let host = request
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok());
    if !host.is_some_and(|h| h == address || h == localhost) {
        return failure(
            StatusCode::FORBIDDEN,
            "Host не совпадает с локальным адресом сервера",
        )
        .into_response();
    }
    if request.headers().get(header::ORIGIN).is_some_and(|origin| {
        !origin
            .to_str()
            .is_ok_and(|o| o == format!("http://{address}") || o == format!("http://{localhost}"))
    }) {
        return failure(
            StatusCode::FORBIDDEN,
            "Origin не совпадает с локальным сервером",
        )
        .into_response();
    }
    if request
        .headers()
        .get("sec-fetch-site")
        .is_some_and(|v| v == "cross-site")
    {
        return failure(StatusCode::FORBIDDEN, "Cross-site чтение запрещено").into_response();
    }
    if request.method() != axum::http::Method::GET && request.method() != axum::http::Method::HEAD {
        return failure(
            StatusCode::METHOD_NOT_ALLOWED,
            "HTTP API поддерживает только наблюдение",
        )
        .into_response();
    }
    if request.uri().to_string().len() > 4096
        || request
            .headers()
            .get(header::CONTENT_LENGTH)
            .is_some_and(|v| v != "0")
        || request.headers().contains_key(header::TRANSFER_ENCODING)
    {
        return failure(
            StatusCode::PAYLOAD_TOO_LARGE,
            "Тело запроса и длинный URI не поддерживаются",
        )
        .into_response();
    }
    let api = request.uri().path().starts_with("/api/");
    let mut response = next.run(request).await;
    if api
        && response.status().is_client_error()
        && !response
            .headers()
            .get(header::CONTENT_TYPE)
            .is_some_and(|v| v.as_bytes().starts_with(b"application/json"))
    {
        response = failure(
            response.status(),
            "Параметры запроса не соответствуют API v1",
        )
        .into_response();
    }
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("no-store"),
    );
    response.headers_mut().insert(
        "x-content-type-options",
        axum::http::HeaderValue::from_static("nosniff"),
    );
    response.headers_mut().insert(
        "x-frame-options",
        axum::http::HeaderValue::from_static("DENY"),
    );
    response.headers_mut().insert(
        "referrer-policy",
        axum::http::HeaderValue::from_static("no-referrer"),
    );
    response.headers_mut().insert("content-security-policy",axum::http::HeaderValue::from_static("default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; connect-src 'self'; object-src 'none'; base-uri 'none'; frame-ancestors 'none'"));
    response
}
fn checked(q: &Query) -> Result<(), (StatusCode, Json<Envelope<Value>>)> {
    q.bounds()
        .map(|_| ())
        .map_err(|e| failure(StatusCode::BAD_REQUEST, e))
}
fn find_review(
    state: &AppState,
    id: &str,
) -> Result<Arc<ReviewData>, (StatusCode, Json<Envelope<Value>>)> {
    if !files::hex(id, 24) {
        return Err(failure(StatusCode::BAD_REQUEST, "Некорректный review ID"));
    }
    state
        .snapshot()
        .reviews
        .iter()
        .find(|r| r.summary.id == id)
        .cloned()
        .ok_or_else(|| failure(StatusCode::NOT_FOUND, "Review snapshot не найден"))
}
fn resource_id(id: &str) -> bool {
    !id.trim().is_empty() && id.len() <= 1024 && !id.chars().any(char::is_control)
}
async fn snapshot(State(state): State<AppState>) -> ApiResult {
    let c = state.snapshot();
    Ok(json_response(Envelope::available(
        json!({"revision":c.revision,"observed_unix_ms":c.observed_unix_ms,"reviews":c.reviews.iter().map(|r|&r.summary).collect::<Vec<_>>(),"learning":&c.learning,"health":&c.health}),
    )))
}
async fn health(State(state): State<AppState>) -> ApiResult {
    Ok(json_response(Envelope::available(
        state.snapshot().health.clone(),
    )))
}
async fn reviews(State(state): State<AppState>, HttpQuery(q): HttpQuery<Query>) -> ApiResult {
    checked(&q)?;
    let mut rows = state
        .snapshot()
        .reviews
        .iter()
        .map(|r| r.summary.clone())
        .filter(|r| {
            q.matches(&format!(
                "{} {} {}",
                r.namespace, r.head_sha, r.workspace_variant
            ))
        })
        .collect::<Vec<_>>();
    if q.sort.as_deref() == Some("namespace") {
        rows.sort_by(|a, b| a.namespace.cmp(&b.namespace));
    } else if q.sort.as_deref() == Some("oldest") {
        rows.sort_by_key(|r| r.updated_unix_ms);
    }
    Ok(json_response(Envelope::available(
        q.paginate(rows)
            .map_err(|e| failure(StatusCode::BAD_REQUEST, e))?,
    )))
}
async fn review_detail(State(state): State<AppState>, HttpPath(id): HttpPath<String>) -> ApiResult {
    let r = find_review(&state, &id)?;
    let pipeline=r.summary.artifacts.iter().map(|(name,state)|json!({"name":name,"state":state,"description":if *state==Availability::Missing{"Артефакт отсутствует; выполнение этого этапа не наблюдается"}else{"Состояние сохранённого артефакта; не статус текущей деятельности агента"}})).collect::<Vec<_>>();
    let tool_runs = r.pack.as_ref().map(|p| {
        p.tool_runs
            .iter()
            .map(|run| {
                let mut safe = run.clone();
                safe.stderr_summary = None;
                safe
            })
            .collect::<Vec<_>>()
    });
    Ok(json_response(r.envelope(json!({"summary":r.summary,"target":r.pack.as_ref().map(|p|&p.target),"queue_summary":r.summary.queue_summary,"coverage":r.summary.coverage,"tool_runs":tool_runs,"pipeline":pipeline,"limitations":state.snapshot().health.limitations}))))
}
async fn candidates(
    State(state): State<AppState>,
    HttpPath(id): HttpPath<String>,
    HttpQuery(q): HttpQuery<Query>,
) -> ApiResult {
    checked(&q)?;
    let r = find_review(&state, &id)?;
    if r.pack.is_none() {
        return Ok(json_response(Envelope::<Value>::absent(
            r.summary.state,
            "ReviewPack недоступен",
        )));
    }
    Ok(json_response(
        r.envelope(
            q.paginate(r.candidate_rows(&q))
                .map_err(|e| failure(StatusCode::BAD_REQUEST, e))?,
        ),
    ))
}
async fn candidate_detail(
    State(state): State<AppState>,
    HttpPath((id, candidate_id)): HttpPath<(String, String)>,
) -> ApiResult {
    if !resource_id(&candidate_id) {
        return Err(failure(
            StatusCode::BAD_REQUEST,
            "Некорректный candidate ID",
        ));
    }
    let r = find_review(&state, &id)?;
    let c = r
        .candidate_index
        .get(&candidate_id)
        .and_then(|i| r.candidates.get(*i))
        .ok_or_else(|| failure(StatusCode::NOT_FOUND, "Candidate не найден"))?;
    let unit = r
        .unit_by_candidate
        .get(&candidate_id)
        .and_then(|i| r.queue.as_ref()?.units.get(*i))
        .map(unit_summary);
    let classification = r
        .queue
        .as_ref()
        .and_then(|q| q.classifications.get(&candidate_id));
    let source_file = r.pack.as_ref().and_then(|p| {
        p.scope
            .files
            .iter()
            .find(|f| f.path == c.path || f.previous_path.as_ref() == Some(&c.path))
    });
    let deleted =
        source_file.is_some_and(|f| f.status == anki_repo::code_review::scope::FileStatus::Deleted);
    let source_snapshot = r.pack.as_ref().map(|p| {
        if deleted || c.detector == "test_removed" {
            &p.target.base_sha
        } else {
            &p.target.head_sha
        }
    });
    let findings = r.triage.as_ref().map(|t| {
        t.findings
            .iter()
            .filter(|f| f.candidate_ids.contains(&candidate_id))
            .map(finding_summary)
            .collect::<Vec<_>>()
    });
    Ok(json_response(r.envelope(json!({"candidate":c,"classification":classification,"unit":unit,"decision":r.decisions.get(&candidate_id),"findings":findings,"triage_state":r.summary.artifacts.get("triage"),"source_url":null,"source_location":{"snapshot_sha":source_snapshot,"base_sha":r.pack.as_ref().map(|p|&p.target.base_sha),"head_sha":r.pack.as_ref().map(|p|&p.target.head_sha),"file_status":source_file.map(|f|catalog::label(&f.status)),"path":c.path,"line":c.line,"state":"not_observed","limitation":"В review artifacts нет подтверждённого URL origin. Для удалённого файла и test_removed указан base; происхождение строки остальных детекторов определяется исходным evidence."}}))))
}
async fn units(
    State(state): State<AppState>,
    HttpPath(id): HttpPath<String>,
    HttpQuery(q): HttpQuery<Query>,
) -> ApiResult {
    checked(&q)?;
    let r = find_review(&state, &id)?;
    let Some(queue) = &r.queue else {
        return Ok(json_response(Envelope::<Value>::absent(
            *r.summary
                .artifacts
                .get("queue")
                .unwrap_or(&Availability::Missing),
            "ReviewQueue недоступен",
        )));
    };
    let mut rows = queue
        .units
        .iter()
        .map(unit_summary)
        .filter(|u| {
            q.matches(&format!("{} {} {}", u.id, u.detector, u.role))
                && q.priority.as_ref().is_none_or(|p| p == &u.priority)
                && q.detector.as_ref().is_none_or(|d| d == &u.detector)
                && q.role.as_ref().is_none_or(|role| role == &u.role)
                && q.unknown.is_none_or(|v| v == u.unknown)
                && q.surface.as_ref().is_none_or(|surface| {
                    u.classification["surfaces"]
                        .as_array()
                        .is_some_and(|items| items.iter().any(|s| s.as_str() == Some(surface)))
                })
        })
        .collect::<Vec<_>>();
    if q.sort.as_deref() == Some("size") {
        rows.sort_by(|a, b| {
            b.candidate_count
                .cmp(&a.candidate_count)
                .then(a.id.cmp(&b.id))
        });
    } else {
        rows.sort_by(|a, b| {
            catalog::priority_rank(Some(&a.priority))
                .cmp(&catalog::priority_rank(Some(&b.priority)))
                .then(a.id.cmp(&b.id))
        });
    }
    Ok(json_response(
        r.envelope(
            q.paginate(rows)
                .map_err(|e| failure(StatusCode::BAD_REQUEST, e))?,
        ),
    ))
}
async fn unit_detail(
    State(state): State<AppState>,
    HttpPath((id, unit_id)): HttpPath<(String, String)>,
    HttpQuery(q): HttpQuery<Query>,
) -> ApiResult {
    checked(&q)?;
    if !resource_id(&unit_id) {
        return Err(failure(StatusCode::BAD_REQUEST, "Некорректный unit ID"));
    }
    let r = find_review(&state, &id)?;
    let unit = r
        .unit_index
        .get(&unit_id)
        .and_then(|i| r.queue.as_ref()?.units.get(*i))
        .ok_or_else(|| failure(StatusCode::NOT_FOUND, "ReviewUnit не найден"))?;
    let rows = unit
        .candidate_ids()
        .iter()
        .filter_map(|id| r.candidate_index.get(id).and_then(|i| r.candidates.get(*i)))
        .map(|c| r.row(c));
    Ok(json_response(r.envelope(json!({"unit":unit_summary(unit),"signature":unit.signature,"priority_signals":unit.priority_signals,"members":q.paginate(rows).map_err(|e|failure(StatusCode::BAD_REQUEST,e))?}))))
}
async fn triage(
    State(state): State<AppState>,
    HttpPath(id): HttpPath<String>,
    HttpQuery(q): HttpQuery<Query>,
) -> ApiResult {
    checked(&q)?;
    let r = find_review(&state, &id)?;
    let Some(t) = &r.triage else {
        return Ok(json_response(Envelope::<Value>::absent(
            *r.summary
                .artifacts
                .get("triage")
                .unwrap_or(&Availability::Missing),
            "SemanticTriage отсутствует или невалиден; coverage неизвестно",
        )));
    };
    let groups=t.group_decisions.iter().map(|g|json!({"id":g.id,"disposition":g.disposition,"reason_code":g.reason_code,"explanation":g.explanation,"finding_ids":g.finding_ids,"candidate_count":g.candidate_ids.len(),"representative_candidate_ids":g.representative_candidate_ids})).filter(|g|q.matches(&g.to_string()));
    Ok(json_response(r.envelope(json!({"coverage":r.summary.coverage,"findings":q.paginate(t.findings.iter().filter(|f|q.matches(&format!("{} {}",f.title,f.description))).map(finding_summary)).map_err(|e|failure(StatusCode::BAD_REQUEST,e))?,"individual_decisions":q.paginate(t.individual_decisions.iter().filter(|d|q.matches(&format!("{} {} {} {}",d.candidate_id,catalog::label(d.disposition),catalog::label(d.reason_code),d.explanation)))).map_err(|e|failure(StatusCode::BAD_REQUEST,e))?,"group_decisions":q.paginate(groups).map_err(|e|failure(StatusCode::BAD_REQUEST,e))?}))))
}
fn finding_summary(f: &anki_repo::code_review::semantic_triage::SemanticFinding) -> Value {
    json!({"id":f.id,"severity":f.severity,"title":f.title,"description":f.description,"provenance":f.provenance,"candidate_ids":f.candidate_ids.iter().take(3).collect::<Vec<_>>(),"candidate_count":f.candidate_ids.len(),"candidate_ids_truncated":f.candidate_ids.len()>3})
}
async fn group_detail(
    State(state): State<AppState>,
    HttpPath((id, group_id)): HttpPath<(String, String)>,
    HttpQuery(q): HttpQuery<Query>,
) -> ApiResult {
    checked(&q)?;
    if !resource_id(&group_id) {
        return Err(failure(StatusCode::BAD_REQUEST, "Некорректный group ID"));
    }
    let r = find_review(&state, &id)?;
    let g = r
        .triage
        .as_ref()
        .and_then(|t| t.group_decisions.iter().find(|g| g.id == group_id))
        .ok_or_else(|| failure(StatusCode::NOT_FOUND, "GroupDecision не найден"))?;
    let rows = g
        .candidate_ids
        .iter()
        .filter_map(|id| r.candidate_index.get(id).and_then(|i| r.candidates.get(*i)))
        .map(|c| r.row(c));
    Ok(json_response(r.envelope(json!({"decision":{"id":g.id,"disposition":g.disposition,"reason_code":g.reason_code,"explanation":g.explanation,"finding_ids":g.finding_ids,"candidate_count":g.candidate_ids.len(),"representative_candidate_ids":g.representative_candidate_ids},"covered_candidates":q.paginate(rows).map_err(|e|failure(StatusCode::BAD_REQUEST,e))?}))))
}
async fn finding_detail(
    State(state): State<AppState>,
    HttpPath((id, finding_id)): HttpPath<(String, String)>,
    HttpQuery(q): HttpQuery<Query>,
) -> ApiResult {
    checked(&q)?;
    if !resource_id(&finding_id) {
        return Err(failure(StatusCode::BAD_REQUEST, "Некорректный finding ID"));
    }
    let r = find_review(&state, &id)?;
    let f = r
        .triage
        .as_ref()
        .and_then(|t| t.findings.iter().find(|f| f.id == finding_id))
        .ok_or_else(|| failure(StatusCode::NOT_FOUND, "SemanticFinding не найден"))?;
    let rows = f
        .candidate_ids
        .iter()
        .filter_map(|id| r.candidate_index.get(id).and_then(|i| r.candidates.get(*i)))
        .map(|c| r.row(c));
    Ok(json_response(r.envelope(json!({"finding":finding_summary(f),"covered_candidates":q.paginate(rows).map_err(|e|failure(StatusCode::BAD_REQUEST,e))?}))))
}
async fn report(State(state): State<AppState>, HttpPath(id): HttpPath<String>) -> ApiResult {
    let r = find_review(&state, &id)?;
    Ok(json_response(if let Some(report) = &r.report {
        r.envelope(json!({"markdown":report}))
    } else {
        let mut absent = Envelope::absent(
            *r.summary
                .artifacts
                .get("report")
                .unwrap_or(&Availability::Missing),
            "review-report.md отсутствует или недоступен",
        );
        absent.diagnostics.extend(r.diagnostics.clone());
        absent.provenance = r.provenance.clone();
        absent
    }))
}
async fn jobs(State(state): State<AppState>, HttpQuery(q): HttpQuery<Query>) -> ApiResult {
    checked(&q)?;
    let catalog = state.snapshot();
    let rows = catalog
        .jobs
        .iter()
        .filter(|j| {
            q.review_id
                .as_ref()
                .is_none_or(|id| j["review_id"].as_str() == Some(id))
                && q.matches(&format!("{} {} {}", j["id"], j["scope"], j["lifecycle"]))
        })
        .cloned();
    // Список не дублирует request/enforcement; подробности доступны отдельно.
    let rows = rows.map(|mut j| {
        if let Some(map) = j.as_object_mut() {
            map.remove("request");
            map.remove("enforcement");
            map.remove("provenance");
            map.remove("artifact_stamp");
        }
        j
    });
    Ok(json_response(Envelope::available(
        q.paginate(rows)
            .map_err(|e| failure(StatusCode::BAD_REQUEST, e))?,
    )))
}
async fn job_detail(State(state): State<AppState>, HttpPath(id): HttpPath<String>) -> ApiResult {
    if !files::hex(&id, 32) {
        return Err(failure(StatusCode::BAD_REQUEST, "Некорректный job ID"));
    }
    let catalog = state.snapshot();
    let mut job = catalog
        .jobs
        .iter()
        .find(|j| j["id"] == id)
        .cloned()
        .ok_or_else(|| failure(StatusCode::NOT_FOUND, "Execution job не найден"))?;
    if let Some(fields) = job.as_object_mut() {
        fields.remove("artifact_stamp");
    }
    let mut envelope = Envelope::available(job.clone());
    envelope.state =
        serde_json::from_value(job["state"].clone()).unwrap_or(Availability::Incomplete);
    envelope.provenance = serde_json::from_value(job["provenance"].clone()).unwrap_or_default();
    Ok(json_response(envelope))
}
async fn learning_summary(State(state): State<AppState>) -> ApiResult {
    Ok(json_response(state.snapshot().learning.clone()))
}
async fn learning_collection(
    State(state): State<AppState>,
    HttpPath(collection): HttpPath<String>,
    HttpQuery(q): HttpQuery<Query>,
) -> ApiResult {
    checked(&q)?;
    if ![
        "patterns",
        "observations",
        "feedback",
        "decisions",
        "findings",
        "recommendations",
    ]
    .contains(&collection.as_str())
    {
        return Err(failure(
            StatusCode::NOT_FOUND,
            "Неизвестная learning collection",
        ));
    }
    let c = state.snapshot();
    if collection == "recommendations" {
        let relevant = c
            .reviews
            .iter()
            .filter(|r| q.review_id.as_ref().is_none_or(|id| id == &r.summary.id))
            .collect::<Vec<_>>();
        let sources = relevant
            .iter()
            .map(|r| {
                *r.summary
                    .artifacts
                    .get("recommendations")
                    .unwrap_or(&Availability::Missing)
            })
            .collect::<Vec<_>>();
        let valid = sources
            .iter()
            .filter(|s| **s == Availability::Available)
            .count();
        let availability = if valid > 0 {
            if sources.iter().all(|s| *s == Availability::Available) {
                Availability::Available
            } else {
                Availability::Incomplete
            }
        } else if sources.contains(&Availability::Invalid) {
            Availability::Invalid
        } else if sources.contains(&Availability::Unavailable) {
            Availability::Unavailable
        } else {
            Availability::Missing
        };
        let mut envelope = if valid > 0 {
            Envelope::available(value(
                q.paginate(
                    c.learning_collections
                        .get(&collection)
                        .into_iter()
                        .flatten()
                        .filter(|v| {
                            q.review_id
                                .as_ref()
                                .is_none_or(|id| v["review_id"].as_str() == Some(id))
                                && q.matches(&v.to_string())
                        })
                        .cloned(),
                )
                .map_err(|e| failure(StatusCode::BAD_REQUEST, e))?,
            ))
        } else {
            Envelope::absent(
                availability,
                "Проверенный recommendations.json отсутствует или недоступен",
            )
        };
        envelope.state = availability;
        envelope.provenance = relevant
            .iter()
            .flat_map(|r| {
                r.provenance
                    .iter()
                    .filter(|p| p.path.ends_with("/recommendations.json"))
            })
            .cloned()
            .collect();
        envelope.diagnostics.extend(
            relevant
                .iter()
                .flat_map(|r| {
                    r.diagnostics
                        .iter()
                        .filter(|d| d.contains("recommendations.json"))
                })
                .cloned(),
        );
        return Ok(json_response(envelope));
    }
    if let Some(items) = c.learning_collections.get(&collection) {
        let mut envelope = Envelope::available(
            q.paginate(items.iter().filter(|v| q.matches(&v.to_string())).cloned())
                .map_err(|e| failure(StatusCode::BAD_REQUEST, e))?,
        );
        envelope.provenance = c.learning.provenance.clone();
        return Ok(json_response(envelope));
    }
    if c.learning.data.is_none() {
        return Ok(json_response(c.learning.clone()));
    }
    let root = state.root.clone();
    match tokio::task::spawn_blocking(move || catalog::learning_page(&root, &collection, &q)).await
    {
        Ok(Ok(result)) => Ok(json_response(result)),
        Ok(Err(error)) => Ok(json_response(Envelope::<Value>::absent(
            Availability::Unavailable,
            error,
        ))),
        Err(_) => Err(failure(
            StatusCode::SERVICE_UNAVAILABLE,
            "Чтение learning прервано",
        )),
    }
}
async fn events(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
) -> Sse<impl futures_core::Stream<Item = Result<Event, Infallible>>> {
    let mut receiver = state.events.subscribe();
    let mut stop = state.event_stop.subscribe();
    let requested = headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    let replay = state
        .replay
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let current = state.snapshot();
    let replay_possible = requested.is_some_and(|id| {
        id <= current.revision
            && replay
                .front()
                .is_some_and(|first| id >= first.revision.saturating_sub(1))
    });
    let stream = async_stream::stream! {
        let mut latest=requested.unwrap_or(0);
        if replay_possible {
            for event in replay.into_iter().filter(|event|event.revision>requested.unwrap_or(0)) {
                latest=event.revision;
                yield Ok(Event::default().event("snapshot_changed").id(event.revision.to_string()).data(serde_json::to_string(&event).unwrap_or_default()));
            }
        }else{
            latest=current.revision;
            let event=LiveEvent{revision:current.revision,observed_unix_ms:current.observed_unix_ms};
            yield Ok(Event::default().event("snapshot").id(event.revision.to_string()).data(serde_json::to_string(&event).unwrap_or_default()));
        }
        loop {
            let received=tokio::select!{_=stop.changed()=>break,result=receiver.recv()=>result};
            match received {
                Ok(event) if event.revision>latest=>{latest=event.revision;yield Ok(Event::default().event("snapshot_changed").id(event.revision.to_string()).data(serde_json::to_string(&event).unwrap_or_default()));},
                Ok(_)=>(),
                Err(broadcast::error::RecvError::Lagged(_))=>{
                    let current=state.snapshot();latest=current.revision;
                    let event=LiveEvent{revision:current.revision,observed_unix_ms:current.observed_unix_ms};
                    yield Ok(Event::default().event("snapshot").id(event.revision.to_string()).data(serde_json::to_string(&event).unwrap_or_default()));
                },
                Err(broadcast::error::RecvError::Closed)=>break,
            }
        }
    };
    Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("observatory"),
    )
}
async fn static_file(State(state): State<AppState>, request: Request) -> Response {
    let path = request.uri().path();
    if path.starts_with("/api/") {
        return failure(StatusCode::NOT_FOUND, "Неизвестный read-only маршрут").into_response();
    }
    let tail = path.trim_start_matches('/');
    let file = if tail.is_empty() || !tail.rsplit('/').next().unwrap_or("").contains('.') {
        "index.html"
    } else {
        tail
    };
    let extension = Path::new(file)
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    let mime = match extension {
        "html" => "text/html; charset=utf-8",
        "js" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        _ => {
            return failure(StatusCode::NOT_FOUND, "Тип статического файла не разрешён")
                .into_response();
        }
    };
    let relative = PathBuf::from("tools/review-observatory/web/dist").join(file);
    match state.root.read(&relative,16*1024*1024){Ok((bytes,_))=>([(header::CONTENT_TYPE,mime)],bytes).into_response(),Err(_)=>failure(StatusCode::NOT_FOUND,"Frontend не собран или файл недоступен. Выполните npm ci и npm run build в tools/review-observatory/web перед serve.").into_response()}
}
