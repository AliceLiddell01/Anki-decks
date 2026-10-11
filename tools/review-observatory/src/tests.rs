//! Синтетические HTTP-проверки не читают фактические данные репозитория.
use super::*;
use anki_repo::code_review::{
    model::{CandidateEvidence, CandidateOrigin, ReviewPack},
    review_queue, semantic_triage,
};
use axum::{body::Body, http::Request};
use http_body_util::BodyExt;
use rusqlite::Connection;
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, fs, path::PathBuf};
use tower::ServiceExt;

struct Fixture {
    root: PathBuf,
}
struct RestorePermissions(Vec<(PathBuf, fs::Permissions)>);
impl RestorePermissions {
    fn capture(paths: impl IntoIterator<Item = PathBuf>) -> Self {
        Self(
            paths
                .into_iter()
                .map(|path| (path.clone(), fs::metadata(path).unwrap().permissions()))
                .collect(),
        )
    }
}
impl Drop for RestorePermissions {
    fn drop(&mut self) {
        for (path, permissions) in &self.0 {
            let _ = fs::set_permissions(path, permissions.clone());
        }
    }
}
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir()
            .join(format!(
                "review-observatory-{}-{}",
                std::process::id(),
                catalog::now_ms()
            ))
            .join(format!("{:x}", randless()));
        fs::create_dir_all(&root).unwrap();
        Self { root }
    }
    fn source(&self, count: usize) -> PathBuf {
        let head = "a".repeat(40);
        let dir = self.root.join(".anki-repo/review/local").join(&head);
        fs::create_dir_all(&dir).unwrap();
        let mut pack:ReviewPack=serde_json::from_value(json!({"schema_version":1,"target":{"repository_id":"r","base_sha":"b".repeat(40),"head_sha":head,"merge_base_sha":"b".repeat(40)},"scope":{"merge_base_sha":"b".repeat(40),"text_image_limit_bytes":10000,"files":[]},"diagnostics":[],"candidates":[],"language":{"schema_version":1,"files":[],"candidates":[],"skipped":[]},"dependencies":[],"tests":[],"suppressions":[],"risk_surfaces":[],"tool_runs":[]})).unwrap();
        pack.candidates = (0..count)
            .map(|i| CandidateEvidence {
                id: format!("language:candidate-{i:06}"),
                detector: if i % 2 == 0 {
                    "error_path"
                } else {
                    "dependency_change"
                }
                .into(),
                path: format!("src/module-{i}.rs"),
                line: Some(i + 1),
                column: None,
                snippet: Some("exact evidence".into()),
                origin: CandidateOrigin::Unknown,
                signals: vec!["unknown".into()],
                source: "fixture".into(),
                metadata: BTreeMap::new(),
            })
            .collect();
        let bytes = serde_json::to_vec(&pack).unwrap();
        let digest = format!("{:x}", Sha256::digest(&bytes));
        fs::write(dir.join("review.json"), bytes).unwrap();
        let queue = review_queue::build(&pack, &digest, &BTreeMap::new()).unwrap();
        fs::write(
            dir.join("review-queue.json"),
            serde_json::to_vec(&queue).unwrap(),
        )
        .unwrap();
        let triage = semantic_triage::initialize(&pack, &digest);
        fs::write(
            dir.join("semantic-triage.json"),
            serde_json::to_vec(&triage).unwrap(),
        )
        .unwrap();
        dir
    }
    fn state(&self) -> AppState {
        AppState::new(&self.root, "127.0.0.1:3085".parse().unwrap()).unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}
fn randless() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos()
}
fn request(uri: &str) -> axum::http::Request<Body> {
    Request::builder()
        .uri(uri)
        .header("host", "127.0.0.1:3085")
        .body(Body::empty())
        .unwrap()
}
async fn call(state: &AppState, uri: &str) -> (StatusCode, Value, usize) {
    let response = router(state.clone()).oneshot(request(uri)).await.unwrap();
    let status = response.status();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&body).unwrap(), body.len())
}

#[tokio::test]
async fn host_origin_methods_and_bad_queries_are_denied() {
    let fixture = Fixture::new();
    let state = fixture.state();
    for (name, value) in [
        ("host", "evil.test:3085"),
        ("origin", "http://evil.test"),
        ("sec-fetch-site", "cross-site"),
    ] {
        let mut req = request("/api/v1/snapshot");
        req.headers_mut().insert(
            axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
            value.parse().unwrap(),
        );
        assert_eq!(
            router(state.clone()).oneshot(req).await.unwrap().status(),
            StatusCode::FORBIDDEN
        );
    }
    let mut req = request("/api/v1/snapshot");
    *req.method_mut() = axum::http::Method::POST;
    assert_eq!(
        router(state.clone()).oneshot(req).await.unwrap().status(),
        StatusCode::METHOD_NOT_ALLOWED
    );
    for uri in [
        "/api/v1/reviews?limit=101",
        "/api/v1/reviews?page=0",
        "/api/v1/reviews?limit=unknown",
        "/api/v1/reviews?arbitrary=foo",
    ] {
        let (status, result, _) = call(&state, uri).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(result["api_version"], 1);
    }
    assert!(!fixture.root.join(".anki-repo/learning").exists());
}

