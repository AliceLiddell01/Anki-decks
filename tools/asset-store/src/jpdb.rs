//! JPDB vocabulary resolver and browser-rendered pitch-accent acquisition.
//!
//! This module deliberately uses JPDB's rendered, English-language pages as
//! its source of truth. It does not call private APIs or reconstruct pitch
//! graphs. One provider batch shares a single isolated `BrowserSession` and
//! processes queries sequentially.

use std::time::Duration;

use chromiumoxide::{
    Page,
    cdp::browser_protocol::{
        emulation::{
            MediaFeature, ResetPageScaleFactorParams, SetDeviceMetricsOverrideParams,
            SetEmulatedMediaParams, SetLocaleOverrideParams,
        },
        page::{CaptureScreenshotFormat, CaptureScreenshotParams, Viewport},
    },
};
use image::GenericImageView;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::time::{Instant, sleep, timeout};
use url::Url;

use crate::browser_runtime::{
    BrowserRuntimeConfig, BrowserSession, CdpRuntimeMonitor, DeviceMetrics, RuntimeSnapshot,
    is_relevant_resource_type,
};
use crate::pitch_accent::{
    PitchAccentCaptureRect, PitchAccentDarkThemeProof, PitchAccentDomainMetadata,
    PitchAccentEvidence, PitchAccentGraphEvidence, PitchAccentProvider, PitchAccentRenderEvidence,
    PitchAccentRenderKind,
};

const JPDB_ORIGIN: &str = "https://jpdb.io";
const JPDB_HOST: &str = "jpdb.io";
const CAPTURE_VIEWPORT_WIDTH: i64 = 1280;
const CAPTURE_VIEWPORT_HEIGHT: i64 = 1200;
const CAPTURE_DEVICE_SCALE_FACTOR: f64 = 3.0;
const ITEM_TIMEOUT: Duration = Duration::from_secs(90);
const READINESS_TIMEOUT: Duration = Duration::from_secs(20);
const READINESS_POLL_INTERVAL: Duration = Duration::from_millis(100);
const GRAPH_STABILITY_INTERVAL: Duration = Duration::from_millis(250);
const MAX_SEARCH_RESULTS: usize = 128;
const MAX_GRAPH_COUNT: usize = 32;

/// A surface form and optional exact reading to resolve on JPDB.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JpdbPitchQuery {
    pub surface: String,
    pub reading: Option<String>,
}

impl JpdbPitchQuery {
    pub fn new(surface: impl Into<String>, reading: Option<String>) -> Self {
        Self {
            surface: surface.into(),
            reading,
        }
    }

    fn validate(&self) -> Result<(), String> {
        if self.surface.trim().is_empty() {
            return Err("surface must not be empty".into());
        }
        if self
            .reading
            .as_deref()
            .is_some_and(|reading| reading.trim().is_empty())
        {
            return Err("reading must be absent or non-empty".into());
        }
        Ok(())
    }
}

/// Browser acquisition result for one input query.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum JpdbPitchOutcome {
    Acquired {
        asset: JpdbPitchAcquired,
    },
    NoPitchAccentOnSource {
        evidence: JpdbPitchAbsenceEvidence,
    },
    AmbiguousVocabulary {
        surface: String,
        reading: Option<String>,
        candidates: Vec<JpdbVocabularyCandidate>,
    },
    VocabularyNotFound {
        surface: String,
        reading: Option<String>,
    },
    Failed {
        error: JpdbPitchFailure,
    },
}

/// PNG bytes and ready-to-validate pitch-accent metadata.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JpdbPitchAcquired {
    /// The unmodified bytes returned by one native CDP PNG region screenshot.
    pub bytes: Vec<u8>,
    pub metadata: PitchAccentDomainMetadata,
}

/// Positive identity proof for a vocabulary page that has no pitch section.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JpdbPitchAbsenceEvidence {
    pub surface: String,
    pub reading: String,
    pub jpdb_vocabulary_id: u64,
    pub source_url: String,
    pub resolved_surface_forms: Vec<String>,
    pub resolved_readings: Vec<String>,
    /// Observed vocabulary section labels required before absence is accepted.
    pub section_inventory: Vec<String>,
    pub base_page_contract_valid: bool,
    pub pitch_section_present: bool,
    pub pitch_marker_count: u32,
    pub browser: crate::browser_runtime::BrowserRuntimeProvenance,
}

/// A search result that survived exact surface and optional reading matching.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JpdbVocabularyCandidate {
    pub vocabulary_id: u64,
    pub surface_forms: Vec<String>,
    pub readings: Vec<String>,
    /// The actual detail href exposed by JPDB's search result.
    pub detail_url: String,
}

/// Typed technical or source-contract failure for one item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "code", rename_all = "snake_case")]
pub enum JpdbPitchFailure {
    InvalidQuery {
        message: String,
    },
    BrowserSetup {
        message: String,
    },
    BrowserConfiguration {
        message: String,
    },
    Navigation {
        stage: JpdbPitchStage,
        message: String,
    },
    Timeout {
        stage: JpdbPitchStage,
    },
    Telemetry {
        stage: JpdbPitchStage,
        message: String,
    },
    PageContract {
        stage: JpdbPitchStage,
        message: String,
    },
    DetailIdentityMismatch {
        expected_surface: String,
        expected_reading: Option<String>,
        vocabulary_id: Option<u64>,
        observed_surface_forms: Vec<String>,
        observed_readings: Vec<String>,
    },
    DarkThemeUnverified {
        message: String,
    },
    CaptureContract {
        message: String,
    },
    Screenshot {
        message: String,
    },
    InvalidPng {
        message: String,
    },
}

/// Stable acquisition stage included in technical failures.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JpdbPitchStage {
    ConfigureBrowser,
    SearchNavigation,
    SearchReadiness,
    SearchResolution,
    DetailNavigation,
    DetailReadiness,
    DetailVerification,
    PitchInspection,
    Capture,
    PostCaptureVerification,
}

/// Production provider for JPDB's public vocabulary detail pages.
#[derive(Debug, Clone, Copy, Default)]
pub struct JpdbPitchProvider;

impl JpdbPitchProvider {
    /// Resolves and acquires one query in a fresh isolated Chromium session.
    pub async fn acquire(query: &JpdbPitchQuery) -> JpdbPitchOutcome {
        let mut outcomes = Self::acquire_many(std::slice::from_ref(query)).await;
        outcomes.pop().unwrap_or_else(|| {
            failed(JpdbPitchFailure::BrowserSetup {
                message: "provider returned no item outcome".into(),
            })
        })
    }

    /// Resolves queries sequentially while reusing one isolated browser session.
    /// Domain outcomes and item-level technical failures do not stop later items.
    pub async fn acquire_many(queries: &[JpdbPitchQuery]) -> Vec<JpdbPitchOutcome> {
        if queries.is_empty() {
            return Vec::new();
        }

        let runtime = BrowserRuntimeConfig {
            device_metrics: Some(
                DeviceMetrics::new(
                    CAPTURE_VIEWPORT_WIDTH,
                    CAPTURE_VIEWPORT_HEIGHT,
                    CAPTURE_DEVICE_SCALE_FACTOR,
                )
                .expect("static JPDB capture metrics are valid"),
            ),
            prefers_color_scheme: Some("dark".into()),
            ..BrowserRuntimeConfig::default()
        };
        let session = match BrowserSession::launch(runtime).await {
            Ok(session) => session,
            Err(message) => {
                return queries
                    .iter()
                    .map(|_| {
                        failed(JpdbPitchFailure::BrowserSetup {
                            message: message.clone(),
                        })
                    })
                    .collect();
            }
        };

        let setup = configure_page(session.page()).await;
        let outcomes = match setup {
            Ok(()) => acquire_many_in_session(&session, queries).await,
            Err(error) => queries.iter().map(|_| failed(error.clone())).collect(),
        };
        session.close().await;
        outcomes
    }

    /// Reuses a caller-owned `BrowserSession`; an already-open matching detail
    /// page is resolved directly. The provider enforces 3× metrics and dark
    /// color-scheme emulation on the supplied page before processing items.
    pub async fn acquire_many_in_session(
        session: &BrowserSession,
        queries: &[JpdbPitchQuery],
    ) -> Vec<JpdbPitchOutcome> {
        if queries.is_empty() {
            return Vec::new();
        }
        if let Err(error) = configure_page(session.page()).await {
            return queries.iter().map(|_| failed(error.clone())).collect();
        }
        acquire_many_in_session(session, queries).await
    }
}

async fn acquire_many_in_session(
    session: &BrowserSession,
    queries: &[JpdbPitchQuery],
) -> Vec<JpdbPitchOutcome> {
    let mut outcomes = Vec::with_capacity(queries.len());
    for query in queries {
        let result = timeout(
            ITEM_TIMEOUT,
            acquire_one_in_session(
                session.page(),
                session.telemetry(),
                session.provenance(),
                query,
            ),
        )
        .await;
        outcomes.push(match result {
            Ok(outcome) => outcome,
            Err(_) => failed(JpdbPitchFailure::Timeout {
                stage: JpdbPitchStage::DetailReadiness,
            }),
        });
    }
    outcomes
}

async fn configure_page(page: &Page) -> Result<(), JpdbPitchFailure> {
    page.emulate_locale(SetLocaleOverrideParams::builder().locale("en_US").build())
        .await
        .map_err(|error| JpdbPitchFailure::BrowserConfiguration {
            message: format!("CDP Emulation.setLocaleOverride: {error}"),
        })?;
    page.execute(SetDeviceMetricsOverrideParams::new(
        CAPTURE_VIEWPORT_WIDTH,
        CAPTURE_VIEWPORT_HEIGHT,
        CAPTURE_DEVICE_SCALE_FACTOR,
        false,
    ))
    .await
    .map_err(|error| JpdbPitchFailure::BrowserConfiguration {
        message: format!("CDP Emulation.setDeviceMetricsOverride: {error}"),
    })?;
    page.execute(
        SetEmulatedMediaParams::builder()
            .feature(MediaFeature::new("prefers-color-scheme", "dark"))
            .build(),
    )
    .await
    .map_err(|error| JpdbPitchFailure::BrowserConfiguration {
        message: format!("CDP Emulation.setEmulatedMedia: {error}"),
    })?;
    page.execute(ResetPageScaleFactorParams::default())
        .await
        .map_err(|error| JpdbPitchFailure::BrowserConfiguration {
            message: format!("CDP Emulation.resetPageScaleFactor: {error}"),
        })?;
    Ok(())
}

