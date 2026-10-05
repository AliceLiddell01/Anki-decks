//! Единая диагностика времени получения элемента в браузере.
//!
//! События `browser_acquisition_item` и `browser_acquisition_stage` имеют схему
//! `browser_acquisition_v1`. Время измеряется только монотонными `Instant`.
//! Контекст, причины и данные времени выполнения ограничиваются до сериализации;
//! произвольные сообщения и сырые данные CDP этот API не принимает.

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;

use tracing::Dispatch;

use crate::browser_runtime::RuntimeSnapshot;

pub const MAX_IDENTITY_BYTES: usize = 256;
pub const MAX_DIAGNOSTIC_TOKEN_BYTES: usize = 96;
pub const MAX_RUNTIME_SNAPSHOT_BYTES: usize = 32 * 1024;

/// Ограниченный контекст одного элемента. Поля закрыты, чтобы вызывающий код не мог
/// случайно принять URL или произвольный текст вместо идентичности/кода.
#[derive(Debug, Clone)]
pub struct BrowserItemContext {
    provider: String,
    identity: String,
    attempt: u64,
    generation: Option<u64>,
    worker: Option<u64>,
    worker_session: Option<u64>,
    browser_session: Option<u64>,
}

impl BrowserItemContext {
    pub fn new(provider: &str, identity: &str, attempt: u64, generation: u64) -> Self {
        Self::with_optional_generation(provider, identity, attempt, Some(generation))
    }

    /// Для провайдерских запусков без поколения владельца. Не подменяем неизвестное
    /// состояние фиктивным числом `0`.
    pub fn without_generation(provider: &str, identity: &str, attempt: u64) -> Self {
        Self::with_optional_generation(provider, identity, attempt, None)
    }

    fn with_optional_generation(
        provider: &str,
        identity: &str,
        attempt: u64,
        generation: Option<u64>,
    ) -> Self {
        Self {
            provider: safe_token(provider),
            identity: safe_identity(identity),
            attempt,
            generation,
            worker: None,
            worker_session: None,
            browser_session: None,
        }
    }

    pub fn with_worker(mut self, worker: u64, worker_session: u64) -> Self {
        self.worker = Some(worker);
        self.worker_session = Some(worker_session);
        self
    }

    pub fn with_browser_session(mut self, session: u64) -> Self {
        self.browser_session = Some(session);
        self
    }
}

#[derive(Debug)]
struct ItemState {
    context: BrowserItemContext,
    runtime_snapshot: Option<String>,
    stop_reason: Option<String>,
    interruption_code: Option<String>,
    interruption_reason: Option<String>,
    interruption_retryable: bool,
}

/// Владелец времени всего элемента, включая повторы и паузы перед ними.
/// Drop незавершённого таймера фиксирует прерывание Future.
#[must_use = "таймер должен жить до завершения элемента"]
pub struct BrowserItemTimer {
    state: Arc<Mutex<ItemState>>,
    started: Instant,
    dispatch: Dispatch,
    completed: bool,
}

impl BrowserItemTimer {
    pub fn new(context: BrowserItemContext) -> Self {
        Self {
            state: Arc::new(Mutex::new(ItemState {
                context,
                runtime_snapshot: None,
                stop_reason: None,
                interruption_code: None,
                interruption_reason: None,
                interruption_retryable: false,
            })),
            started: Instant::now(),
            dispatch: tracing::dispatcher::get_default(Clone::clone),
            completed: false,
        }
    }

    /// Этап владеет собственным защитным объектом и может пережить временное заимствование элемента.
    pub fn stage(&self, stage: &str) -> BrowserStageTimer {
        BrowserStageTimer {
            state: Arc::clone(&self.state),
            item_started: self.started,
            started: Instant::now(),
            stage: safe_token(stage),
            dispatch: self.dispatch.clone(),
            completed: false,
        }
    }

    pub fn set_attempt(&self, attempt: u64) {
        let mut state = lock(&self.state);
        if state.context.attempt != attempt {
            state.stop_reason = None;
            state.interruption_code = None;
            state.interruption_reason = None;
            state.interruption_retryable = false;
        }
        state.context.attempt = attempt;
    }

    pub fn set_browser_session(&self, session: u64) {
        lock(&self.state).context.browser_session = Some(session);
    }

    pub fn set_stop_reason(&self, reason: &str) {
        lock(&self.state).stop_reason = Some(safe_token(reason));
    }