#[test]
fn file_boundary_refuses_parent_symlinks_special_files_and_oversized_reads() {
    let fixture = Fixture::new();
    fs::create_dir(fixture.root.join("artifacts")).unwrap();
    fs::write(fixture.root.join("artifacts/valid.json"), b"{}\n").unwrap();
    let safe = SafeRoot::new(&fixture.root).unwrap();
    assert!(safe.read(Path::new("../secret"), 100).is_err());
    assert!(safe.read(Path::new("/etc/passwd"), 100).is_err());
    assert!(safe.read(Path::new("artifacts/valid.json"), 1).is_err());
    assert!(safe.read(Path::new("artifacts"), 100).is_err());
    rustix::fs::mkfifoat(
        rustix::fs::CWD,
        fixture.root.join("artifacts/pipe"),
        rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
    )
    .unwrap();
    assert!(safe.read(Path::new("artifacts/pipe"), 100).is_err());
    std::os::unix::fs::symlink("valid.json", fixture.root.join("artifacts/alias.json")).unwrap();
    std::os::unix::fs::symlink("artifacts", fixture.root.join("alias")).unwrap();
    assert!(safe.read(Path::new("artifacts/alias.json"), 100).is_err());
    assert!(safe.read(Path::new("alias/valid.json"), 100).is_err());
    assert!(
        !safe
            .children_checked(Path::new("artifacts"))
            .unwrap()
            .diagnostics
            .is_empty()
    );
}