async fn acquire_one_in_session(
    page: &Page,
    telemetry: &CdpRuntimeMonitor,
    browser: &crate::browser_runtime::BrowserRuntimeProvenance,
    query: &JpdbPitchQuery,
) -> JpdbPitchOutcome {
    if let Err(message) = query.validate() {
        return failed(JpdbPitchFailure::InvalidQuery { message });
    }

    let initial_url = match current_url(page).await {
        Ok(url) => url,
        Err(error) => return failed(error),
    };
    let mut detail_url = None;

    if let Some(route) = parse_detail_route(&initial_url) {
        let initial_epoch = telemetry.begin_epoch();
        if let Err(error) = wait_for_critical_readiness(
            page,
            telemetry,
            initial_epoch,
            JpdbPitchStage::DetailReadiness,
        )
        .await
        {
            return failed(error);
        }
        match wait_for_detail_snapshot(page, telemetry, initial_epoch).await {
            Ok(snapshot) if detail_matches_query(&snapshot, query) => {
                detail_url = Some((route, snapshot));
            }
            Ok(_) => {}
            Err(error) if !initial_url.starts_with("about:") => {
                // A prior batch item can leave an unrelated vocabulary page
                // open. It is safe to search for this item instead.
                if matches!(error, JpdbPitchFailure::Timeout { .. }) {
                    // Continue below with a fresh search navigation.
                } else {
                    return failed(error);
                }
            }
            Err(error) => return failed(error),
        }
    }

    if detail_url.is_none() {
        let search_url = match build_search_url(&query.surface) {
            Ok(url) => url,
            Err(message) => {
                return failed(JpdbPitchFailure::InvalidQuery { message });
            }
        };
        let search_epoch = telemetry.begin_epoch();
        if let Err(error) = navigate(page, &search_url, JpdbPitchStage::SearchNavigation).await {
            return failed(error);
        }
        if let Err(error) = wait_for_critical_readiness(
            page,
            telemetry,
            search_epoch,
            JpdbPitchStage::SearchReadiness,
        )
        .await
        {
            return failed(error);
        }
        let page_url = match current_url(page).await {
            Ok(url) => url,
            Err(error) => return failed(error),
        };
        if let Some(route) = parse_detail_route(&page_url) {
            let snapshot = match wait_for_detail_snapshot(page, telemetry, search_epoch).await {
                Ok(snapshot) => snapshot,
                Err(error) => return failed(error),
            };
            if !detail_matches_query(&snapshot, query) {
                return vocabulary_not_found(query);
            }
            detail_url = Some((route, snapshot));
        } else if is_search_url(&page_url) {
            let search_results =
                match wait_for_search_candidates(page, telemetry, search_epoch).await {
                    Ok(results) => results,
                    Err(error) => return failed(error),
                };
            let matching = match select_candidates(&search_results.candidates, query) {
                Ok(matching) => matching,
                Err(error) => return failed(error),
            };
            match matching.len() {
                0 => return vocabulary_not_found(query),
                1 => {
                    let candidate = &matching[0];
                    let Some(route) = parse_detail_route(&candidate.detail_url) else {
                        return failed(JpdbPitchFailure::PageContract {
                            stage: JpdbPitchStage::SearchResolution,
                            message: "matching vocabulary result had an invalid detail href".into(),
                        });
                    };
                    if route.vocabulary_id != candidate.vocabulary_id {
                        return failed(JpdbPitchFailure::PageContract {
                            stage: JpdbPitchStage::SearchResolution,
                            message: "candidate ID does not match the result's actual detail href"
                                .into(),
                        });
                    }
                    let detail_epoch = telemetry.begin_epoch();
                    if let Err(error) = navigate(
                        page,
                        &candidate.detail_url,
                        JpdbPitchStage::DetailNavigation,
                    )
                    .await
                    {
                        return failed(error);
                    }
                    if let Err(error) = wait_for_critical_readiness(
                        page,
                        telemetry,
                        detail_epoch,
                        JpdbPitchStage::DetailReadiness,
                    )
                    .await
                    {
                        return failed(error);
                    }
                    let snapshot =
                        match wait_for_detail_snapshot(page, telemetry, detail_epoch).await {
                            Ok(snapshot) => snapshot,
                            Err(error) => return failed(error),
                        };
                    if snapshot.vocabulary_id != route.vocabulary_id
                        || parse_detail_route(&snapshot.url).as_ref() != Some(&route)
                        || !detail_matches_query(&snapshot, query)
                    {
                        return failed(JpdbPitchFailure::DetailIdentityMismatch {
                            expected_surface: query.surface.clone(),
                            expected_reading: query.reading.clone(),
                            vocabulary_id: Some(route.vocabulary_id),
                            observed_surface_forms: unique_forms(&snapshot.forms)
                                .into_iter()
                                .map(|form| form.surface)
                                .collect(),
                            observed_readings: unique_readings(&snapshot.forms),
                        });
                    }
                    detail_url = Some((route, snapshot));
                }
                _ => {
                    return JpdbPitchOutcome::AmbiguousVocabulary {
                        surface: query.surface.clone(),
                        reading: query.reading.clone(),
                        candidates: matching,
                    };
                }
            }
        } else {
            return failed(JpdbPitchFailure::PageContract {
                stage: JpdbPitchStage::SearchResolution,
                message: format!("JPDB search navigation ended at an unexpected URL: {page_url}"),
            });
        }
    }

    let Some((route, snapshot)) = detail_url else {
        return failed(JpdbPitchFailure::PageContract {
            stage: JpdbPitchStage::DetailVerification,
            message: "resolver did not produce a verified vocabulary detail page".into(),
        });
    };
    if !detail_matches_query(&snapshot, query) {
        return failed(JpdbPitchFailure::DetailIdentityMismatch {
            expected_surface: query.surface.clone(),
            expected_reading: query.reading.clone(),
            vocabulary_id: Some(route.vocabulary_id),
            observed_surface_forms: unique_forms(&snapshot.forms)
                .into_iter()
                .map(|form| form.surface)
                .collect(),
            observed_readings: unique_readings(&snapshot.forms),
        });
    }
    match inspect_and_capture(
        page,
        telemetry,
        browser,
        query,
        route.vocabulary_id,
        snapshot,
    )
    .await
    {
        Ok(outcome) => outcome,
        Err(error) => failed(error),
    }
}

async fn navigate(page: &Page, url: &str, stage: JpdbPitchStage) -> Result<(), JpdbPitchFailure> {
    timeout(READINESS_TIMEOUT, page.goto(url.to_owned()))
        .await
        .map_err(|_| JpdbPitchFailure::Timeout { stage })?
        .map_err(|error| JpdbPitchFailure::Navigation {
            stage,
            message: error.to_string(),
        })?;
    Ok(())
}

async fn current_url(page: &Page) -> Result<String, JpdbPitchFailure> {
    page.evaluate("() => location.href")
        .await
        .map_err(|error| JpdbPitchFailure::PageContract {
            stage: JpdbPitchStage::SearchResolution,
            message: format!("read current URL: {error}"),
        })?
        .into_value()
        .map_err(|error| JpdbPitchFailure::PageContract {
            stage: JpdbPitchStage::SearchResolution,
            message: format!("decode current URL: {error}"),
        })
}

async fn wait_for_critical_readiness(
    page: &Page,
    telemetry: &CdpRuntimeMonitor,
    epoch: u64,
    stage: JpdbPitchStage,
) -> Result<(), JpdbPitchFailure> {
    let deadline = Instant::now() + READINESS_TIMEOUT;
    loop {
        let url = current_url(page).await?;
        if !is_jpdb_url(&url) {
            return Err(JpdbPitchFailure::PageContract {
                stage,
                message: format!("navigation left jpdb.io: {url}"),
            });
        }
        let snapshot = telemetry.snapshot(epoch);
        if let Some(message) = critical_telemetry_failure(&snapshot) {
            return Err(JpdbPitchFailure::Telemetry { stage, message });
        }
        let ready_state: String = page
            .evaluate("() => document.readyState")
            .await
            .map_err(|error| JpdbPitchFailure::PageContract {
                stage,
                message: format!("read document readiness: {error}"),
            })?
            .into_value()
            .map_err(|error| JpdbPitchFailure::PageContract {
                stage,
                message: format!("decode document readiness: {error}"),
            })?;
        if matches!(ready_state.as_str(), "interactive" | "complete") {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(JpdbPitchFailure::Timeout { stage });
        }
        sleep(READINESS_POLL_INTERVAL).await;
    }
}

fn critical_telemetry_failure(snapshot: &RuntimeSnapshot) -> Option<String> {
    if snapshot.monitor_failed {
        return Some("CDP runtime monitor reported lost or incomplete telemetry".into());
    }
    if snapshot.javascript_exceptions > 0 {
        return Some(format!(
            "page raised {} JavaScript exception(s)",
            snapshot.javascript_exceptions
        ));
    }
    if let Some(outcome) = snapshot.network_failures.iter().find(|outcome| {
        is_critical_resource(
            &outcome.resource_type,
            outcome.url.as_deref(),
            outcome.is_top_level,
        )
    }) {
        return Some(format!(
            "critical request failed: {} ({})",
            outcome.url.as_deref().unwrap_or("unknown URL"),
            outcome
                .failure_reason
                .as_deref()
                .unwrap_or("unknown network failure")
        ));
    }
    if let Some(outcome) = snapshot.http_errors.iter().find(|outcome| {
        is_critical_resource(
            &outcome.resource_type,
            outcome.url.as_deref(),
            outcome.is_top_level,
        )
    }) {
        return Some(format!(
            "critical request returned HTTP {}: {}",
            outcome.status_code.unwrap_or_default(),
            outcome.url.as_deref().unwrap_or("unknown URL")
        ));
    }
    None
}