    /// Координатор задаёт причину перед Drop отменяемого future: активный этап
    /// получает точный код тайм-аута или отмены вместе с последним снимком.
    pub fn set_interruption(&self, reason: &str, failure_code: &str, retryable: bool) {
        let mut state = lock(&self.state);
        state.interruption_reason = Some(safe_token(reason));
        state.interruption_code = Some(safe_token(failure_code));
        state.interruption_retryable = retryable;
    }

    /// Все этапы видят последние безопасные данные, в том числе при Drop.
    pub fn record_runtime_snapshot(&self, snapshot: &RuntimeSnapshot) {
        record_snapshot(&self.state, snapshot);
    }

    pub fn finish_success(self) {
        self.finish_outcome("success");
    }

    pub fn finish_outcome(mut self, outcome: &str) {
        self.emit(outcome, None, false);
        self.completed = true;
    }

    pub fn finish_failure(
        mut self,
        code: &str,
        retryable: bool,
        snapshot: Option<&RuntimeSnapshot>,
    ) {
        if let Some(snapshot) = snapshot {
            self.record_runtime_snapshot(snapshot);
        }
        self.emit("failure", Some(code), retryable);
        self.completed = true;
    }

    pub fn interrupt(mut self, reason: &str, snapshot: Option<&RuntimeSnapshot>) {
        self.set_stop_reason(reason);
        if let Some(snapshot) = snapshot {
            self.record_runtime_snapshot(snapshot);
        }
        let (code, retryable) = interruption_code(&self.state, "browser_item_interrupted");
        self.emit("interrupted", Some(&code), retryable);
        self.completed = true;
    }

    fn emit(&self, outcome: &str, failure: Option<&str>, retryable: bool) {
        emit(Event {
            event: "browser_acquisition_item",
            stage: "item",
            state: &self.state,
            dispatch: &self.dispatch,
            started: self.started,
            item_started: self.started,
            outcome,
            failure,
            retryable,
        });
    }
}

impl Drop for BrowserItemTimer {
    fn drop(&mut self) {
        if !self.completed {
            default_stop_reason(&self.state);
            let (code, retryable) = interruption_code(&self.state, "browser_item_interrupted");
            self.emit("interrupted", Some(&code), retryable);
        }
    }
}

/// Этап фиксируется ровно один раз, включая отмену во время `.await`.
#[must_use = "таймер должен жить до завершения этапа"]
pub struct BrowserStageTimer {
    state: Arc<Mutex<ItemState>>,
    item_started: Instant,
    started: Instant,
    stage: String,
    dispatch: Dispatch,
    completed: bool,
}

impl BrowserStageTimer {
    pub fn record_runtime_snapshot(&self, snapshot: &RuntimeSnapshot) {
        record_snapshot(&self.state, snapshot);
    }

    pub fn finish_success(self) {
        self.finish_outcome("success");
    }

    pub fn finish_outcome(mut self, outcome: &str) {
        self.emit(outcome, None, false);
        self.completed = true;
    }

    pub fn finish_failure(
        mut self,
        code: &str,
        retryable: bool,
        snapshot: Option<&RuntimeSnapshot>,
    ) {
        if let Some(snapshot) = snapshot {
            self.record_runtime_snapshot(snapshot);
        }
        self.emit("failure", Some(code), retryable);
        self.completed = true;
    }

    pub fn interrupt(mut self, reason: &str, snapshot: Option<&RuntimeSnapshot>) {
        lock(&self.state).stop_reason = Some(safe_token(reason));
        if let Some(snapshot) = snapshot {
            self.record_runtime_snapshot(snapshot);
        }
        let (code, retryable) = interruption_code(&self.state, "browser_stage_interrupted");
        self.emit("interrupted", Some(&code), retryable);
        self.completed = true;
    }

    fn emit(&self, outcome: &str, failure: Option<&str>, retryable: bool) {
        emit(Event {
            event: "browser_acquisition_stage",
            stage: &self.stage,
            state: &self.state,
            dispatch: &self.dispatch,
            started: self.started,
            item_started: self.item_started,
            outcome,
            failure,
            retryable,
        });
    }
}

impl Drop for BrowserStageTimer {
    fn drop(&mut self) {
        if !self.completed {
            default_stop_reason(&self.state);
            let (code, retryable) = interruption_code(&self.state, "browser_stage_interrupted");
            self.emit("interrupted", Some(&code), retryable);
        }
    }
}

fn lock(state: &Mutex<ItemState>) -> MutexGuard<'_, ItemState> {
    state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn default_stop_reason(state: &Mutex<ItemState>) {
    let mut state = lock(state);
    if state.stop_reason.is_none() {
        state.stop_reason = Some(
            state
                .interruption_reason
                .clone()
                .unwrap_or_else(|| "future_dropped".to_owned()),
        );
    }
}