#[tokio::test]
async fn large_snapshot_pages_preserve_evidence_and_unknown_semantics() {
    let fixture = Fixture::new();
    let directory = fixture.source(6156);
    let state = fixture.state();
    let id = state.snapshot().reviews[0].summary.id.clone();
    let (status, page, len) = call(
        &state,
        &format!("/api/v1/reviews/{id}/candidates?page=2&limit=25"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(page["data"]["total"], 6156);
    assert_eq!(page["data"]["items"].as_array().unwrap().len(), 25);
    assert!(len < 50_000);
    let candidate = page["data"]["items"][0]["id"].as_str().unwrap();
    let (_, detail, len) = call(
        &state,
        &format!("/api/v1/reviews/{id}/candidates/{candidate}"),
    )
    .await;
    assert_eq!(detail["data"]["candidate"]["snippet"], "exact evidence");
    assert_eq!(detail["data"]["decision"], Value::Null);
    assert!(len < 10000);
    let (_, coverage, _) = call(&state, &format!("/api/v1/reviews/{id}/triage")).await;
    assert_eq!(coverage["data"]["coverage"]["unreviewed_candidates"], 6156);
    assert_eq!(coverage["data"]["coverage"]["reviewed_candidates"], 0);
    let (_, units, len) = call(&state, &format!("/api/v1/reviews/{id}/units?limit=25")).await;
    assert_eq!(units["data"]["items"].as_array().unwrap().len(), 25);
    assert!(len < 50000);
    fs::remove_file(directory.join("semantic-triage.json")).unwrap();
    let updated = catalog::refresh(&state.root, &state.snapshot());
    state.publish(updated);
    let (_, detail, _) = call(
        &state,
        &format!("/api/v1/reviews/{id}/candidates/{candidate}"),
    )
    .await;
    assert_eq!(detail["data"]["findings"], Value::Null);
    assert_eq!(detail["data"]["triage_state"], "missing");
}

#[tokio::test]
async fn malformed_artifact_degrades_without_stopping_other_routes() {
    let fixture = Fixture::new();
    let directory = fixture.source(2);
    fs::write(directory.join("semantic-triage.json"), b"{\"unfinished\":").unwrap();
    let state = fixture.state();
    let snapshot = state.snapshot();
    assert_eq!(
        snapshot.reviews[0].summary.artifacts["triage"],
        Availability::Invalid
    );
    let (status, page, _) = call(
        &state,
        &format!(
            "/api/v1/reviews/{}/candidates",
            snapshot.reviews[0].summary.id
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(page["data"]["total"], 2);
    assert_eq!(call(&state, "/api/v1/health").await.0, StatusCode::OK);
}

#[tokio::test]
async fn inaccessible_parent_is_unavailable_instead_of_missing() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new();
    let directory = fixture.source(2);
    let learning = fixture.root.join(".anki-repo/learning");
    fs::create_dir_all(&learning).unwrap();
    fs::write(learning.join("state.sqlite"), b"inaccessible").unwrap();
    let restore_permissions = RestorePermissions::capture([directory.clone(), learning.clone()]);
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o0)).unwrap();
    fs::set_permissions(&learning, fs::Permissions::from_mode(0o0)).unwrap();
    match fs::symlink_metadata(directory.join("review.json")) {
        Ok(_) => {
            drop(restore_permissions);
            return;
        }
        Err(error) => assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied),
    }
    let state = fixture.state();
    drop(restore_permissions);
    let snapshot = state.snapshot();
    assert_eq!(snapshot.reviews.len(), 1);
    for name in ["review", "queue", "triage", "report"] {
        assert_eq!(
            snapshot.reviews[0].summary.artifacts[name],
            Availability::Unavailable
        );
    }
    assert_eq!(snapshot.reviews[0].summary.state, Availability::Unavailable);
    assert_eq!(snapshot.learning.state, Availability::Unavailable);
    let (_, report, _) = call(
        &state,
        &format!("/api/v1/reviews/{}/report", snapshot.reviews[0].summary.id),
    )
    .await;
    assert_eq!(report["state"], "unavailable");
    assert!(!report["diagnostics"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn unsupported_learning_schema_is_incomplete() {
    let fixture = Fixture::new();
    let directory = fixture.root.join(".anki-repo/learning");
    fs::create_dir_all(&directory).unwrap();
    let database = directory.join("state.sqlite");
    let connection = Connection::open(database).unwrap();
    connection.pragma_update(None, "user_version", 99).unwrap();
    drop(connection);

    let snapshot = fixture.state().snapshot();

    assert_eq!(snapshot.learning.state, Availability::Incomplete);
}

#[tokio::test]
async fn baseline_priority_order_is_high_normal_low() {
    let fixture = Fixture::new();
    fixture.source(3);
    let state = fixture.state();
    let mut catalog = (*state.snapshot()).clone();
    let mut review = (*catalog.reviews[0]).clone();
    let units = &mut review.queue.as_mut().unwrap().units;
    units[0].priority = review_queue::ReviewPriority::Low;
    units[1].priority = review_queue::ReviewPriority::High;
    units[2].priority = review_queue::ReviewPriority::Normal;
    let id = review.summary.id.clone();
    catalog.reviews[0] = Arc::new(review);
    catalog.revision += 1;
    state.publish(catalog);
    for collection in ["candidates", "units"] {
        let (_, page, _) = call(&state, &format!("/api/v1/reviews/{id}/{collection}")).await;
        let priorities = page["data"]["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["priority"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(priorities, ["high", "normal", "low"]);
    }
}

#[tokio::test]
async fn missing_recommendations_differ_from_valid_empty_artifact() {
    let fixture = Fixture::new();
    let directory = fixture.source(0);
    let state = fixture.state();
    let (_, response, _) = call(&state, "/api/v1/learning/recommendations").await;
    assert_eq!(response["state"], "missing");
    assert_eq!(response["data"], Value::Null);
    let pack = fs::read(directory.join("review.json")).unwrap();
    let queue = fs::read(directory.join("review-queue.json")).unwrap();
    let recommendations = json!({"schema_version":anki_repo::code_review::learning::recommend::RECOMMENDATIONS_SCHEMA_VERSION,"policy_version":anki_repo::code_review::learning::LEARNING_POLICY_VERSION,"generation":{"revision":0,"trusted_reviews":0,"quarantined_reviews":0,"trusted_units":0},"repository_id":"r","base_sha":"b".repeat(40),"head_sha":"a".repeat(40),"merge_base_sha":"b".repeat(40),"workspace_variant":"root","review_pack_sha256":format!("{:x}",Sha256::digest(pack)),"queue_sha256":format!("{:x}",Sha256::digest(queue)),"input_trust":"structure_only_quarantine","learning_disabled":true,"suggested_order":[],"recommendations":[],"limitations":[]});
    fs::write(
        directory.join("recommendations.json"),
        serde_json::to_vec(&recommendations).unwrap(),
    )
    .unwrap();
    state.publish(catalog::refresh(&state.root, &state.snapshot()));
    let (_, response, _) = call(&state, "/api/v1/learning/recommendations").await;
    assert_eq!(response["state"], "available");
    assert_eq!(response["data"]["total"], 0);
    assert!(
        response["provenance"][0]["path"]
            .as_str()
            .unwrap()
            .ends_with("/recommendations.json")
    );
    fs::write(directory.join("recommendations.json"), b"{").unwrap();
    state.publish(catalog::refresh(&state.root, &state.snapshot()));
    let (_, response, _) = call(&state, "/api/v1/learning/recommendations").await;
    assert_eq!(response["state"], "invalid");
    assert_eq!(response["data"], Value::Null);
    assert_eq!(
        state.snapshot().reviews[0].summary.state,
        Availability::Incomplete
    );
}

#[tokio::test]
async fn sse_replay_is_bounded_and_resets_after_a_gap() {
    let fixture = Fixture::new();
    let state = fixture.state();
    let initial = state.snapshot().revision;
    for _ in 0..70 {
        let mut c = (*state.snapshot()).clone();
        c.revision += 1;
        state.publish(c);
    }
    assert_eq!(state.replay.lock().unwrap().len(), 64);
    let mut req = request("/api/v1/events");
    req.headers_mut()
        .insert("last-event-id", initial.to_string().parse().unwrap());
    let response = router(state.clone()).oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let mut body = response.into_body();
    let bytes = body.frame().await.unwrap().unwrap().into_data().unwrap();
    assert!(String::from_utf8_lossy(&bytes).contains("event: snapshot\n"));
    state.shutdown();
    assert!(
        tokio::time::timeout(Duration::from_secs(1), body.frame())
            .await
            .unwrap()
            .is_none()
    );
}