fn is_critical_resource(
    resource_type: &chromiumoxide::cdp::browser_protocol::network::ResourceType,
    raw_url: Option<&str>,
    is_top_level: bool,
) -> bool {
    if !is_relevant_resource_type(resource_type) {
        return false;
    }
    if is_top_level {
        return true;
    }
    if resource_type == &chromiumoxide::cdp::browser_protocol::network::ResourceType::Document {
        return false;
    }
    let is_jpdb_origin = raw_url
        .and_then(|raw| Url::parse(raw).ok())
        .and_then(|url| url.host_str().map(str::to_owned))
        .is_some_and(|host| host == JPDB_HOST || host.ends_with(".jpdb.io"));
    is_jpdb_origin
        && matches!(
            resource_type,
            chromiumoxide::cdp::browser_protocol::network::ResourceType::Stylesheet
                | chromiumoxide::cdp::browser_protocol::network::ResourceType::Script
                | chromiumoxide::cdp::browser_protocol::network::ResourceType::Font
        )
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DetailRoute {
    vocabulary_id: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct JpdbVocabularyForm {
    surface: String,
    reading: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DetailSnapshot {
    url: String,
    vocabulary_id: u64,
    forms: Vec<JpdbVocabularyForm>,
    part_of_speech: Vec<String>,
    section_inventory: Vec<String>,
    base_page_contract_valid: bool,
    pitch_section_present: bool,
    pitch_label: Option<String>,
    pitch_marker_count: usize,
    graph_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawVocabularyCandidate {
    detail_url: Option<String>,
    forms: Vec<JpdbVocabularyForm>,
    part_of_speech: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSearchSnapshot {
    ready: bool,
    no_results_found: bool,
    result_count: usize,
    truncated: bool,
    candidates: Vec<RawVocabularyCandidate>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ResolvedCandidate {
    public: JpdbVocabularyCandidate,
    forms: Vec<JpdbVocabularyForm>,
}

async fn wait_for_detail_snapshot(
    page: &Page,
    telemetry: &CdpRuntimeMonitor,
    epoch: u64,
) -> Result<DetailSnapshot, JpdbPitchFailure> {
    let deadline = Instant::now() + READINESS_TIMEOUT;
    loop {
        if let Some(message) = critical_telemetry_failure(&telemetry.snapshot(epoch)) {
            return Err(JpdbPitchFailure::Telemetry {
                stage: JpdbPitchStage::DetailReadiness,
                message,
            });
        }
        let url = current_url(page).await?;
        let Some(route) = parse_detail_route(&url) else {
            if Instant::now() >= deadline {
                return Err(JpdbPitchFailure::Timeout {
                    stage: JpdbPitchStage::DetailReadiness,
                });
            }
            sleep(READINESS_POLL_INTERVAL).await;
            continue;
        };
        match read_detail_snapshot(page, route.vocabulary_id).await {
            Ok(snapshot) => return Ok(snapshot),
            Err(JpdbPitchFailure::Timeout { .. }) if Instant::now() < deadline => {
                sleep(READINESS_POLL_INTERVAL).await;
            }
            Err(JpdbPitchFailure::Timeout { .. }) => {
                return Err(JpdbPitchFailure::PageContract {
                    stage: JpdbPitchStage::DetailReadiness,
                    message: "JPDB detail DOM did not reach its stable vocabulary contract before the readiness deadline".into(),
                });
            }
            Err(error) => return Err(error),
        }
    }
}

async fn read_detail_snapshot(
    page: &Page,
    expected_vocabulary_id: u64,
) -> Result<DetailSnapshot, JpdbPitchFailure> {
    let raw: Value = page
        .evaluate(DETAIL_SNAPSHOT_SCRIPT)
        .await
        .map_err(|error| JpdbPitchFailure::PageContract {
            stage: JpdbPitchStage::DetailVerification,
            message: format!("read JPDB detail DOM: {error}"),
        })?
        .into_value()
        .map_err(|error| JpdbPitchFailure::PageContract {
            stage: JpdbPitchStage::DetailVerification,
            message: format!("decode JPDB detail DOM: {error}"),
        })?;
    if let Some(message) = raw["error"].as_str() {
        let _ = message;
        return Err(JpdbPitchFailure::Timeout {
            stage: JpdbPitchStage::DetailReadiness,
        });
    }
    let url = raw["url"].as_str().unwrap_or_default().to_owned();
    let Some(route) = parse_detail_route(&url) else {
        return Err(JpdbPitchFailure::PageContract {
            stage: JpdbPitchStage::DetailVerification,
            message: "detail DOM is not on a jpdb.io vocabulary detail route".into(),
        });
    };
    if route.vocabulary_id != expected_vocabulary_id {
        return Err(JpdbPitchFailure::PageContract {
            stage: JpdbPitchStage::DetailVerification,
            message: "detail route changed while its DOM was inspected".into(),
        });
    }
    if raw["search_language"].as_str() != Some("english") {
        return Err(JpdbPitchFailure::PageContract {
            stage: JpdbPitchStage::DetailVerification,
            message: "JPDB interface is not in English (`#search-bar-lang` != english)".into(),
        });
    }
    let forms: Vec<JpdbVocabularyForm> =
        serde_json::from_value(raw["forms"].clone()).map_err(|error| {
            JpdbPitchFailure::PageContract {
                stage: JpdbPitchStage::DetailVerification,
                message: format!("invalid detail form evidence: {error}"),
            }
        })?;
    let part_of_speech: Vec<String> = serde_json::from_value(raw["part_of_speech"].clone())
        .map_err(|error| JpdbPitchFailure::PageContract {
            stage: JpdbPitchStage::DetailVerification,
            message: format!("invalid detail part-of-speech evidence: {error}"),
        })?;
    let section_inventory: Vec<String> = serde_json::from_value(raw["section_inventory"].clone())
        .map_err(|error| JpdbPitchFailure::PageContract {
        stage: JpdbPitchStage::DetailVerification,
        message: format!("invalid detail section inventory: {error}"),
    })?;
    let base_page_contract_valid = raw["base_page_contract_valid"].as_bool().unwrap_or(false);
    if forms.is_empty() || part_of_speech.is_empty() || !base_page_contract_valid {
        return Err(JpdbPitchFailure::Timeout {
            stage: JpdbPitchStage::DetailReadiness,
        });
    }
    let pitch_section_present = raw["pitch_section_present"].as_bool().unwrap_or(false);
    let pitch_label = raw["pitch_label"].as_str().map(str::to_owned);
    let pitch_marker_count = raw["pitch_marker_count"].as_u64().unwrap_or(0) as usize;
    let graph_count = raw["graph_count"].as_u64().unwrap_or(0) as usize;
    Ok(DetailSnapshot {
        url,
        vocabulary_id: route.vocabulary_id,
        forms,
        part_of_speech,
        section_inventory,
        base_page_contract_valid,
        pitch_section_present,
        pitch_label,
        pitch_marker_count,
        graph_count,
    })
}

async fn read_search_candidates(page: &Page) -> Result<RawSearchSnapshot, JpdbPitchFailure> {
    let raw: Value = page
        .evaluate(SEARCH_CANDIDATES_SCRIPT)
        .await
        .map_err(|error| JpdbPitchFailure::PageContract {
            stage: JpdbPitchStage::SearchResolution,
            message: format!("read JPDB search results: {error}"),
        })?
        .into_value()
        .map_err(|error| JpdbPitchFailure::PageContract {
            stage: JpdbPitchStage::SearchResolution,
            message: format!("decode JPDB search results: {error}"),
        })?;
    if let Some(message) = raw["error"].as_str() {
        return Err(JpdbPitchFailure::PageContract {
            stage: JpdbPitchStage::SearchResolution,
            message: message.to_owned(),
        });
    }
    let snapshot: RawSearchSnapshot =
        serde_json::from_value(raw).map_err(|error| JpdbPitchFailure::PageContract {
            stage: JpdbPitchStage::SearchResolution,
            message: format!("invalid JPDB search result inventory: {error}"),
        })?;
    if !snapshot.ready {
        return Err(JpdbPitchFailure::Timeout {
            stage: JpdbPitchStage::SearchReadiness,
        });
    }
    validate_search_inventory(&snapshot)?;
    Ok(snapshot)
}

async fn wait_for_search_candidates(
    page: &Page,
    telemetry: &CdpRuntimeMonitor,
    epoch: u64,
) -> Result<RawSearchSnapshot, JpdbPitchFailure> {
    let deadline = Instant::now() + READINESS_TIMEOUT;
    loop {
        if let Some(message) = critical_telemetry_failure(&telemetry.snapshot(epoch)) {
            return Err(JpdbPitchFailure::Telemetry {
                stage: JpdbPitchStage::SearchReadiness,
                message,
            });
        }
        match read_search_candidates(page).await {
            Ok(snapshot) => return Ok(snapshot),
            Err(JpdbPitchFailure::Timeout {
                stage: JpdbPitchStage::SearchReadiness,
            }) if Instant::now() < deadline => sleep(READINESS_POLL_INTERVAL).await,
            Err(JpdbPitchFailure::Timeout {
                stage: JpdbPitchStage::SearchReadiness,
            }) => {
                return Err(JpdbPitchFailure::PageContract {
                    stage: JpdbPitchStage::SearchResolution,
                    message: "JPDB search did not expose stable results or an explicit no-results marker before the readiness deadline".into(),
                });
            }
            Err(error) => return Err(error),
        }
    }
}

fn validate_search_inventory(snapshot: &RawSearchSnapshot) -> Result<(), JpdbPitchFailure> {
    if !snapshot.ready {
        return Err(JpdbPitchFailure::PageContract {
            stage: JpdbPitchStage::SearchResolution,
            message: "JPDB search result inventory was not ready".into(),
        });
    }
    if snapshot.truncated {
        return Err(JpdbPitchFailure::PageContract {
            stage: JpdbPitchStage::SearchResolution,
            message: "JPDB search result count exceeded the provider's bounded extraction limit"
                .into(),
        });
    }
    if snapshot.candidates.len() > snapshot.result_count {
        return Err(JpdbPitchFailure::PageContract {
            stage: JpdbPitchStage::SearchResolution,
            message: "JPDB vocabulary row count exceeds the declared search result count".into(),
        });
    }
    if snapshot.no_results_found && snapshot.result_count != 0 {
        return Err(JpdbPitchFailure::PageContract {
            stage: JpdbPitchStage::SearchResolution,
            message: "JPDB search marked no results while result rows are present".into(),
        });
    }
    if !snapshot.no_results_found && snapshot.result_count == 0 {
        return Err(JpdbPitchFailure::PageContract {
            stage: JpdbPitchStage::SearchResolution,
            message: "JPDB search DOM is empty without an explicit no-results marker".into(),
        });
    }
    if snapshot.no_results_found && !snapshot.candidates.is_empty() {
        return Err(JpdbPitchFailure::PageContract {
            stage: JpdbPitchStage::SearchResolution,
            message: "JPDB no-results marker conflicts with vocabulary candidates".into(),
        });
    }
    Ok(())
}

fn select_candidates(
    raw_candidates: &[RawVocabularyCandidate],
    query: &JpdbPitchQuery,
) -> Result<Vec<JpdbVocabularyCandidate>, JpdbPitchFailure> {
    if raw_candidates.len() > MAX_SEARCH_RESULTS {
        return Err(JpdbPitchFailure::PageContract {
            stage: JpdbPitchStage::SearchResolution,
            message: "JPDB returned more candidates than the provider's bounded result limit"
                .into(),
        });
    }
    let mut by_id: Vec<ResolvedCandidate> = Vec::new();
    for raw in raw_candidates {
        if raw.part_of_speech.is_empty()
            || raw.forms.is_empty()
            || raw
                .forms
                .iter()
                .any(|form| form.surface.is_empty() || form.reading.is_empty())
        {
            return Err(JpdbPitchFailure::PageContract {
                stage: JpdbPitchStage::SearchResolution,
                message:
                    "a recognized vocabulary result is missing forms or part-of-speech evidence"
                        .into(),
            });
        }
        let Some(detail_url) = raw.detail_url.as_deref() else {
            return Err(JpdbPitchFailure::PageContract {
                stage: JpdbPitchStage::SearchResolution,
                message: "a recognized vocabulary result has no actual detail href".into(),
            });
        };
        let Some(route) = parse_detail_route(detail_url) else {
            return Err(JpdbPitchFailure::PageContract {
                stage: JpdbPitchStage::SearchResolution,
                message: "a recognized vocabulary result has an invalid detail href".into(),
            });
        };
        if raw
            .part_of_speech
            .iter()
            .any(|pos| pos.trim().eq_ignore_ascii_case("name"))
        {
            continue;
        }
        if !forms_match_query(&raw.forms, query) {
            continue;
        }
        let public = JpdbVocabularyCandidate {
            vocabulary_id: route.vocabulary_id,
            surface_forms: unique_forms(&raw.forms)
                .into_iter()
                .map(|form| form.surface)
                .collect(),
            readings: unique_readings(&raw.forms),
            detail_url: detail_url.to_owned(),
        };
        if let Some(existing) = by_id
            .iter_mut()
            .find(|candidate| candidate.public.vocabulary_id == route.vocabulary_id)
        {
            existing.forms = merge_forms(&existing.forms, &raw.forms);
            existing.public.surface_forms = unique_forms(&existing.forms)
                .into_iter()
                .map(|form| form.surface)
                .collect();
            existing.public.readings = unique_readings(&existing.forms);
        } else {
            by_id.push(ResolvedCandidate {
                public,
                forms: raw.forms.clone(),
            });
        }
    }
    Ok(by_id
        .into_iter()
        .map(|candidate| candidate.public)
        .collect())
}

fn forms_match_query(forms: &[JpdbVocabularyForm], query: &JpdbPitchQuery) -> bool {
    forms.iter().any(|form| form.surface == query.surface)
        && query
            .reading
            .as_deref()
            .is_none_or(|reading| forms.iter().any(|form| form.reading == reading))
}

fn resolve_query_reading(forms: &[JpdbVocabularyForm], query: &JpdbPitchQuery) -> Option<String> {
    if let Some(reading) = query.reading.as_deref() {
        return forms
            .iter()
            .any(|form| form.reading == reading)
            .then(|| reading.to_owned());
    }

    forms
        .iter()
        .find(|form| form.surface == query.surface && !form.reading.is_empty())
        .or_else(|| forms.iter().find(|form| !form.reading.is_empty()))
        .map(|form| form.reading.clone())
}

fn detail_matches_query(detail: &DetailSnapshot, query: &JpdbPitchQuery) -> bool {
    parse_detail_route(&detail.url).is_some_and(|route| route.vocabulary_id == detail.vocabulary_id)
        && !detail
            .part_of_speech
            .iter()
            .any(|pos| pos.trim().eq_ignore_ascii_case("name"))
        && forms_match_query(&detail.forms, query)
}

fn merge_forms(
    existing: &[JpdbVocabularyForm],
    additional: &[JpdbVocabularyForm],
) -> Vec<JpdbVocabularyForm> {
    let mut forms = existing.to_vec();
    for form in additional {
        if !forms.contains(form) {
            forms.push(form.clone());
        }
    }
    forms
}

fn unique_forms(forms: &[JpdbVocabularyForm]) -> Vec<JpdbVocabularyForm> {
    let mut unique = Vec::new();
    for form in forms {
        if !form.surface.is_empty()
            && !unique
                .iter()
                .any(|existing: &JpdbVocabularyForm| existing == form)
        {
            unique.push(form.clone());
        }
    }
    unique
}

fn unique_readings(forms: &[JpdbVocabularyForm]) -> Vec<String> {
    let mut readings = Vec::new();
    for form in unique_forms(forms) {
        if !form.reading.is_empty() && !readings.contains(&form.reading) {
            readings.push(form.reading);
        }
    }
    readings
}

async fn inspect_and_capture(
    page: &Page,
    telemetry: &CdpRuntimeMonitor,
    browser: &crate::browser_runtime::BrowserRuntimeProvenance,
    query: &JpdbPitchQuery,
    vocabulary_id: u64,
    mut detail: DetailSnapshot,
) -> Result<JpdbPitchOutcome, JpdbPitchFailure> {
    if !is_jpdb_url(&detail.url) || detail.vocabulary_id != vocabulary_id {
        return Err(JpdbPitchFailure::DetailIdentityMismatch {
            expected_surface: query.surface.clone(),
            expected_reading: query.reading.clone(),
            vocabulary_id: Some(vocabulary_id),
            observed_surface_forms: unique_forms(&detail.forms)
                .into_iter()
                .map(|form| form.surface)
                .collect(),
            observed_readings: unique_readings(&detail.forms),
        });
    }
    if !detail_matches_query(&detail, query) {
        return Err(JpdbPitchFailure::DetailIdentityMismatch {
            expected_surface: query.surface.clone(),
            expected_reading: query.reading.clone(),
            vocabulary_id: Some(vocabulary_id),
            observed_surface_forms: unique_forms(&detail.forms)
                .into_iter()
                .map(|form| form.surface)
                .collect(),
            observed_readings: unique_readings(&detail.forms),
        });
    }
    let resolved = unique_forms(&detail.forms);
    let resolved_reading = resolve_query_reading(&resolved, query).ok_or_else(|| {
        JpdbPitchFailure::DetailIdentityMismatch {
            expected_surface: query.surface.clone(),
            expected_reading: query.reading.clone(),
            vocabulary_id: Some(vocabulary_id),
            observed_surface_forms: resolved.iter().map(|form| form.surface.clone()).collect(),
            observed_readings: unique_readings(&resolved),
        }
    })?;
    let inspection_epoch = telemetry.begin_epoch();
    wait_for_critical_readiness(
        page,
        telemetry,
        inspection_epoch,
        JpdbPitchStage::PitchInspection,
    )
    .await?;
    match classify_pitch_state(&detail)? {
        PitchDomState::Absent => {
            let first_absence_snapshot = detail.clone();
            sleep(GRAPH_STABILITY_INTERVAL).await;
            detail = read_detail_snapshot(page, vocabulary_id).await?;
            if detail != first_absence_snapshot {
                return Err(JpdbPitchFailure::PageContract {
                    stage: JpdbPitchStage::PitchInspection,
                    message: "the no-pitch detail section inventory was not stable".into(),
                });
            }
            if let Some(message) = critical_telemetry_failure(&telemetry.snapshot(inspection_epoch))
            {
                return Err(JpdbPitchFailure::Telemetry {
                    stage: JpdbPitchStage::PitchInspection,
                    message,
                });
            }
            return Ok(JpdbPitchOutcome::NoPitchAccentOnSource {
                evidence: JpdbPitchAbsenceEvidence {
                    surface: query.surface.clone(),
                    reading: resolved_reading.clone(),
                    jpdb_vocabulary_id: vocabulary_id,
                    source_url: detail.url,
                    resolved_surface_forms: resolved
                        .iter()
                        .map(|form| form.surface.clone())
                        .collect(),
                    resolved_readings: unique_readings(&resolved),
                    section_inventory: detail.section_inventory.clone(),
                    base_page_contract_valid: detail.base_page_contract_valid,
                    pitch_section_present: detail.pitch_section_present,
                    pitch_marker_count: detail.pitch_marker_count as u32,
                    browser: browser.clone(),
                },
            });
        }
        PitchDomState::Present { graph_count } if graph_count == detail.graph_count => {}
        PitchDomState::Present { .. } => {
            return Err(JpdbPitchFailure::PageContract {
                stage: JpdbPitchStage::PitchInspection,
                message: "recognized pitch graph count changed during classification".into(),
            });
        }
    }
    if detail.graph_count > MAX_GRAPH_COUNT {
        return Err(JpdbPitchFailure::PageContract {
            stage: JpdbPitchStage::PitchInspection,
            message: "JPDB pitch graph count exceeds the provider's bounded capture limit".into(),
        });
    }

    activate_dark_mode(page).await?;
    let first = capture_snapshot(page).await?;
    sleep(GRAPH_STABILITY_INTERVAL).await;
    let second = capture_snapshot(page).await?;
    if first != second {
        return Err(JpdbPitchFailure::CaptureContract {
            message: "pitch graph DOM, geometry, or theme changed before capture".into(),
        });
    }
    detail = read_detail_snapshot(page, vocabulary_id).await?;
    if !detail_matches_query(&detail, query)
        || detail.graph_count != second.graphs.len()
        || detail.pitch_label.as_deref() != Some("Pitch accent")
    {
        return Err(JpdbPitchFailure::DetailIdentityMismatch {
            expected_surface: query.surface.clone(),
            expected_reading: query.reading.clone(),
            vocabulary_id: Some(vocabulary_id),
            observed_surface_forms: unique_forms(&detail.forms)
                .into_iter()
                .map(|form| form.surface)
                .collect(),
            observed_readings: unique_readings(&detail.forms),
        });
    }

    let clip = Viewport::builder()
        .x(second.rect.x)
        .y(second.rect.y)
        .width(second.rect.width)
        .height(second.rect.height)
        .scale(1.0)
        .build()
        .map_err(|message| JpdbPitchFailure::CaptureContract { message })?;
    let params = CaptureScreenshotParams::builder()
        .format(CaptureScreenshotFormat::Png)
        .clip(clip)
        .capture_beyond_viewport(false)
        .build();
    let bytes = page
        .screenshot(params)
        .await
        .map_err(|error| JpdbPitchFailure::Screenshot {
            message: error.to_string(),
        })?;
    if !bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Err(JpdbPitchFailure::InvalidPng {
            message: "native browser capture did not return PNG bytes".into(),
        });
    }
    let image =
        image::load_from_memory_with_format(&bytes, image::ImageFormat::Png).map_err(|error| {
            JpdbPitchFailure::InvalidPng {
                message: format!("native browser screenshot is not a decodable PNG: {error}"),
            }
        })?;
    let (pixel_width, pixel_height) = image.dimensions();
    if pixel_width == 0 || pixel_height == 0 {
        return Err(JpdbPitchFailure::InvalidPng {
            message: "native browser screenshot has an empty pixel size".into(),
        });
    }
    let post_capture = capture_snapshot(page).await?;
    if post_capture != second {
        return Err(JpdbPitchFailure::DarkThemeUnverified {
            message: "theme, graph DOM, or capture geometry changed during native screenshot"
                .into(),
        });
    }
    let post_capture_detail = read_detail_snapshot(page, vocabulary_id).await?;
    if post_capture_detail.url != detail.url
        || post_capture_detail.vocabulary_id != vocabulary_id
        || !detail_matches_query(&post_capture_detail, query)
        || unique_forms(&post_capture_detail.forms) != resolved
    {
        return Err(JpdbPitchFailure::DetailIdentityMismatch {
            expected_surface: query.surface.clone(),
            expected_reading: query.reading.clone(),
            vocabulary_id: Some(vocabulary_id),
            observed_surface_forms: unique_forms(&post_capture_detail.forms)
                .into_iter()
                .map(|form| form.surface)
                .collect(),
            observed_readings: unique_readings(&post_capture_detail.forms),
        });
    }
    if let Some(message) = critical_telemetry_failure(&telemetry.snapshot(inspection_epoch)) {
        return Err(JpdbPitchFailure::Telemetry {
            stage: JpdbPitchStage::PostCaptureVerification,
            message,
        });
    }
    let expected_width = (second.rect.width * CAPTURE_DEVICE_SCALE_FACTOR).round() as u32;
    let expected_height = (second.rect.height * CAPTURE_DEVICE_SCALE_FACTOR).round() as u32;
    let edge_rounding_tolerance = CAPTURE_DEVICE_SCALE_FACTOR.ceil() as u32;
    if pixel_width.abs_diff(expected_width) > edge_rounding_tolerance
        || pixel_height.abs_diff(expected_height) > edge_rounding_tolerance
    {
        return Err(JpdbPitchFailure::CaptureContract {
            message: format!(
                "PNG size {pixel_width}x{pixel_height} does not match CSS clip {:.3}x{:.3} at 3x",
                second.rect.width, second.rect.height
            ),
        });
    }

    let resolved_surface_forms: Vec<String> =
        resolved.iter().map(|form| form.surface.clone()).collect();
    let resolved_readings = unique_readings(&resolved);
    let graphs = second
        .graphs
        .iter()
        .enumerate()
        .map(|(index, graph)| PitchAccentGraphEvidence {
            index: index as u32,
            selector: graph.selector.clone(),
        })
        .collect::<Vec<_>>();
    let metadata = PitchAccentDomainMetadata {
        surface: query.surface.clone(),
        reading: resolved_reading,
        jpdb_vocabulary_id: vocabulary_id,
        evidence: PitchAccentEvidence {
            provider: PitchAccentProvider::Jpdb,
            source_url: detail.url,
            resolved_surface_forms,
            resolved_readings,
            graph_count: graphs.len() as u32,
            render: PitchAccentRenderEvidence {
                kind: PitchAccentRenderKind::BrowserRegionScreenshot,
                selector: ".subsection-pitch-accent .subsection > div > div > div[style*=\"word-break: keep-all\"]".into(),
                graphs,
                viewport_width: second.viewport_width,
                viewport_height: second.viewport_height,
                pixel_width,
                pixel_height,
                device_scale_factor: CAPTURE_DEVICE_SCALE_FACTOR,
                page_scale_factor: second.page_scale_factor,
                dark_theme: second.dark_theme,
                capture_rect: second.rect,
            },
            browser: browser.clone(),
        },
    };
    Ok(JpdbPitchOutcome::Acquired {
        asset: JpdbPitchAcquired { bytes, metadata },
    })
}

async fn activate_dark_mode(page: &Page) -> Result<(), JpdbPitchFailure> {
    page.evaluate("() => { document.documentElement.classList.add('dark-mode'); return true; }")
        .await
        .map_err(|error| JpdbPitchFailure::DarkThemeUnverified {
            message: format!("activate html.dark-mode: {error}"),
        })?
        .into_value::<bool>()
        .map_err(|error| JpdbPitchFailure::DarkThemeUnverified {
            message: format!("decode dark-mode activation result: {error}"),
        })?;
    let state = capture_snapshot(page).await?;
    if !state
        .dark_theme
        .document_element_classes
        .iter()
        .any(|class| class == "dark-mode")
        || state.dark_theme.prefers_color_scheme != "dark"
        || state.dark_theme.computed_color_scheme.trim().is_empty()
    {
        return Err(JpdbPitchFailure::DarkThemeUnverified {
            message: format!("JPDB dark-mode proof is incomplete: {:?}", state.dark_theme),
        });
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq)]
struct CaptureGraph {
    selector: String,
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

#[derive(Debug, Clone, PartialEq)]
struct CaptureSnapshot {
    graphs: Vec<CaptureGraph>,
    rect: PitchAccentCaptureRect,
    viewport_width: u32,
    viewport_height: u32,
    page_scale_factor: f64,
    device_pixel_ratio: f64,
    dark_theme: PitchAccentDarkThemeProof,
}

async fn capture_snapshot(page: &Page) -> Result<CaptureSnapshot, JpdbPitchFailure> {
    let raw: Value = page
        .evaluate(CAPTURE_SNAPSHOT_SCRIPT)
        .await
        .map_err(|error| JpdbPitchFailure::CaptureContract {
            message: format!("read graph capture geometry: {error}"),
        })?
        .into_value()
        .map_err(|error| JpdbPitchFailure::CaptureContract {
            message: format!("decode graph capture geometry: {error}"),
        })?;
    if let Some(message) = raw["error"].as_str() {
        return Err(JpdbPitchFailure::CaptureContract {
            message: message.to_owned(),
        });
    }
    let graphs: Vec<CaptureGraph> = raw["graphs"]
        .as_array()
        .ok_or_else(|| JpdbPitchFailure::CaptureContract {
            message: "capture DOM returned no graph array".into(),
        })?
        .iter()
        .map(|graph| {
            Ok(CaptureGraph {
                selector: graph["selector"]
                    .as_str()
                    .ok_or_else(|| JpdbPitchFailure::CaptureContract {
                        message: "graph selector is missing".into(),
                    })?
                    .to_owned(),
                x: finite_number(&graph["x"], "graph x")?,
                y: finite_number(&graph["y"], "graph y")?,
                width: finite_number(&graph["width"], "graph width")?,
                height: finite_number(&graph["height"], "graph height")?,
            })
        })
        .collect::<Result<_, JpdbPitchFailure>>()?;
    if graphs.is_empty() || graphs.len() > MAX_GRAPH_COUNT {
        return Err(JpdbPitchFailure::CaptureContract {
            message: "capture DOM returned an empty or excessive graph set".into(),
        });
    }
    let rect = PitchAccentCaptureRect {
        x: finite_number(&raw["rect"]["x"], "capture x")?,
        y: finite_number(&raw["rect"]["y"], "capture y")?,
        width: finite_number(&raw["rect"]["width"], "capture width")?,
        height: finite_number(&raw["rect"]["height"], "capture height")?,
    };
    let viewport_width = raw["viewport_width"]
        .as_u64()
        .and_then(|value| u32::try_from(value).ok())
        .filter(|value| *value > 0)
        .ok_or_else(|| JpdbPitchFailure::CaptureContract {
            message: "capture DOM returned an invalid viewport width".into(),
        })?;
    let viewport_height = raw["viewport_height"]
        .as_u64()
        .and_then(|value| u32::try_from(value).ok())
        .filter(|value| *value > 0)
        .ok_or_else(|| JpdbPitchFailure::CaptureContract {
            message: "capture DOM returned an invalid viewport height".into(),
        })?;
    let page_scale_factor = finite_number(&raw["page_scale_factor"], "page scale factor")?;
    let device_pixel_ratio = finite_number(&raw["device_pixel_ratio"], "device pixel ratio")?;
    let classes: Vec<String> = serde_json::from_value(
        raw["dark_theme"]["document_element_classes"].clone(),
    )
    .map_err(|error| JpdbPitchFailure::DarkThemeUnverified {
        message: format!("invalid html class evidence: {error}"),
    })?;
    let dark_theme = PitchAccentDarkThemeProof {
        document_element_classes: classes,
        prefers_color_scheme: raw["dark_theme"]["prefers_color_scheme"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
        computed_color_scheme: raw["dark_theme"]["computed_color_scheme"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
    };
    validate_capture_geometry(
        rect,
        viewport_width,
        viewport_height,
        page_scale_factor,
        device_pixel_ratio,
    )?;
    Ok(CaptureSnapshot {
        graphs,
        rect,
        viewport_width,
        viewport_height,
        page_scale_factor,
        device_pixel_ratio,
        dark_theme,
    })
}

fn finite_number(value: &Value, label: &str) -> Result<f64, JpdbPitchFailure> {
    value
        .as_f64()
        .filter(|number| number.is_finite())
        .ok_or_else(|| JpdbPitchFailure::CaptureContract {
            message: format!("capture DOM returned invalid {label}"),
        })
}

fn validate_capture_geometry(
    rect: PitchAccentCaptureRect,
    viewport_width: u32,
    viewport_height: u32,
    page_scale_factor: f64,
    device_pixel_ratio: f64,
) -> Result<(), JpdbPitchFailure> {
    if ![rect.x, rect.y, rect.width, rect.height]
        .into_iter()
        .all(f64::is_finite)
        || rect.x < 0.0
        || rect.y < 0.0
        || rect.width <= 0.0
        || rect.height <= 0.0
        || rect.x + rect.width > f64::from(viewport_width) + 0.5
        || rect.y + rect.height > f64::from(viewport_height) + 0.5
    {
        return Err(JpdbPitchFailure::CaptureContract {
            message: "graph union rectangle is invalid or outside the browser viewport".into(),
        });
    }
    if (page_scale_factor - 1.0).abs() > f64::EPSILON {
        return Err(JpdbPitchFailure::CaptureContract {
            message: format!("page scale factor is {page_scale_factor}, expected 1.0"),
        });
    }
    if (device_pixel_ratio - CAPTURE_DEVICE_SCALE_FACTOR).abs() > 0.001 {
        return Err(JpdbPitchFailure::CaptureContract {
            message: format!(
                "window.devicePixelRatio is {device_pixel_ratio}, expected {CAPTURE_DEVICE_SCALE_FACTOR}"
            ),
        });
    }
    Ok(())
}

fn build_search_url(surface: &str) -> Result<String, String> {
    if surface.trim().is_empty() {
        return Err("surface must not be empty".into());
    }
    let mut url = Url::parse(&format!("{JPDB_ORIGIN}/search"))
        .map_err(|error| format!("invalid JPDB search origin: {error}"))?;
    url.query_pairs_mut().append_pair("q", surface);
    Ok(url.to_string())
}

fn is_jpdb_url(raw_url: &str) -> bool {
    Url::parse(raw_url).is_ok_and(|url| {
        url.scheme() == "https"
            && url
                .host_str()
                .is_some_and(|host| host.eq_ignore_ascii_case(JPDB_HOST))
            && url.port_or_known_default() == Some(443)
            && url.username().is_empty()
            && url.password().is_none()
    })
}

fn is_search_url(raw_url: &str) -> bool {
    Url::parse(raw_url).is_ok_and(|url| is_jpdb_url(url.as_str()) && url.path() == "/search")
}

fn parse_detail_route(raw_url: &str) -> Option<DetailRoute> {
    let url = Url::parse(raw_url).ok()?;
    if !is_jpdb_url(url.as_str()) {
        return None;
    }
    let mut segments = url.path_segments()?;
    if segments.next()? != "vocabulary" {
        return None;
    }
    let vocabulary_id = segments.next()?.parse::<u64>().ok().filter(|id| *id > 0)?;
    let surface_slug = segments.next()?;
    if surface_slug.is_empty() || surface_slug.eq_ignore_ascii_case("used-in") {
        return None;
    }
    if segments
        .next()
        .is_some_and(|reading| reading.eq_ignore_ascii_case("used-in"))
        || segments.next().is_some()
    {
        return None;
    }
    Some(DetailRoute { vocabulary_id })
}

fn vocabulary_not_found(query: &JpdbPitchQuery) -> JpdbPitchOutcome {
    JpdbPitchOutcome::VocabularyNotFound {
        surface: query.surface.clone(),
        reading: query.reading.clone(),
    }
}

fn failed(error: JpdbPitchFailure) -> JpdbPitchOutcome {
    JpdbPitchOutcome::Failed { error }
}

const SEARCH_CANDIDATES_SCRIPT: &str = r#"() => {
  const lang = document.querySelector('#search-bar-lang');
  const results = document.querySelector('.results');
  if (!lang || !results) return { ready: false, no_results_found: false, result_count: 0, truncated: false, candidates: [] };
  if (lang.value !== 'english') return { error: 'JPDB interface is not in English (#search-bar-lang != english)' };
  const rubyForm = root => {
    if (!root) return [];
    let surface = '';
    let reading = '';
    const compact = value => value.replace(/\s+/gu, '');
    const isKana = value => /^[\u3040-\u309f\u30a0-\u30ffー゙゚]+$/u.test(value);
    const baseText = node => {
      if (node.nodeType === Node.ELEMENT_NODE && node.tagName === 'RT') return '';
      if (node.nodeType === Node.TEXT_NODE) return node.textContent || '';
      if (node.nodeType === Node.ELEMENT_NODE && node.matches('.property-text')) return '';
      return [...node.childNodes].map(baseText).join('');
    };
    const visit = node => {
      if (node.nodeType === Node.TEXT_NODE) {
        const text = compact(node.textContent || '');
        surface += text;
        if (isKana(text)) reading += text;
        return;
      }
      if (!(node instanceof Element) || node.matches('.property-text')) return;
      if (node.tagName === 'RT') {
        reading += compact(node.textContent || '');
        return;
      }
      if (node.tagName === 'RUBY') {
        let pendingBase = '';
        for (const child of node.childNodes) {
          if (child instanceof Element && child.tagName === 'RT') {
            const base = compact(pendingBase);
            const annotation = compact(child.textContent || '');
            surface += base;
            reading += annotation || (isKana(base) ? base : '');
            pendingBase = '';
          } else {
            pendingBase += baseText(child);
          }
        }
        const trailingBase = compact(pendingBase);
        surface += trailingBase;
        if (isKana(trailingBase)) reading += trailingBase;
        return;
      }
      for (const child of node.childNodes) visit(child);
    };
    for (const child of root.childNodes) visit(child);
    if (!surface) {
      const fallback = compact(root.innerText || root.textContent || '');
      return fallback ? [{ surface: fallback, reading: isKana(fallback) ? fallback : '' }] : [];
    }
    return [{ surface, reading }];
  };
  const detailHref = (root, vocabularyId) => {
    const valid = anchor => {
      try {
        const url = new URL(anchor.href, location.href);
        const pieces = url.pathname.split('/').filter(Boolean);
        return url.hostname === 'jpdb.io' && pieces[0] === 'vocabulary' && pieces[1] === String(vocabularyId) && pieces.length >= 3 && pieces.length <= 4 && pieces[2] !== 'used-in' && pieces[3] !== 'used-in';
      } catch (_) { return false; }
    };
    const detailsLinks = [...root.querySelectorAll('a.view-conjugations-link[href]')].filter(valid);
    if (detailsLinks.length) return detailsLinks[0].href;
    const spellingLinks = [...root.querySelectorAll('.subsection-other-spellings .alt-spelling a.plain')].filter(valid);
    return spellingLinks.length ? spellingLinks[0].href : null;
  };
  const candidates = [];
  const resultRows = [...results.querySelectorAll('div[id^="result-"]')];
  const vocabularyRows = [...results.querySelectorAll('div.result.vocabulary')];
  const resultCount = Math.max(resultRows.length, vocabularyRows.length);
  const noResultsFound = /\bNo results found\./i.test(results.innerText || '');
  if (noResultsFound && resultCount) return { error: 'JPDB no-results marker conflicts with result rows' };
  if (!noResultsFound && resultCount === 0) return { ready: false, no_results_found: false, result_count: 0, truncated: false, candidates: [] };
  for (const row of vocabularyRows.slice(0, 128)) {
    const primary = row.querySelector('.subsection-headword .primary-spelling');
    const primaryForms = rubyForm(primary);
    const forms = [...primaryForms];
    for (const alternate of row.querySelectorAll('.subsection-other-spellings .alt-spelling a.plain')) {
      if (alternate.closest('a[href^="/kanji/"]')) continue;
      forms.push(...rubyForm(alternate));
    }
    const canonicalReading = primaryForms.find(form => form.reading)?.reading
      || forms.find(form => form.reading)?.reading
      || '';
    for (const form of forms) if (!form.reading) form.reading = canonicalReading || form.surface;
    const partOfSpeech = [...row.querySelectorAll('.subsection-meanings .part-of-speech')]
      .map(node => (node.innerText || node.textContent || '').trim()).filter(Boolean);
    const anchor = [...row.querySelectorAll('a[href^="/vocabulary/"], a[href^="https://jpdb.io/vocabulary/"]')]
      .find(valid => {
        try {
          const url = new URL(valid.href, location.href);
          return url.hostname === 'jpdb.io' && url.pathname.split('/').filter(Boolean)[0] === 'vocabulary';
        } catch (_) { return false; }
      });
    let vocabularyId = null;
    if (anchor) {
      const parts = new URL(anchor.href, location.href).pathname.split('/').filter(Boolean);
      vocabularyId = Number(parts[1]);
    }
    const href = Number.isSafeInteger(vocabularyId) && vocabularyId > 0 ? detailHref(row, vocabularyId) : null;
    if (!forms.length || !partOfSpeech.length || !href) {
      return { error: 'a recognized JPDB vocabulary result is missing form, part-of-speech, or detail-href evidence' };
    }
    candidates.push({
      detail_url: href,
      forms,
      part_of_speech: partOfSpeech,
    });
  }
  return {
    ready: true,
    no_results_found: noResultsFound,
    result_count: resultCount,
    truncated: resultCount > 128 || vocabularyRows.length > 128,
    candidates,
  };
}"#;

const DETAIL_SNAPSHOT_SCRIPT: &str = r#"() => {
  const lang = document.querySelector('#search-bar-lang');
  const meanings = document.querySelector('.subsection-meanings');
  const primary = document.querySelector('.subsection-headword .primary-spelling');
  const rubyForm = root => {
    if (!root) return [];
    let surface = '';
    let reading = '';
    const compact = value => value.replace(/\s+/gu, '');
    const isKana = value => /^[\u3040-\u309f\u30a0-\u30ffー゙゚]+$/u.test(value);
    const baseText = node => {
      if (node.nodeType === Node.ELEMENT_NODE && node.tagName === 'RT') return '';
      if (node.nodeType === Node.TEXT_NODE) return node.textContent || '';
      if (node.nodeType === Node.ELEMENT_NODE && node.matches('.property-text')) return '';
      return [...node.childNodes].map(baseText).join('');
    };
    const visit = node => {
      if (node.nodeType === Node.TEXT_NODE) {
        const text = compact(node.textContent || '');
        surface += text;
        if (isKana(text)) reading += text;
        return;
      }
      if (!(node instanceof Element) || node.matches('.property-text')) return;
      if (node.tagName === 'RT') {
        reading += compact(node.textContent || '');
        return;
      }
      if (node.tagName === 'RUBY') {
        let pendingBase = '';
        for (const child of node.childNodes) {
          if (child instanceof Element && child.tagName === 'RT') {
            const base = compact(pendingBase);
            const annotation = compact(child.textContent || '');
            surface += base;
            reading += annotation || (isKana(base) ? base : '');
            pendingBase = '';
          } else {
            pendingBase += baseText(child);
          }
        }
        const trailingBase = compact(pendingBase);
        surface += trailingBase;
        if (isKana(trailingBase)) reading += trailingBase;
        return;
      }
      for (const child of node.childNodes) visit(child);
    };
    for (const child of root.childNodes) visit(child);
    if (!surface) {
      const fallback = compact(root.innerText || root.textContent || '');
      return fallback ? [{ surface: fallback, reading: isKana(fallback) ? fallback : '' }] : [];
    }
    return [{ surface, reading }];
  };
  if (!lang || !primary || !meanings) return { error: 'JPDB vocabulary detail contract is not ready' };
  const primaryForms = rubyForm(primary);
  const forms = [...primaryForms];
  for (const alternate of document.querySelectorAll('.subsection-other-spellings .alt-spelling a.plain')) {
    forms.push(...rubyForm(alternate));
  }
  const canonicalReading = primaryForms.find(form => form.reading)?.reading
    || forms.find(form => form.reading)?.reading
    || '';
  for (const form of forms) if (!form.reading) form.reading = canonicalReading || form.surface;
  const section = document.querySelector('.subsection-pitch-accent');
  const label = section?.querySelector('.subsection-label')?.innerText?.trim() || null;
  const sectionInventory = [...document.querySelectorAll('h6, .subsection-label')]
    .map(node => (node.innerText || node.textContent || '').trim()).filter(Boolean);
  const pitchMarkerCount = sectionInventory.filter(text => /pitch|accent/i.test(text)).length;
  let graphCount = 0;
  if (section) {
    const subsection = section.querySelector('.subsection');
    const column = subsection?.firstElementChild;
    if (column) {
      graphCount = [...column.children].reduce((count, row) => count + [...row.children].filter(node => {
        if (!(node instanceof HTMLElement) || !node.matches('div[style*="word-break: keep-all"]')) return false;
        return [...node.querySelectorAll(':scope > div')].some(piece => (piece.getAttribute('style') || '').includes('--pitch-'));
      }).length, 0);
    }
  }
  const partOfSpeech = [...meanings.querySelectorAll('.part-of-speech')]
    .map(node => (node.innerText || node.textContent || '').trim()).filter(Boolean);
  const meaningsLabel = meanings.querySelector('.subsection-label')?.innerText?.trim() || '';
  const hasMeaningDescription = Boolean(meanings.querySelector('.description'));
  return {
    url: location.href,
    search_language: lang.value,
    forms,
    part_of_speech: partOfSpeech,
    section_inventory: sectionInventory,
    base_page_contract_valid: meaningsLabel === 'Meanings' && partOfSpeech.length > 0 && hasMeaningDescription,
    pitch_section_present: Boolean(section),
    pitch_label: label,
    pitch_marker_count: pitchMarkerCount,
    graph_count: graphCount,
  };
}"#;

const CAPTURE_SNAPSHOT_SCRIPT: &str = r#"() => {
  const section = document.querySelector('.subsection-pitch-accent');
  const subsection = section?.querySelector('.subsection');
  const column = subsection?.firstElementChild;
  const lang = document.querySelector('#search-bar-lang');
  if (!section || !column || !lang || lang.value !== 'english') return { error: 'pitch section, graph column, or English UI is unavailable' };
  section.scrollIntoView({ block: 'center', inline: 'nearest' });
  const graphs = [];
  [...column.children].forEach((row, rowIndex) => {
    [...row.children].forEach(node => {
      if (!(node instanceof HTMLElement) || !node.matches('div[style*="word-break: keep-all"]')) return;
      const pitchPieces = [...node.querySelectorAll(':scope > div')].filter(piece => (piece.getAttribute('style') || '').includes('--pitch-'));
      if (!pitchPieces.length) return;
      const rect = node.getBoundingClientRect();
      const selector = `.subsection-pitch-accent .subsection > div > div:nth-child(${rowIndex + 1}) > div[style*="word-break: keep-all"]`;
      graphs.push({ selector, x: rect.x, y: rect.y, width: rect.width, height: rect.height });
    });
  });
  if (!graphs.length) return { error: 'pitch section exists but no expected graph DOM was recognized' };
  const left = Math.min(...graphs.map(graph => graph.x));
  const top = Math.min(...graphs.map(graph => graph.y));
  const right = Math.max(...graphs.map(graph => graph.x + graph.width));
  const bottom = Math.max(...graphs.map(graph => graph.y + graph.height));
  const root = document.documentElement;
  const style = getComputedStyle(root);
  return {
    graphs,
    rect: { x: left, y: top, width: right - left, height: bottom - top },
    viewport_width: window.innerWidth,
    viewport_height: window.innerHeight,
    page_scale_factor: window.visualViewport?.scale ?? NaN,
    device_pixel_ratio: window.devicePixelRatio,
    dark_theme: {
      document_element_classes: [...root.classList],
      prefers_color_scheme: matchMedia('(prefers-color-scheme: dark)').matches ? 'dark' : 'light',
      computed_color_scheme: style.colorScheme,
    },
  };
}"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn form(surface: &str, reading: &str) -> JpdbVocabularyForm {
        JpdbVocabularyForm {
            surface: surface.into(),
            reading: reading.into(),
        }
    }

    #[test]
    fn search_url_uses_url_api_and_percent_encodes_surface() {
        let url = Url::parse(&build_search_url("元気 & 元気?").unwrap()).unwrap();
        assert_eq!(url.origin().ascii_serialization(), JPDB_ORIGIN);
        assert_eq!(url.path(), "/search");
        assert_eq!(
            url.query_pairs().find(|(key, _)| key == "q").unwrap().1,
            "元気 & 元気?"
        );
        assert!(url.as_str().contains("%E5%85%83%E6%B0%97"));
        assert!(url.as_str().contains("%26"));
    }

    #[test]
    fn detail_route_requires_jpdb_vocabulary_path_and_positive_id() {
        assert_eq!(
            parse_detail_route("https://jpdb.io/vocabulary/1540590/幽霊/ゆうれい#a"),
            Some(DetailRoute {
                vocabulary_id: 1_540_590
            })
        );
        assert_eq!(
            parse_detail_route("https://jpdb.io/vocabulary/0/test"),
            None
        );
        assert_eq!(
            parse_detail_route("https://jpdb.io/vocabulary/1540590/used-in"),
            None
        );
        assert_eq!(
            parse_detail_route("https://jpdb.io/vocabulary/1540590/幽霊/used-in"),
            None
        );
        assert_eq!(
            parse_detail_route("http://jpdb.io/vocabulary/1540590/test"),
            None
        );
        assert_eq!(
            parse_detail_route("https://example.com/vocabulary/1540590/test"),
            None
        );
    }

    #[test]
    fn only_exact_surface_and_optional_exact_reading_match_candidates() {
        let rows = vec![
            RawVocabularyCandidate {
                detail_url: Some("https://jpdb.io/vocabulary/10/元気/げんき".into()),
                forms: vec![form("元気", "げんき"), form("ゲンキ", "げんき")],
                part_of_speech: vec!["Adjective (な)".into()],
            },
            RawVocabularyCandidate {
                detail_url: Some("https://jpdb.io/vocabulary/11/元気/げんき".into()),
                forms: vec![form("元気", "げんき")],
                part_of_speech: vec!["Name".into()],
            },
            RawVocabularyCandidate {
                detail_url: Some("https://jpdb.io/vocabulary/12/元気/もとき".into()),
                forms: vec![form("元気", "もとき")],
                part_of_speech: vec!["Noun".into()],
            },
            RawVocabularyCandidate {
                detail_url: Some("https://jpdb.io/vocabulary/13/元気/げんき".into()),
                forms: vec![form("元気屋", "げんきや")],
                part_of_speech: vec!["Noun".into()],
            },
            RawVocabularyCandidate {
                detail_url: Some("https://jpdb.io/vocabulary/14/猫/ねこ".into()),
                forms: vec![form("猫", "ねこ"), form("ネコ", "ねこ")],
                part_of_speech: vec!["Noun".into()],
            },
        ];
        let matched =
            select_candidates(&rows, &JpdbPitchQuery::new("元気", Some("げんき".into()))).unwrap();
        assert_eq!(matched.len(), 1);
        assert_eq!(matched[0].vocabulary_id, 10);
        let ambiguous = select_candidates(&rows, &JpdbPitchQuery::new("元気", None)).unwrap();
        assert_eq!(ambiguous.len(), 2);
        assert_eq!(ambiguous[0].vocabulary_id, 10);
        assert_eq!(ambiguous[1].vocabulary_id, 12);
        assert!(
            select_candidates(&rows, &JpdbPitchQuery::new("げんき", None))
                .unwrap()
                .is_empty()
        );
        let kana_alt =
            select_candidates(&rows, &JpdbPitchQuery::new("ネコ", Some("ねこ".into()))).unwrap();
        assert_eq!(kana_alt.len(), 1);
        assert_eq!(kana_alt[0].vocabulary_id, 14);
    }

    #[test]
    fn candidate_rows_require_a_real_matching_detail_href() {
        let rows = vec![RawVocabularyCandidate {
            detail_url: Some("https://jpdb.io/vocabulary/12/元気/used-in".into()),
            forms: vec![form("元気", "げんき")],
            part_of_speech: vec!["Noun".into()],
        }];
        assert!(matches!(
            select_candidates(&rows, &JpdbPitchQuery::new("元気", None)),
            Err(JpdbPitchFailure::PageContract { .. })
        ));
    }

    #[test]
    fn not_found_requires_explicit_empty_search_evidence() {
        let explicit_empty = RawSearchSnapshot {
            ready: true,
            no_results_found: true,
            result_count: 0,
            truncated: false,
            candidates: Vec::new(),
        };
        assert!(validate_search_inventory(&explicit_empty).is_ok());
        let unresolved_empty = RawSearchSnapshot {
            ready: false,
            no_results_found: false,
            result_count: 0,
            truncated: false,
            candidates: Vec::new(),
        };
        assert!(matches!(
            validate_search_inventory(&unresolved_empty),
            Err(JpdbPitchFailure::PageContract { .. })
        ));
        let capped = RawSearchSnapshot {
            ready: true,
            no_results_found: false,
            result_count: 129,
            truncated: true,
            candidates: Vec::new(),
        };
        assert!(matches!(
            validate_search_inventory(&capped),
            Err(JpdbPitchFailure::PageContract { .. })
        ));
    }

    #[test]
    fn surface_and_reading_can_be_confirmed_by_distinct_forms_of_one_entry() {
        let forms = vec![
            form("ネコ", "ネコ"),
            form("猫", "ねこ"),
            form("ねこ", "ねこ"),
        ];
        let query = JpdbPitchQuery::new("ネコ", Some("ねこ".into()));

        assert!(forms_match_query(&forms, &query));
        assert_eq!(
            resolve_query_reading(&forms, &query).as_deref(),
            Some("ねこ")
        );
    }

    #[test]
    fn pitch_absence_requires_a_verified_detail_and_missing_section() {
        let detail = DetailSnapshot {
            url: "https://jpdb.io/vocabulary/1540590/幽霊/ゆうれい".into(),
            vocabulary_id: 1_540_590,
            forms: vec![form("幽霊", "ゆうれい")],
            part_of_speech: vec!["Noun".into()],
            section_inventory: vec!["Meanings".into(), "Kanji used".into()],
            base_page_contract_valid: true,
            pitch_section_present: false,
            pitch_label: None,
            pitch_marker_count: 0,
            graph_count: 0,
        };
        assert!(detail_matches_query(
            &detail,
            &JpdbPitchQuery::new("幽霊", None)
        ));
        assert!(classify_pitch_state(&detail).is_ok_and(|state| state == PitchDomState::Absent));

        let mut unexpected_empty_section = detail.clone();
        unexpected_empty_section.pitch_section_present = true;
        unexpected_empty_section.pitch_label = Some("Pitch accent".into());
        unexpected_empty_section.pitch_marker_count = 1;
        assert!(matches!(
            classify_pitch_state(&unexpected_empty_section),
            Err(JpdbPitchFailure::PageContract { .. })
        ));

        let mut selector_drift = detail.clone();
        selector_drift.pitch_marker_count = 1;
        selector_drift.section_inventory.push("Pitch accent".into());
        assert!(matches!(
            classify_pitch_state(&selector_drift),
            Err(JpdbPitchFailure::PageContract { .. })
        ));

        let mut incomplete_base_contract = detail.clone();
        incomplete_base_contract.base_page_contract_valid = false;
        assert!(matches!(
            classify_pitch_state(&incomplete_base_contract),
            Err(JpdbPitchFailure::PageContract { .. })
        ));

        let mut wrong_identity = detail;
        wrong_identity.vocabulary_id = 99;
        assert!(!detail_matches_query(
            &wrong_identity,
            &JpdbPitchQuery::new("幽霊", None)
        ));
    }

    #[test]
    fn current_page_recognizes_search_and_detail_only_on_jpdb() {
        assert!(is_search_url("https://jpdb.io/search?q=%E5%B9%BD%E9%9C%8A"));
        assert!(!is_search_url(
            "https://jpdb.io/vocabulary/1540590/幽霊/ゆうれい"
        ));
        assert!(parse_detail_route("https://jpdb.io/vocabulary/1540590/幽霊/ゆうれい").is_some());
        assert!(parse_detail_route("https://jpdb.io/vocabulary/1540590/幽霊").is_some());
        assert!(
            parse_detail_route("https://jpdb.io/vocabulary/1540590/幽霊/ゆうれい?x=1#a").is_some()
        );
    }

    #[test]
    fn readiness_ignores_unrelated_subdocuments_but_tracks_main_navigation() {
        use chromiumoxide::cdp::browser_protocol::network::ResourceType;

        assert!(is_critical_resource(
            &ResourceType::Document,
            Some("https://analytics.example/frame"),
            true,
        ));
        assert!(!is_critical_resource(
            &ResourceType::Document,
            Some("https://analytics.example/frame"),
            false,
        ));
        assert!(!is_critical_resource(
            &ResourceType::Script,
            Some("https://analytics.example/widget.js"),
            false,
        ));
        assert!(is_critical_resource(
            &ResourceType::Script,
            Some("https://jpdb.io/assets/application.js"),
            false,
        ));
        assert!(!is_critical_resource(
            &ResourceType::Image,
            Some("https://jpdb.io/images/background.png"),
            false,
        ));
    }

    #[test]
    fn graph_union_must_be_in_viewport_at_one_to_one_page_scale_and_three_x() {
        assert!(
            validate_capture_geometry(
                PitchAccentCaptureRect {
                    x: 10.0,
                    y: 20.0,
                    width: 100.0,
                    height: 30.0
                },
                1280,
                1200,
                1.0,
                3.0,
            )
            .is_ok()
        );
        assert!(
            validate_capture_geometry(
                PitchAccentCaptureRect {
                    x: 10.0,
                    y: 20.0,
                    width: 100.0,
                    height: 30.0
                },
                1280,
                1200,
                1.1,
                3.0,
            )
            .is_err()
        );
        assert!(
            validate_capture_geometry(
                PitchAccentCaptureRect {
                    x: 10.0,
                    y: 20.0,
                    width: 100.0,
                    height: 30.0
                },
                1280,
                1200,
                1.0,
                2.0,
            )
            .is_err()
        );
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PitchDomState {
    Absent,
    Present { graph_count: usize },
}

fn classify_pitch_state(detail: &DetailSnapshot) -> Result<PitchDomState, JpdbPitchFailure> {
    if detail.vocabulary_id == 0
        || detail.forms.is_empty()
        || detail.part_of_speech.is_empty()
        || !detail.base_page_contract_valid
        || !detail
            .section_inventory
            .iter()
            .any(|label| label == "Meanings")
        || !parse_detail_route(&detail.url)
            .is_some_and(|route| route.vocabulary_id == detail.vocabulary_id)
        || detail
            .part_of_speech
            .iter()
            .any(|pos| pos.trim().eq_ignore_ascii_case("name"))
    {
        return Err(JpdbPitchFailure::PageContract {
            stage: JpdbPitchStage::DetailVerification,
            message: "detail identity and base vocabulary page contract are not proven".into(),
        });
    }
    if !detail.pitch_section_present {
        if detail.pitch_marker_count != 0 {
            return Err(JpdbPitchFailure::PageContract {
                stage: JpdbPitchStage::PitchInspection,
                message: "pitch-related section text exists outside the recognized pitch container"
                    .into(),
            });
        }
        return Ok(PitchDomState::Absent);
    }
    if detail.pitch_label.as_deref() != Some("Pitch accent")
        || detail.pitch_marker_count != 1
        || detail.graph_count == 0
    {
        return Err(JpdbPitchFailure::PageContract {
            stage: JpdbPitchStage::PitchInspection,
            message: "pitch section exists but its label or graph DOM is missing".into(),
        });
    }
    Ok(PitchDomState::Present {
        graph_count: detail.graph_count,
    })
}
