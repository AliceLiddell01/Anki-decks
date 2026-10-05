use std::fs;
use std::io::{self, Write};
use std::sync::{Arc, Mutex};

use asset_store::browser_diagnostics::{
    BrowserItemContext, BrowserItemTimer, MAX_DIAGNOSTIC_TOKEN_BYTES, MAX_IDENTITY_BYTES,
    MAX_RUNTIME_SNAPSHOT_BYTES,
};
use asset_store::browser_runtime::{
    MAX_RUNTIME_DIAGNOSTIC_ITEMS, NetworkOutcome, PendingRequestObservation, RuntimeSnapshot,
    TrackedRequest,
};
use asset_store::hashing::sha256_hex;
use asset_store::temp_workspace::TempWorkspace;
use chromiumoxide::cdp::browser_protocol::network::ResourceType;
use serde_json::Value;
use tracing_subscriber::fmt::MakeWriter;

#[derive(Clone, Default)]
struct Buffer(Arc<Mutex<Vec<u8>>>);

impl Write for Buffer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for Buffer {
    type Writer = Self;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// Логи проходят через JSONL-файл в принадлежащем тесту temp workspace.
/// Тесты не открывают `.asset-store` и не читают `decks/**`.
fn jsonl_events(operation: impl FnOnce()) -> Vec<Value> {
    let workspace = TempWorkspace::create("browser-acquisition-diagnostics-contract")
        .expect("изолированный temp workspace создаётся");
    let buffer = Buffer::default();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .with_max_level(tracing::Level::TRACE)
        .without_time()
        .with_writer(buffer.clone())
        .finish();

    tracing::subscriber::with_default(subscriber, operation);

    let path = workspace.path().join("browser-acquisition.jsonl");
    let bytes = buffer
        .0
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    fs::write(&path, bytes).expect("synthetic JSONL записывается в temp workspace");
    let contents = fs::read_to_string(path).expect("JSONL читается из temp workspace");
    let events = contents
        .lines()
        .map(|line| serde_json::from_str(line).expect("каждая строка является JSON"))
        .collect();

    workspace.close().expect("temp workspace удаляется");
    events
}

fn fields(event: &Value) -> &Value {
    &event["fields"]
}

fn tracked_request(request_id: &str, epoch: u64, resource_type: ResourceType) -> TrackedRequest {
    TrackedRequest {
        request_id: request_id.to_owned(),
        resource_type,
        url: format!("https://private.example/{request_id}?token=secret-query"),
        epoch,
        is_top_level: false,
    }
}

fn network_outcome(
    request_id: &str,
    epoch: u64,
    failure_reason: Option<&str>,
    status_code: Option<u16>,
) -> NetworkOutcome {
    network_outcome_of_type(
        request_id,
        epoch,
        ResourceType::Image,
        failure_reason,
        status_code,
    )
}

fn network_outcome_of_type(
    request_id: &str,
    epoch: u64,
    resource_type: ResourceType,
    failure_reason: Option<&str>,
    status_code: Option<u16>,
) -> NetworkOutcome {
    NetworkOutcome {
        resource_type,
        url: Some(format!(
            "https://private.example/{request_id}?token=secret-query"
        )),
        failure_reason: failure_reason.map(str::to_owned),
        request_id: request_id.to_owned(),
        epoch,
        status_code,
        is_top_level: false,
    }
}

#[test]
fn jsonl_schema_correlates_stages_and_item_with_monotonic_durations() {
    let events = jsonl_events(|| {
        let item = BrowserItemTimer::new(
            BrowserItemContext::new("jpdb", "雨:あめ", 3, 8)
                .with_worker(2, 5)
                .with_browser_session(41),
        );
        item.stage("search_navigation").finish_success();
        item.finish_failure("browser_navigation_timeout", true, None);
    });

    assert_eq!(events.len(), 2, "этап и item дают отдельные JSONL-события");
    let stage = fields(&events[0]);
    let item = fields(&events[1]);

    for event in [stage, item] {
        assert_eq!(event["schema"], "browser_acquisition_v1");
        assert_eq!(event["schema_version"], 1);
        assert_eq!(event["provider"], "jpdb");
        assert_eq!(event["identity"], "雨:あめ");
        assert_eq!(event["attempt"], 3);
        assert_eq!(event["generation"], 8);
        assert_eq!(event["worker"], 2);
        assert_eq!(event["worker_session"], 5);
        assert_eq!(event["browser_session"], 41);
        assert!(event["stage_duration_ms"].as_u64().is_some());
        assert!(event["item_duration_ms"].as_u64().is_some());
    }

    assert_eq!(stage["event"], "browser_acquisition_stage");
    assert_eq!(stage["stage"], "search_navigation");
    assert_eq!(stage["outcome"], "success");
    assert_eq!(item["event"], "browser_acquisition_item");
    assert_eq!(item["stage"], "item");
    assert_eq!(item["outcome"], "failure");
    assert_eq!(item["failure_code"], "browser_navigation_timeout");
    assert_eq!(item["retryable"], true);

    let stage_duration = stage["stage_duration_ms"].as_u64().unwrap();
    let stage_item_duration = stage["item_duration_ms"].as_u64().unwrap();
    let item_duration = item["item_duration_ms"].as_u64().unwrap();
    assert!(stage_duration <= stage_item_duration);
    assert!(stage_item_duration <= item_duration);
}

#[test]
fn unavailable_generation_stays_null_and_owner_generation_is_preserved() {
    let events = jsonl_events(|| {
        BrowserItemTimer::new(BrowserItemContext::without_generation(
            "yarxi-suu-browser",
            "漢",
            2,
        ))
        .finish_success();
        BrowserItemTimer::new(BrowserItemContext::new("yarxi-suu-browser", "語", 1, 7))
            .finish_success();
    });

    assert_eq!(fields(&events[0])["generation"], Value::Null);
    assert_eq!(fields(&events[0])["identity"], "漢");
    assert_eq!(fields(&events[1])["generation"], 7);
    assert_eq!(fields(&events[1])["identity"], "語");
}

#[test]
fn snapshot_diagnostic_separates_current_and_stale_epoch_entries() {
    let mut stale_pending = tracked_request("stale-pending", 5, ResourceType::Fetch);
    stale_pending.is_top_level = true;
    let snapshot = RuntimeSnapshot {
        epoch: 6,
        pending_observations: vec![
            PendingRequestObservation {
                request_id: "bootstrap-pending".into(),
                observed_ms: 17,
            },
            PendingRequestObservation {
                request_id: "current-pending".into(),
                observed_ms: 29,
            },
            PendingRequestObservation {
                request_id: "stale-pending".into(),
                observed_ms: 41,
            },
        ],
        pending_requests: vec![
            tracked_request("bootstrap-pending", 0, ResourceType::Document),
            tracked_request("current-pending", 6, ResourceType::Image),
            stale_pending,
        ],
        network_failures: vec![
            network_outcome_of_type(
                "bootstrap-network",
                0,
                ResourceType::Document,
                Some("NET::ERR_CONNECTION_RESET"),
                None,
            ),
            network_outcome_of_type(
                "current-network",
                6,
                ResourceType::Image,
                Some("Authorization: Bearer secret-header"),
                None,
            ),
            network_outcome_of_type(
                "stale-network",
                5,
                ResourceType::Fetch,
                Some("NET::ERR_TIMED_OUT"),
                None,
            ),
        ],
        http_errors: vec![
            network_outcome_of_type("bootstrap-http", 0, ResourceType::Document, None, Some(503)),
            network_outcome_of_type("current-http", 6, ResourceType::Image, None, Some(404)),
            network_outcome_of_type("stale-http", 5, ResourceType::Fetch, None, Some(500)),
        ],
        javascript_exceptions: 9,
        monitor_failed: false,
        current_javascript_exceptions: 2,
        bootstrap_javascript_exceptions: 1,
        stale_javascript_exceptions: 6,
        ..RuntimeSnapshot::default()
    };

    let diagnostic = snapshot.diagnostic();
    let serialized = serde_json::to_value(&diagnostic).expect("diagnostic сериализуется");

    assert_eq!(serialized["epoch"], 6);
    assert_eq!(serialized["pending_request_count"], 2);
    assert_eq!(serialized["network_failure_count"], 2);
    assert_eq!(serialized["http_error_count"], 2);
    assert_eq!(serialized["javascript_exceptions"], 9);
    assert_eq!(serialized["current_javascript_exceptions"], 2);
    assert_eq!(serialized["bootstrap_javascript_exceptions"], 1);
    assert_eq!(serialized["stale_javascript_exceptions"], 6);
    assert_eq!(serialized["pending_requests"].as_array().unwrap().len(), 2);
    assert_eq!(serialized["network_failures"].as_array().unwrap().len(), 2);
    assert_eq!(serialized["http_errors"].as_array().unwrap().len(), 2);
    assert_eq!(serialized["stale_pending_request_count"], 1);
    assert_eq!(serialized["stale_network_failure_count"], 1);
    assert_eq!(serialized["stale_http_error_count"], 1);

    let bootstrap_pending = &serialized["pending_requests"][0];
    assert_eq!(bootstrap_pending["epoch"], 0);
    assert_eq!(bootstrap_pending["in_current_epoch"], false);
    assert_eq!(bootstrap_pending["observed_ms"], 17);
    let current_pending = &serialized["pending_requests"][1];
    assert_eq!(current_pending["epoch"], 6);
    assert_eq!(current_pending["in_current_epoch"], true);
    assert_eq!(current_pending["observed_ms"], 29);
    assert_eq!(
        bootstrap_pending["resource_type"],
        serde_json::to_value(ResourceType::Document).unwrap()
    );
    assert_eq!(bootstrap_pending["is_top_level"], false);
    assert_eq!(
        current_pending["resource_type"],
        serde_json::to_value(ResourceType::Image).unwrap()
    );
    assert_eq!(current_pending["is_top_level"], false);

    let stale_pending = &serialized["stale_pending_requests"][0];
    assert_eq!(stale_pending["request_key"], sha256_hex("stale-pending"));
    assert_eq!(stale_pending["epoch"], 5);
    assert_eq!(stale_pending["in_current_epoch"], false);
    assert_eq!(
        stale_pending["resource_type"],
        serde_json::to_value(ResourceType::Fetch).unwrap()
    );
    assert_eq!(stale_pending["is_top_level"], true);
    assert_eq!(stale_pending["observed_ms"], 41);

    let network = serialized["network_failures"].as_array().unwrap();
    assert_eq!(network[0]["failure_category"], "net::ERR_CONNECTION_RESET");
    assert_eq!(
        network[1]["failure_category"],
        "не классифицированная сетевая ошибка"
    );
    assert_eq!(network[0]["request_key"], sha256_hex("bootstrap-network"));
    assert_eq!(network[1]["request_key"], sha256_hex("current-network"));
    assert_eq!(network[0]["epoch"], 0);
    assert_eq!(network[0]["in_current_epoch"], false);
    assert_eq!(
        network[0]["resource_type"],
        serde_json::to_value(ResourceType::Document).unwrap()
    );
    assert_eq!(network[1]["epoch"], 6);
    assert_eq!(network[1]["in_current_epoch"], true);
    assert_eq!(
        network[1]["resource_type"],
        serde_json::to_value(ResourceType::Image).unwrap()
    );

    let http = serialized["http_errors"].as_array().unwrap();
    assert_eq!(http[0]["request_key"], sha256_hex("bootstrap-http"));
    assert_eq!(http[0]["epoch"], 0);
    assert_eq!(http[0]["in_current_epoch"], false);
    assert_eq!(
        http[0]["resource_type"],
        serde_json::to_value(ResourceType::Document).unwrap()
    );
    assert_eq!(http[1]["request_key"], sha256_hex("current-http"));
    assert_eq!(http[1]["epoch"], 6);
    assert_eq!(http[1]["in_current_epoch"], true);
    assert_eq!(
        http[1]["resource_type"],
        serde_json::to_value(ResourceType::Image).unwrap()
    );

    let stale_network = &serialized["stale_network_failures"][0];
    assert_eq!(stale_network["request_key"], sha256_hex("stale-network"));
    assert_eq!(stale_network["epoch"], 5);
    assert_eq!(stale_network["in_current_epoch"], false);
    assert_eq!(
        stale_network["resource_type"],
        serde_json::to_value(ResourceType::Fetch).unwrap()
    );
    assert_eq!(stale_network["failure_category"], "net::ERR_TIMED_OUT");

    let stale_http = &serialized["stale_http_errors"][0];
    assert_eq!(stale_http["request_key"], sha256_hex("stale-http"));
    assert_eq!(stale_http["epoch"], 5);
    assert_eq!(stale_http["in_current_epoch"], false);
    assert_eq!(
        stale_http["resource_type"],
        serde_json::to_value(ResourceType::Fetch).unwrap()
    );
    assert_eq!(stale_http["status_code"], 500);

    let diagnostic_json = serde_json::to_string(&diagnostic).unwrap();
    for secret in [
        "private.example",
        "secret-query",
        "secret-header",
        "stale-pending",
        "stale-network",
        "stale-http",
    ] {
        assert!(
            !diagnostic_json.contains(secret),
            "runtime snapshot exposed a test fixture value"
        );
    }
    assert!(diagnostic_json.contains(&sha256_hex("bootstrap-network")));
}

#[test]
fn bounded_snapshot_and_identity_are_sanitized_in_serialized_jsonl() {
    let oversized_snapshot = RuntimeSnapshot {
        epoch: 19,
        pending_observations: (0..1000)
            .map(|index| PendingRequestObservation {
                request_id: format!("pending-secret-{index}"),
                observed_ms: index,
            })
            .collect(),
        pending_requests: (0..1000)
            .map(|index| {
                tracked_request(&format!("pending-secret-{index}"), 19, ResourceType::Fetch)
            })
            .collect(),
        network_failures: (0..1000)
            .map(|index| {
                network_outcome(
                    &format!("network-secret-{index}"),
                    19,
                    Some(&format!("Authorization: Bearer secret-{index}")),
                    None,
                )
            })
            .collect(),
        http_errors: (0..1000)
            .map(|index| network_outcome(&format!("http-secret-{index}"), 19, None, Some(503)))
            .collect(),
        javascript_exceptions: 7,
        monitor_failed: true,
        ..RuntimeSnapshot::default()
    };
    let long_identity = "雨".repeat(200);
    let long_provider = "p".repeat(MAX_DIAGNOSTIC_TOKEN_BYTES + 25);
    let long_stage = "s".repeat(MAX_DIAGNOSTIC_TOKEN_BYTES + 25);

    let events = jsonl_events(|| {
        let item = BrowserItemTimer::new(BrowserItemContext::new(
            &long_provider,
            &long_identity,
            1,
            0,
        ));
        item.stage(&long_stage).finish_success();
        item.finish_failure(
            "browser_network_runtime_failure",
            true,
            Some(&oversized_snapshot),
        );
    });

    assert_eq!(events.len(), 2);
    let stage = fields(&events[0]);
    assert_eq!(
        stage["provider"].as_str().unwrap().len(),
        MAX_DIAGNOSTIC_TOKEN_BYTES
    );
    assert_eq!(
        stage["stage"].as_str().unwrap().len(),
        MAX_DIAGNOSTIC_TOKEN_BYTES
    );

    let item = fields(&events[1]);
    let identity = item["identity"].as_str().unwrap();
    assert!(identity.len() <= MAX_IDENTITY_BYTES);
    assert!(identity.chars().all(|character| character == '雨'));

    let runtime_snapshot = item["runtime_snapshot"].as_str().unwrap();
    assert!(runtime_snapshot.len() <= MAX_RUNTIME_SNAPSHOT_BYTES);
    let diagnostic: Value = serde_json::from_str(runtime_snapshot)
        .expect("runtime snapshot остаётся валидным JSON внутри JSONL");
    assert_eq!(diagnostic["pending_request_count"], 1000);
    assert_eq!(diagnostic["network_failure_count"], 1000);
    assert_eq!(diagnostic["http_error_count"], 1000);
    assert_eq!(
        diagnostic["pending_requests"].as_array().unwrap().len(),
        MAX_RUNTIME_DIAGNOSTIC_ITEMS
    );
    assert_eq!(
        diagnostic["network_failures"].as_array().unwrap().len(),
        MAX_RUNTIME_DIAGNOSTIC_ITEMS
    );
    assert_eq!(
        diagnostic["http_errors"].as_array().unwrap().len(),
        MAX_RUNTIME_DIAGNOSTIC_ITEMS
    );

    let entire_log = events[0].to_string() + &events[1].to_string();
    for secret in [
        "private.example",
        "secret-query",
        "Authorization",
        "Bearer",
        "pending-secret-",
        "network-secret-",
        "http-secret-",
    ] {
        assert!(
            !entire_log.contains(secret),
            "diagnostic JSONL exposed a test fixture value"
        );
    }
}

#[test]
fn concurrent_item_contexts_keep_browser_session_evidence_isolated() {
    let snapshot_a = RuntimeSnapshot {
        epoch: 3,
        network_failures: vec![network_outcome(
            "session-a-private-request",
            3,
            Some("NET::ERR_CONNECTION_RESET"),
            None,
        )],
        ..RuntimeSnapshot::default()
    };
    let snapshot_b = RuntimeSnapshot {
        epoch: 9,
        http_errors: vec![
            network_outcome("session-b-private-one", 9, None, Some(503)),
            network_outcome("session-b-private-two", 9, None, Some(502)),
        ],
        ..RuntimeSnapshot::default()
    };

    let events = jsonl_events(|| {
        let item_a = BrowserItemTimer::new(
            BrowserItemContext::new("jpdb", "item-a", 1, 0).with_browser_session(71),
        );
        let item_b = BrowserItemTimer::new(
            BrowserItemContext::new("yarxi", "item-b", 2, 4).with_browser_session(82),
        );
        item_a.record_runtime_snapshot(&snapshot_a);
        item_b.record_runtime_snapshot(&snapshot_b);
        item_b.finish_failure("browser_http_failure", true, None);
        item_a.finish_failure("browser_network_failure", true, None);
    });

    assert_eq!(events.len(), 2);
    let item_b = fields(&events[0]);
    let evidence_b: Value = serde_json::from_str(item_b["runtime_snapshot"].as_str().unwrap())
        .expect("evidence сеанса B сериализована");
    assert_eq!(item_b["browser_session"], 82);
    assert_eq!(item_b["identity"], "item-b");
    assert_eq!(evidence_b["epoch"], 9);
    assert_eq!(evidence_b["network_failure_count"], 0);
    assert_eq!(evidence_b["http_error_count"], 2);
    assert_eq!(
        evidence_b["http_errors"][0]["request_key"],
        sha256_hex("session-b-private-one")
    );
    assert_eq!(
        evidence_b["http_errors"][1]["request_key"],
        sha256_hex("session-b-private-two")
    );

    let item_a = fields(&events[1]);
    let evidence_a: Value = serde_json::from_str(item_a["runtime_snapshot"].as_str().unwrap())
        .expect("evidence сеанса A сериализована");
    assert_eq!(item_a["browser_session"], 71);
    assert_eq!(item_a["identity"], "item-a");
    assert_eq!(evidence_a["epoch"], 3);
    assert_eq!(evidence_a["network_failure_count"], 1);
    assert_eq!(evidence_a["http_error_count"], 0);
    assert_eq!(
        evidence_a["network_failures"][0]["request_key"],
        sha256_hex("session-a-private-request")
    );

    let serialized_events = events
        .iter()
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("");
    for other_session_request in [
        ("item-a", "session-b-private-one"),
        ("item-a", "session-b-private-two"),
        ("item-b", "session-a-private-request"),
    ] {
        let event = events
            .iter()
            .find(|event| fields(event)["identity"] == other_session_request.0)
            .unwrap();
        assert!(
            !event
                .to_string()
                .contains(&sha256_hex(other_session_request.1))
        );
    }
    assert!(!serialized_events.contains("private.example"));
}