fn interruption_code(state: &Mutex<ItemState>, default: &str) -> (String, bool) {
    let state = lock(state);
    (
        state
            .interruption_code
            .clone()
            .unwrap_or_else(|| default.to_owned()),
        state.interruption_retryable,
    )
}

fn record_snapshot(state: &Mutex<ItemState>, snapshot: &RuntimeSnapshot) {
    let serialized = runtime_snapshot_json(snapshot);
    lock(state).runtime_snapshot = Some(serialized);
}

/// Безопасные ограниченные данные JSON для отказа сеанса до запуска первого элемента.
/// Содержит только диагностическую проекцию без сырых URL, заголовков и сообщений CDP.
pub fn runtime_snapshot_json(snapshot: &RuntimeSnapshot) -> String {
    // Сырой RuntimeSnapshot никогда не проходит через Serialize.
    let diagnostic = snapshot.diagnostic();
    let serialized = serde_json::to_string(&diagnostic)
        .unwrap_or_else(|_| r#"{"serialization_failed":true}"#.to_owned());
    if serialized.len() <= MAX_RUNTIME_SNAPSHOT_BYTES {
        serialized
    } else {
        serde_json::json!({
            "truncated": true,
            "epoch": diagnostic.epoch,
            "pending_request_count": diagnostic.pending_request_count,
            "network_failure_count": diagnostic.network_failure_count,
            "http_error_count": diagnostic.http_error_count,
            "stale_pending_request_count": diagnostic.stale_pending_request_count,
            "stale_network_failure_count": diagnostic.stale_network_failure_count,
            "stale_http_error_count": diagnostic.stale_http_error_count,
            "javascript_exceptions": snapshot.javascript_exceptions,
            "current_javascript_exceptions": snapshot.current_javascript_exceptions,
            "bootstrap_javascript_exceptions": snapshot.bootstrap_javascript_exceptions,
            "stale_javascript_exceptions": snapshot.stale_javascript_exceptions,
            "monitor_failed": snapshot.monitor_failed,
        })
        .to_string()
    }
}

struct Event<'a> {
    event: &'static str,
    stage: &'a str,
    state: &'a Mutex<ItemState>,
    dispatch: &'a Dispatch,
    started: Instant,
    item_started: Instant,
    outcome: &'a str,
    failure: Option<&'a str>,
    retryable: bool,
}

fn emit(event: Event<'_>) {
    let state = lock(event.state);
    let context = &state.context;
    let now = Instant::now();
    let stage_duration_ms = millis(now.saturating_duration_since(event.started));
    let item_duration_ms = millis(now.saturating_duration_since(event.item_started));
    let failure_code = event.failure.map(safe_token);
    let outcome = safe_token(event.outcome);
    let runtime_snapshot = (event.failure.is_some() || state.stop_reason.is_some())
        .then_some(state.runtime_snapshot.as_deref())
        .flatten();
    // Уровень Debug сохраняет эти подробности в JSONL, не расширяя обычный вывод прогресса.
    tracing::dispatcher::with_default(event.dispatch, || {
        tracing::debug!(
            event = event.event,
            schema = "browser_acquisition_v1",
            schema_version = 1u64,
            provider = context.provider.as_str(),
            identity = context.identity.as_str(),
            attempt = context.attempt,
            generation = context.generation,
            worker = context.worker,
            worker_session = context.worker_session,
            browser_session = context.browser_session,
            stage = event.stage,
            stage_duration_ms,
            item_duration_ms,
            outcome = outcome.as_str(),
            failure_code = failure_code.as_deref(),
            retryable = event.retryable,
            stop_reason = state.stop_reason.as_deref(),
            runtime_snapshot,
            "Время получения элемента браузером"
        );
    });
}

fn millis(duration: std::time::Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn safe_token(value: &str) -> String {
    if value.is_empty()
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
    {
        return "redacted".to_owned();
    }
    value.chars().take(MAX_DIAGNOSTIC_TOKEN_BYTES).collect()
}

fn safe_identity(value: &str) -> String {
    let mut end = 0;
    for (index, character) in value.char_indices() {
        let next = index + character.len_utf8();
        if next > MAX_IDENTITY_BYTES {
            break;
        }
        end = next;
    }
    let prefix = &value[..end];
    if prefix.is_empty()
        || contains_sensitive_identity_marker(prefix)
        || !prefix.chars().all(|character| {
            character.is_alphanumeric()
                || matches!(
                    character,
                    ' ' | '_'
                        | '-'
                        | '.'
                        | ':'
                        | '\''
                        | '’'
                        | '・'
                        | '〜'
                        | '～'
                        | '。'
                        | '、'
                        | '「'
                        | '」'
                        | '『'
                        | '』'
                        | '（'
                        | '）'
                )
        })
    {
        return "redacted".to_owned();
    }
    prefix.to_owned()
}

fn contains_sensitive_identity_marker(value: &str) -> bool {
    const MARKERS: &[&[u8]] = &[
        b"authorization:",
        b"authorization ",
        b"proxy-authorization:",
        b"cookie:",
        b"set-cookie:",
        b"bearer ",
        b"http:",
        b"https:",
        b"javascript:",
        b"data:",
        b"token:",
        b"access_token:",
        b"password:",
        b"api_key:",
        b"api-key:",
    ];
    let bytes = value.as_bytes();
    MARKERS.iter().any(|marker| {
        bytes
            .windows(marker.len())
            .any(|window| window.eq_ignore_ascii_case(marker))
    })
}

#[cfg(test)]
mod tests {
    use std::io::{self, Write};

    use chromiumoxide::cdp::browser_protocol::network::ResourceType;
    use tracing_subscriber::fmt::MakeWriter;

    use super::*;
    use crate::browser_runtime::NetworkOutcome;

    #[derive(Clone, Default)]
    struct Buffer(Arc<Mutex<Vec<u8>>>);

    impl Write for Buffer {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
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

    fn events(operation: impl FnOnce()) -> Vec<serde_json::Value> {
        let buffer = Buffer::default();
        let subscriber = tracing_subscriber::fmt()
            .json()
            .with_max_level(tracing::Level::TRACE)
            .without_time()
            .with_writer(buffer.clone())
            .finish();
        tracing::subscriber::with_default(subscriber, operation);
        let bytes = buffer.0.lock().unwrap().clone();
        String::from_utf8(bytes)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    #[test]
    fn schema_correlates_item_and_stage_with_monotonic_durations() {
        let events = events(|| {
            let item = BrowserItemTimer::new(
                BrowserItemContext::new("jpdb", "雨", 2, 3)
                    .with_worker(4, 5)
                    .with_browser_session(6),
            );
            item.stage("navigation").finish_success();
            item.finish_outcome("acquired");
        });
        assert_eq!(events.len(), 2);
        let stage = &events[0]["fields"];
        let item = &events[1]["fields"];
        for event in [stage, item] {
            assert_eq!(event["schema"], "browser_acquisition_v1");
            assert_eq!(event["provider"], "jpdb");
            assert_eq!(event["identity"], "雨");
            assert_eq!(event["attempt"], 2);
            assert_eq!(event["generation"], 3);
            assert_eq!(event["worker"], 4);
            assert_eq!(event["worker_session"], 5);
            assert_eq!(event["browser_session"], 6);
            assert!(!event["retryable"].as_bool().unwrap());
            assert!(event["stage_duration_ms"].as_u64().is_some());
            assert!(event["item_duration_ms"].as_u64().is_some());
        }
        assert_eq!(stage["event"], "browser_acquisition_stage");
        assert_eq!(stage["stage"], "navigation");
        assert_eq!(item["event"], "browser_acquisition_item");
        assert_eq!(item["outcome"], "acquired");
        assert!(stage["stage_duration_ms"].as_u64() <= stage["item_duration_ms"].as_u64());
        assert!(stage["item_duration_ms"].as_u64() <= item["item_duration_ms"].as_u64());
    }

    #[test]
    fn interrupted_stage_and_item_share_safe_latest_failure_evidence() {
        let events = events(|| {
            let item = BrowserItemTimer::new(BrowserItemContext::new("yarxi", "雨", 1, 0));
            let stage = item.stage("media_readiness");
            item.record_runtime_snapshot(&RuntimeSnapshot {
                epoch: 7,
                network_failures: vec![NetworkOutcome {
                    resource_type: ResourceType::Image,
                    url: Some("https://secret.test/image?token=secret-query".to_owned()),
                    failure_reason: Some("cookie=secret-cookie".to_owned()),
                    request_id: "secret-request".to_owned(),
                    epoch: 7,
                    status_code: None,
                    is_top_level: false,
                }],
                monitor_failed: true,
                ..RuntimeSnapshot::default()
            });
            item.set_interruption("item_timeout", "yarxi_media_readiness_timeout", true);
            drop(stage);
            drop(item);
        });
        assert_eq!(events.len(), 2);
        for event in events {
            let fields = &event["fields"];
            assert_eq!(fields["outcome"], "interrupted");
            assert_eq!(fields["stop_reason"], "item_timeout");
            assert_eq!(fields["failure_code"], "yarxi_media_readiness_timeout");
            assert_eq!(fields["retryable"], true);
            let snapshot = fields["runtime_snapshot"].as_str().unwrap();
            assert!(snapshot.len() <= MAX_RUNTIME_SNAPSHOT_BYTES);
            assert!(!snapshot.contains("secret"));
            assert!(!snapshot.contains("https"));
            let evidence: serde_json::Value = serde_json::from_str(snapshot).unwrap();
            assert_eq!(evidence["monitor_failed"], true);
            assert_eq!(evidence["network_failure_count"], 1);
        }
    }

    #[test]
    fn failure_is_emitted_once_and_does_not_leak_untrusted_tokens() {
        let events = events(|| {
            let item = BrowserItemTimer::new(BrowserItemContext::new(
                "jpdb",
                "https://example.test?cookie=secret",
                1,
                2,
            ));
            item.stage("navigation")
                .finish_failure("browser_navigation_timeout", true, None);
            item.finish_failure("cookie=secret", false, None);
        });
        assert_eq!(events.len(), 2);
        assert_eq!(
            events[0]["fields"]["failure_code"],
            "browser_navigation_timeout"
        );
        assert_eq!(events[0]["fields"]["retryable"], true);
        assert_eq!(events[1]["fields"]["failure_code"], "redacted");
        for event in events {
            assert_eq!(event["fields"]["identity"], "redacted");
            assert!(!event.to_string().contains("secret"));
        }
    }

    #[test]
    fn anticipated_interruption_is_not_reported_for_successful_work() {
        let events = events(|| {
            let item = BrowserItemTimer::new(BrowserItemContext::new("jpdb", "雨", 1, 0));
            item.set_interruption("ctrl_c", "acquisition_interrupted", false);
            item.stage("navigation").finish_success();
            item.finish_success();
        });
        assert_eq!(events.len(), 2);
        for event in events {
            assert_eq!(event["fields"]["outcome"], "success");
            assert!(event["fields"]["stop_reason"].is_null());
            assert!(event["fields"]["failure_code"].is_null());
        }
    }

    #[test]
    fn large_failure_snapshot_keeps_counts_and_bounds_serialized_evidence() {
        let events = events(|| {
            let item = BrowserItemTimer::new(BrowserItemContext::new("jpdb", "雨", 1, 0));
            let failure = NetworkOutcome {
                resource_type: ResourceType::Image,
                url: Some("https://secret.test?token=secret-query".repeat(100)),
                failure_reason: Some("Authorization: Bearer secret-header".repeat(100)),
                request_id: "secret-request".to_owned(),
                epoch: 9,
                status_code: Some(503),
                is_top_level: false,
            };
            let snapshot = RuntimeSnapshot {
                epoch: 9,
                network_failures: vec![failure.clone(); 1000],
                http_errors: vec![failure; 1000],
                ..RuntimeSnapshot::default()
            };
            item.finish_failure("browser_network_runtime_failure", true, Some(&snapshot));
        });
        let serialized = events[0]["fields"]["runtime_snapshot"].as_str().unwrap();
        assert!(serialized.len() <= MAX_RUNTIME_SNAPSHOT_BYTES);
        assert!(!serialized.contains("secret"));
        let snapshot: serde_json::Value = serde_json::from_str(serialized).unwrap();
        assert_eq!(snapshot["network_failure_count"], 1000);
        assert_eq!(snapshot["http_error_count"], 1000);
        assert!(snapshot["network_failures"].as_array().unwrap().len() < 1000);
        assert!(snapshot["http_errors"].as_array().unwrap().len() < 1000);
    }

    #[test]
    fn bounds_preserve_utf8_and_remove_payloads() {
        let identity = safe_identity(&"雨".repeat(1000));
        assert!(identity.len() <= MAX_IDENTITY_BYTES);
        assert!(identity.chars().all(|character| character == '雨'));
        assert_eq!(
            safe_token(&"a".repeat(1000)).len(),
            MAX_DIAGNOSTIC_TOKEN_BYTES
        );
        for value in [
            "cookie=secret",
            "Authorization: Bearer secret",
            "猫 Authorization: Bearer secret",
            "猫 cookie: session=secret",
            "雨 api_key: secret",
            "<html>body</html>",
            "a\nb",
        ] {
            assert_eq!(safe_identity(value), "redacted");
            assert_eq!(safe_token(value), "redacted");
        }
        for surface in ["ウォーター・サーバー", "〜によって", "雨（あめ）", "don't"]
        {
            assert_eq!(safe_identity(surface), surface);
        }
    }
}
