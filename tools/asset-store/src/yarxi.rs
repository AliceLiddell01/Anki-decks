//! Browser provider for the dynamic Yarxi article and its left-hand media.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
use chromiumoxide::{
    Browser, BrowserConfig, Page,
    cdp::browser_protocol::page::{
        CaptureScreenshotFormat, EnableParams, GetResourceContentParams, GetResourceTreeParams,
    },
    cdp::browser_protocol::{
        emulation::{MediaFeature, SetDeviceMetricsOverrideParams, SetEmulatedMediaParams},
        network::{
            EnableParams as NetworkEnableParams, EventLoadingFailed, EventLoadingFinished,
            EventRequestWillBeSent, EventResponseReceived, ResourceType,
        },
    },
    cdp::js_protocol::runtime::{EnableParams as RuntimeEnableParams, EventExceptionThrown},
};
use futures::StreamExt;
use image::{GenericImageView, RgbaImage};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::time::{sleep, timeout};
use url::Url;

const PROVIDER_ID: &str = "yarxi-suu-browser";
const PROVIDER_VERSION: &str = "6";
const SITE_URL: &str = "https://www.yarxi.su/";
const SITE_HOST: &str = "www.yarxi.su";
const OPERATION_TIMEOUT: Duration = Duration::from_secs(20 * 60);
const BATCH_PACING: Duration = Duration::from_millis(900);
const CAPTURE_VIEWPORT_WIDTH: i64 = 1280;
const CAPTURE_VIEWPORT_HEIGHT: i64 = 900;
const CAPTURE_DEVICE_SCALE_FACTOR: f64 = 2.175;
const CAPTURE_MIN_EDGE_PX: u32 = 140;
const CAPTURE_MAX_EDGE_PX: u32 = 220;
const CAPTURE_LIGHT_GLYPH_MIN_LUMINANCE: f64 = 0.60;
const YARXI_DARK_THEME_STYLE_ID: &str = "asset-store-yarxi-dark-theme";
const YARXI_DARK_THEME_STYLE: &str = ":root { color-scheme: dark !important; } #app { color: rgb(var(--w-base-color-rgb)) !important; }";
const MAX_TRACKED_REQUESTS: usize = 256;
const MAX_NETWORK_OUTCOMES: usize = 256;
const MAX_NETWORK_DIAGNOSTIC_ITEMS: usize = 3;
const MAX_NETWORK_DIAGNOSTIC_PATH_CHARS: usize = 160;

/// Источник bytes и bounded evidence browser acquisition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SelectionResult {
    PrimaryGif,
    LeftmostPngFallback,
    RenderedFontSamplePng,
}

/// Explicit acquisition intent. `PreferredSource` is the production default;
/// `RenderedFontSamplePng` exists for a deliberate browser-render acceptance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AcquisitionTarget {
    #[default]
    PreferredSource,
    RenderedFontSamplePng,
}

/// Полученный оригинальный бинарный asset.
#[derive(Debug, Clone, PartialEq)]
pub struct AcquiredMedia {
    pub character: String,
    pub source_url: String,
    pub article_unicode: String,
    pub selection: SelectionResult,
    pub bytes: Vec<u8>,
    pub evidence: AcquisitionEvidence,
}

/// Сохраняемое evidence выбора источника, без browser/session state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcquisitionEvidence {
    pub provider: String,
    pub provider_version: String,
    pub article_unicode: String,
    pub article_number: Option<u32>,
    pub frequency_index: Option<u32>,
    pub selection: SelectionResult,
    #[serde(default)]
    pub target: AcquisitionTarget,
    pub fallback_absence_proof: Option<String>,
    pub rendered_font_sample: Option<RenderedFontSampleEvidence>,
    pub source_url: String,
}

/// Evidence for a PNG rasterized from one visible Yarxi font sample DOM element.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RenderedFontSampleEvidence {
    pub index: usize,
    pub selector: String,
    pub class_name: String,
    pub title: String,
    pub text: String,
    pub font_family: String,
    pub font_size: String,
    pub capture: String,
    pub css_rect: CssRect,
    pub pixel_width: u32,
    pub pixel_height: u32,
    pub viewport_width: u32,
    pub viewport_height: u32,
    pub device_scale_factor: f64,
    pub page_scale_factor: f64,
    pub prefers_color_scheme_dark: bool,
    pub dark_environment: String,
    pub document_theme: String,
    pub foreground_color: String,
    pub tile_background_color: String,
    pub page_background_color: String,
    pub effective_background_color: String,
    pub border_color: String,
    pub border_style: String,
    pub border_width: String,
    pub border_visible: bool,
    pub box_shadow: String,
    pub inline_style_unchanged: bool,
    pub dark_background_pixels: u32,
    pub light_glyph_pixels: u32,
    pub frame_pixels: u32,
    pub measured_dark_background_rgb: String,
    pub measured_light_glyph_rgb: String,
    pub measured_frame_rgb: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct CssRect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// Чёткое состояние primary GIF для policy/fake tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrimaryGifState {
    PresentLoaded { url: String },
    AbsentConfirmed { proof: String },
    Unknown { reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ImageElementState {
    url: String,
    complete: bool,
    width: u32,
    height: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct NetworkRequestState {
    resource_type: ResourceType,
    url: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct NetworkOutcome {
    resource_type: ResourceType,
    url: Option<String>,
    failure_reason: Option<String>,
}

#[derive(Debug, Default)]
struct BrowserRuntimeEvidence {
    relevant_requests: HashMap<String, NetworkRequestState>,
    network_failures: Vec<NetworkOutcome>,
    http_errors: Vec<NetworkOutcome>,
    javascript_exceptions: u32,
    monitor_failed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RuntimeReadiness {
    pending_relevant_requests: usize,
    network_failures: u32,
    http_errors: u32,
    javascript_exceptions: u32,
    monitor_failed: bool,
}

impl BrowserRuntimeEvidence {
    fn readiness(&self, excluded_primary_gif: Option<&str>) -> RuntimeReadiness {
        RuntimeReadiness {
            pending_relevant_requests: self
                .relevant_requests
                .values()
                .filter(|request| {
                    request.resource_type != ResourceType::Image
                        || Some(request.url.as_str()) != excluded_primary_gif
                })
                .count(),
            network_failures: self
                .network_failures
                .iter()
                .filter(|failure| !is_excluded_primary_gif(failure, excluded_primary_gif))
                .count() as u32,
            http_errors: self
                .http_errors
                .iter()
                .filter(|failure| !is_excluded_primary_gif(failure, excluded_primary_gif))
                .count() as u32,
            javascript_exceptions: self.javascript_exceptions,
            monitor_failed: self.monitor_failed,
        }
    }

    fn font_sample_readiness(&self, selected_image_urls: &HashSet<String>) -> RuntimeReadiness {
        RuntimeReadiness {
            pending_relevant_requests: self
                .relevant_requests
                .values()
                .filter(|request| {
                    is_relevant_font_sample_resource(
                        &request.resource_type,
                        Some(&request.url),
                        selected_image_urls,
                    )
                })
                .count(),
            network_failures: self
                .network_failures
                .iter()
                .filter(|failure| {
                    is_relevant_font_sample_resource(
                        &failure.resource_type,
                        failure.url.as_deref(),
                        selected_image_urls,
                    )
                })
                .count() as u32,
            http_errors: self
                .http_errors
                .iter()
                .filter(|failure| {
                    is_relevant_font_sample_resource(
                        &failure.resource_type,
                        failure.url.as_deref(),
                        selected_image_urls,
                    )
                })
                .count() as u32,
            javascript_exceptions: self.javascript_exceptions,
            monitor_failed: self.monitor_failed,
        }
    }
}

fn is_relevant_font_sample_resource(
    resource_type: &ResourceType,
    url: Option<&str>,
    selected_image_urls: &HashSet<String>,
) -> bool {
    resource_type != &ResourceType::Image
        || url.is_none_or(|url| url.is_empty() || selected_image_urls.contains(url))
}

fn is_excluded_primary_gif(outcome: &NetworkOutcome, excluded_primary_gif: Option<&str>) -> bool {
    outcome.resource_type == ResourceType::Image
        && outcome
            .url
            .as_deref()
            .is_some_and(|url| Some(url) == excluded_primary_gif)
}

fn sanitized_network_location(raw_url: &str) -> String {
    let Ok(url) = Url::parse(raw_url) else {
        return "<unparseable URL>".into();
    };
    if !matches!(url.scheme(), "http" | "https") || !url.origin().is_tuple() {
        return format!("<{} resource>", url.scheme());
    }
    let raw_path = if url.path().is_empty() {
        "/"
    } else {
        url.path()
    };
    let mut path = raw_path
        .chars()
        .take(MAX_NETWORK_DIAGNOSTIC_PATH_CHARS)
        .collect::<String>();
    if raw_path.chars().count() > MAX_NETWORK_DIAGNOSTIC_PATH_CHARS {
        path.push('…');
    }
    format!("{}{path}", url.origin().ascii_serialization())
}

fn sanitized_network_failure_reason(raw_reason: &str) -> String {
    let upper = raw_reason.trim().to_ascii_uppercase();
    let Some(suffix) = upper.strip_prefix("NET::ERR_") else {
        return "unclassified network error".into();
    };
    if suffix.is_empty()
        || !suffix
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
    {
        return "unclassified network error".into();
    }
    format!("net::ERR_{suffix}")
}

#[derive(Debug)]
struct BrowserEvidenceMonitor {
    state: Arc<Mutex<BrowserRuntimeEvidence>>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl BrowserEvidenceMonitor {
    fn readiness(&self, excluded_primary_gif: Option<&str>) -> RuntimeReadiness {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .readiness(excluded_primary_gif)
    }

    fn font_sample_readiness(&self, selected_image_urls: &HashSet<String>) -> RuntimeReadiness {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .font_sample_readiness(selected_image_urls)
    }

    fn clear_explicitly_approved_tls_interstitial_failure(&self) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .network_failures
            .retain(|failure| {
                !(failure.resource_type == ResourceType::Document
                    && failure.url.as_deref() == Some(SITE_URL)
                    && failure.failure_reason.as_deref().is_some_and(|reason| {
                        reason.eq_ignore_ascii_case("net::ERR_CERT_AUTHORITY_INVALID")
                    }))
            });
    }

    fn network_failure_details(&self, excluded_primary_gif: Option<&str>) -> String {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let failures: Vec<_> = state
            .network_failures
            .iter()
            .filter(|failure| !is_excluded_primary_gif(failure, excluded_primary_gif))
            .collect();
        if failures.is_empty() {
            return "none".into();
        }
        let mut details = failures
            .iter()
            .take(MAX_NETWORK_DIAGNOSTIC_ITEMS)
            .map(|failure| {
                let location = failure
                    .url
                    .as_deref()
                    .map(sanitized_network_location)
                    .unwrap_or_else(|| "<url unavailable>".into());
                let reason = failure
                    .failure_reason
                    .as_deref()
                    .map(sanitized_network_failure_reason)
                    .unwrap_or_else(|| "unclassified network error".into());
                format!("{:?} {location} {reason}", failure.resource_type)
            })
            .collect::<Vec<_>>();
        if failures.len() > MAX_NETWORK_DIAGNOSTIC_ITEMS {
            details.push(format!(
                "+{} more network failures",
                failures.len() - MAX_NETWORK_DIAGNOSTIC_ITEMS
            ));
        }
        details.join("; ")
    }

    fn font_sample_network_failure_details(&self, selected_image_urls: &HashSet<String>) -> String {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let failures: Vec<_> = state
            .network_failures
            .iter()
            .filter(|failure| {
                is_relevant_font_sample_resource(
                    &failure.resource_type,
                    failure.url.as_deref(),
                    selected_image_urls,
                )
            })
            .collect();
        if failures.is_empty() {
            return "none".into();
        }
        let mut details = failures
            .iter()
            .take(MAX_NETWORK_DIAGNOSTIC_ITEMS)
            .map(|failure| {
                let location = failure
                    .url
                    .as_deref()
                    .map(sanitized_network_location)
                    .unwrap_or_else(|| "<url unavailable>".into());
                let reason = failure
                    .failure_reason
                    .as_deref()
                    .map(sanitized_network_failure_reason)
                    .unwrap_or_else(|| "unclassified network error".into());
                format!("{:?} {location} {reason}", failure.resource_type)
            })
            .collect::<Vec<_>>();
        if failures.len() > MAX_NETWORK_DIAGNOSTIC_ITEMS {
            details.push(format!(
                "+{} more network failures",
                failures.len() - MAX_NETWORK_DIAGNOSTIC_ITEMS
            ));
        }
        details.join("; ")
    }

    fn abort(self) {
        for task in self.tasks {
            task.abort();
        }
    }
}

fn classify_primary_state(
    primary: Option<&ImageElementState>,
    information_area_ready: bool,
    dom_stable: bool,
    all_images_complete: bool,
    runtime: RuntimeReadiness,
) -> PrimaryGifState {
    if let Some(image) = primary {
        if image.complete && image.width > 0 && image.height > 0 && !image.url.is_empty() {
            return PrimaryGifState::PresentLoaded {
                url: image.url.clone(),
            };
        }
        return PrimaryGifState::Unknown {
            reason: if image.complete {
                "primary GIF exists but natural dimensions are zero (failed load)".into()
            } else {
                "primary GIF element exists and is still loading".into()
            },
        };
    }
    if !runtime_is_clean(runtime) {
        return PrimaryGifState::Unknown {
            reason: runtime_failure_reason(runtime),
        };
    }
    if information_area_ready && dom_stable && all_images_complete {
        PrimaryGifState::AbsentConfirmed {
            proof: format!(
                "information area ready; article Unicode exact; all DOM images complete; DOM stable for 2 seconds; no img.kakijun-gif; CDP Network/Runtime clean (pending={}, failed={}, HTTP errors={}, JS exceptions={})",
                runtime.pending_relevant_requests,
                runtime.network_failures,
                runtime.http_errors,
                runtime.javascript_exceptions
            ),
        }
    } else {
        PrimaryGifState::Unknown {
            reason: "information/media area is not yet complete and stable".into(),
        }
    }
}

fn runtime_is_clean(runtime: RuntimeReadiness) -> bool {
    !runtime.monitor_failed
        && runtime.pending_relevant_requests == 0
        && runtime.network_failures == 0
        && runtime.http_errors == 0
        && runtime.javascript_exceptions == 0
}

fn runtime_failure_reason(runtime: RuntimeReadiness) -> String {
    format!(
        "network/runtime path is not clean (monitor_failed={}, pending={}, failed={}, HTTP errors={}, JS exceptions={})",
        runtime.monitor_failed,
        runtime.pending_relevant_requests,
        runtime.network_failures,
        runtime.http_errors,
        runtime.javascript_exceptions
    )
}

#[derive(Debug, Clone, PartialEq)]
enum MediaSourceChoice {
    Url {
        selection: SelectionResult,
        url: String,
        absence_proof: Option<String>,
    },
    RenderedFontSample {
        sample: FontSampleState,
        absence_proof: Option<String>,
    },
}

/// Выбирает raw image bytes или точный видимый font sample как PNG raster.
/// Right-side kakijun URL никогда не входит в допустимые источники.
#[cfg(test)]
fn choose_media_source(
    primary: PrimaryGifState,
    font_samples: &[FontSampleState],
    expected_character: &str,
) -> Result<MediaSourceChoice, String> {
    choose_media_source_for_target(
        primary,
        font_samples,
        expected_character,
        AcquisitionTarget::PreferredSource,
    )
}

fn choose_media_source_for_target(
    primary: PrimaryGifState,
    font_samples: &[FontSampleState],
    expected_character: &str,
    target: AcquisitionTarget,
) -> Result<MediaSourceChoice, String> {
    if target == AcquisitionTarget::RenderedFontSamplePng {
        return Ok(MediaSourceChoice::RenderedFontSample {
            sample: choose_font_sample(font_samples, expected_character)?,
            absence_proof: None,
        });
    }
    match primary {
        PrimaryGifState::PresentLoaded { url } if target == AcquisitionTarget::PreferredSource => {
            Ok(MediaSourceChoice::Url {
                selection: SelectionResult::PrimaryGif,
                url,
                absence_proof: None,
            })
        }
        PrimaryGifState::PresentLoaded { .. } => unreachable!("preferred GIF returned above"),
        PrimaryGifState::AbsentConfirmed { proof } => {
            let sample = choose_font_sample(font_samples, expected_character)?;
            if let Some(image) = sample.images.first() {
                if !image.complete || image.width == 0 || image.height == 0 || image.url.is_empty()
                {
                    return Err(
                        "fallback_font_sample_image_unavailable: левое изображение образца не загрузилось"
                            .into(),
                    );
                }
                let parsed = Url::parse(&image.url).map_err(
                    |_| "fallback_url_invalid: URL крайнего левого изображения некорректен",
                )?;
                if parsed.host_str().is_some_and(is_kakijun_host) {
                    return Err(
                        "kakijun_source_forbidden: kakijun.jp не является PNG fallback".into(),
                    );
                }
                return Ok(MediaSourceChoice::Url {
                    selection: SelectionResult::LeftmostPngFallback,
                    url: image.url.clone(),
                    absence_proof: Some(proof),
                });
            }
            Ok(MediaSourceChoice::RenderedFontSample {
                sample,
                absence_proof: Some(proof),
            })
        }
        PrimaryGifState::Unknown { reason } => Err(format!(
            "gif_unknown: нельзя разрешить image/font-sample fallback: {reason}"
        )),
    }
}

fn choose_font_sample(
    font_samples: &[FontSampleState],
    expected_character: &str,
) -> Result<FontSampleState, String> {
    font_samples
        .iter()
        .filter(|sample| {
            sample.visible
                && !sample
                    .class_name
                    .split_whitespace()
                    .any(|class| class.starts_with("stroke-order"))
                && sample.width > 0.0
                && sample.height > 0.0
                && (sample.text.trim() == expected_character
                    || (!sample.images.is_empty() && sample.text.trim().is_empty()))
        })
        .min_by(|left, right| {
            left.left
                .total_cmp(&right.left)
                .then_with(|| left.top.total_cmp(&right.top))
        })
        .cloned()
        .ok_or_else(|| {
            "fallback_font_sample_missing: нет видимого font-sample с точным текстом кандзи"
                .to_owned()
        })
}

/// Выполняет один изолированный browser batch. Certificate interstitial можно
/// пропустить только при явном флаге, только на точном host Yarxi.
pub fn acquire_many(
    characters: &[String],
    allow_insecure_tls: bool,
) -> Result<Vec<Result<AcquiredMedia, String>>, String> {
    acquire_many_with_target(
        characters,
        allow_insecure_tls,
        AcquisitionTarget::PreferredSource,
    )
}

/// Acquires media with an explicitly selected source policy. Acceptance tools
/// may request a rendered font-sample PNG; regular callers should use
/// [`acquire_many`] and retain the primary-GIF preference.
pub fn acquire_many_with_target(
    characters: &[String],
    allow_insecure_tls: bool,
    target: AcquisitionTarget,
) -> Result<Vec<Result<AcquiredMedia, String>>, String> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|error| format!("browser runtime: {error}"))?;
    runtime
        .block_on(async move { acquire_many_async(characters, allow_insecure_tls, target).await })
}

async fn acquire_many_async(
    characters: &[String],
    allow_insecure_tls: bool,
    target: AcquisitionTarget,
) -> Result<Vec<Result<AcquiredMedia, String>>, String> {
    let mut config = BrowserConfig::builder()
        .incognito()
        .respect_https_errors()
        .launch_timeout(Duration::from_secs(30))
        .request_timeout(Duration::from_secs(30));
    if let Some(executable) = find_browser_executable() {
        config = config.chrome_executable(executable);
    }
    let config = config
        .build()
        .map_err(|error| format!("browser config: {error}"))?;
    let (mut browser, mut handler) = Browser::launch(config)
        .await
        .map_err(|error| format!("browser launch: {error}"))?;
    let handler_task = tokio::spawn(async move {
        while let Some(event) = handler.next().await {
            if event.is_err() {
                break;
            }
        }
    });

    let operation = timeout(OPERATION_TIMEOUT, async {
        let page = browser
            .new_page("about:blank")
            .await
            .map_err(|error| format!("browser page: {error}"))?;
        page.execute(EnableParams::default())
            .await
            .map_err(|error| format!("CDP Page.enable: {error}"))?;
        page.execute(SetDeviceMetricsOverrideParams::new(
            CAPTURE_VIEWPORT_WIDTH,
            CAPTURE_VIEWPORT_HEIGHT,
            CAPTURE_DEVICE_SCALE_FACTOR,
            false,
        ))
        .await
        .map_err(|error| format!("CDP Emulation.setDeviceMetricsOverride: {error}"))?;
        page.execute(
            SetEmulatedMediaParams::builder()
                .feature(MediaFeature::new("prefers-color-scheme", "dark"))
                .build(),
        )
        .await
        .map_err(|error| format!("CDP Emulation.setEmulatedMedia: {error}"))?;
        let evidence_monitor = start_browser_evidence_monitor(&page).await?;
        let tls_interstitial_approved = match page.goto(SITE_URL).await {
            Err(error) => {
                async_error_or_tls_interstitial(&page, allow_insecure_tls, error).await?;
                true
            }
            Ok(_) => {
                let tls_probe: Value = page
                    .evaluate("() => ({ code: document.querySelector('#error-code')?.textContent?.trim() || '', proceed: Boolean(document.querySelector('#proceed-link')) })")
                    .await
                    .map_err(|error| format!("TLS page check: {error}"))?
                    .into_value()
                    .map_err(|error| format!("TLS page check result: {error}"))?;
                if !tls_probe["code"].as_str().unwrap_or_default().is_empty() {
                    async_error_or_tls_interstitial(
                        &page,
                        allow_insecure_tls,
                        tls_probe["code"].as_str().unwrap_or_default(),
                    )
                    .await?;
                    true
                } else {
                    false
                }
            }
        };
        let site_host: bool = page
            .evaluate(format!("location.hostname === {SITE_HOST:?}"))
            .await
            .map_err(|error| format!("Yarxi host check: {error}"))?
            .into_value()
            .map_err(|error| format!("Yarxi host check result: {error}"))?;
        if !site_host {
            return Err("provider_host_mismatch: загрузка ушла с www.yarxi.su".into());
        }
        wait_until(&page, Duration::from_secs(20), || {
            "Boolean([...document.querySelectorAll('.kanji-search-form input[placeholder=\"Чтение\"]')].find(node => node.getClientRects().length > 0 && getComputedStyle(node).visibility !== 'hidden'))"
        })
        .await?;
        if tls_interstitial_approved {
            evidence_monitor.clear_explicitly_approved_tls_interstitial_failure();
        }
        if target == AcquisitionTarget::RenderedFontSamplePng {
            apply_dark_theme(&page).await?;
        }

        let mut outcomes = Vec::with_capacity(characters.len());
        for (index, character) in characters.iter().enumerate() {
            if index > 0 {
                sleep(BATCH_PACING).await;
            }
            outcomes.push(acquire_one(&page, character, target, &evidence_monitor).await);
        }
        evidence_monitor.abort();
        Ok::<_, String>(outcomes)
    })
    .await
    .map_err(|_| "browser operation timeout".to_owned())
    .and_then(|result| result);

    let close_result = browser.close().await;
    handler_task.abort();
    if let Err(error) = close_result
        && operation.is_ok()
    {
        return Err(format!("browser cleanup: {error}"));
    }
    operation
}

fn find_browser_executable() -> Option<PathBuf> {
    for variable in ["CHROME_BIN", "CHROMIUM_BIN"] {
        if let Some(path) = std::env::var_os(variable).map(PathBuf::from)
            && is_executable_file(&path)
        {
            return Some(path);
        }
    }
    if let Some(path_entries) = std::env::var_os("PATH") {
        for directory in std::env::split_paths(&path_entries) {
            for name in ["chromium", "chromium-browser", "google-chrome", "chrome"] {
                let path = directory.join(name);
                if is_executable_file(&path) {
                    return Some(path);
                }
            }
        }
    }
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    let cache = home.join(".cache/ms-playwright");
    let mut candidates = Vec::new();
    let entries = std::fs::read_dir(cache).ok()?;
    for entry in entries.flatten() {
        let name = entry.file_name();
        if !name.to_string_lossy().starts_with("chromium-") {
            continue;
        }
        candidates.push(entry.path().join("chrome-linux64/chrome"));
        candidates.push(entry.path().join("chrome-linux/chrome"));
    }
    candidates.sort();
    candidates.into_iter().find(|path| is_executable_file(path))
}

fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

async fn start_browser_evidence_monitor(page: &Page) -> Result<BrowserEvidenceMonitor, String> {
    let request_events = page
        .event_listener::<EventRequestWillBeSent>()
        .await
        .map_err(|error| format!("CDP Network.requestWillBeSent listener: {error}"))?;
    let finished_events = page
        .event_listener::<EventLoadingFinished>()
        .await
        .map_err(|error| format!("CDP Network.loadingFinished listener: {error}"))?;
    let failed_events = page
        .event_listener::<EventLoadingFailed>()
        .await
        .map_err(|error| format!("CDP Network.loadingFailed listener: {error}"))?;
    let response_events = page
        .event_listener::<EventResponseReceived>()
        .await
        .map_err(|error| format!("CDP Network.responseReceived listener: {error}"))?;
    let exception_events = page
        .event_listener::<EventExceptionThrown>()
        .await
        .map_err(|error| format!("CDP Runtime.exceptionThrown listener: {error}"))?;

    page.execute(NetworkEnableParams::default())
        .await
        .map_err(|error| format!("CDP Network.enable: {error}"))?;
    page.execute(RuntimeEnableParams::default())
        .await
        .map_err(|error| format!("CDP Runtime.enable: {error}"))?;

    let state = Arc::new(Mutex::new(BrowserRuntimeEvidence::default()));
    let mut tasks = Vec::with_capacity(5);

    {
        let state = Arc::clone(&state);
        tasks.push(tokio::spawn(async move {
            let mut events = request_events;
            while let Some(event) = events.next().await {
                let Some(resource_type) = event.r#type.as_ref() else {
                    mark_monitor_failed(&state);
                    continue;
                };
                if is_relevant_resource_type(resource_type) {
                    let id = event.request_id.as_ref().to_owned();
                    let mut state = state
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    if !state.relevant_requests.contains_key(&id)
                        && state.relevant_requests.len() >= MAX_TRACKED_REQUESTS
                    {
                        state.monitor_failed = true;
                        continue;
                    }
                    state.relevant_requests.insert(
                        id.clone(),
                        NetworkRequestState {
                            resource_type: resource_type.clone(),
                            url: event.request.url.clone(),
                        },
                    );
                }
            }
            mark_monitor_failed(&state);
        }));
    }
    {
        let state = Arc::clone(&state);
        tasks.push(tokio::spawn(async move {
            let mut events = finished_events;
            while let Some(event) = events.next().await {
                let id = event.request_id.as_ref();
                let mut state = state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                state.relevant_requests.remove(id);
            }
            mark_monitor_failed(&state);
        }));
    }
    {
        let state = Arc::clone(&state);
        tasks.push(tokio::spawn(async move {
            let mut events = failed_events;
            while let Some(event) = events.next().await {
                let id = event.request_id.as_ref();
                let mut state = state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let tracked = state.relevant_requests.remove(id);
                if tracked.is_some() || is_relevant_resource_type(&event.r#type) {
                    if state.network_failures.len() >= MAX_NETWORK_OUTCOMES {
                        state.monitor_failed = true;
                    } else {
                        state.network_failures.push(NetworkOutcome {
                            resource_type: tracked
                                .as_ref()
                                .map(|request| request.resource_type.clone())
                                .unwrap_or_else(|| event.r#type.clone()),
                            url: tracked.map(|request| request.url),
                            failure_reason: Some(event.error_text.clone()),
                        });
                    }
                }
            }
            mark_monitor_failed(&state);
        }));
    }
    {
        let state = Arc::clone(&state);
        tasks.push(tokio::spawn(async move {
            let mut events = response_events;
            while let Some(event) = events.next().await {
                if is_relevant_resource_type(&event.r#type) && event.response.status >= 400 {
                    let mut state = state
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    if state.http_errors.len() >= MAX_NETWORK_OUTCOMES {
                        state.monitor_failed = true;
                    } else {
                        state.http_errors.push(NetworkOutcome {
                            resource_type: event.r#type.clone(),
                            url: Some(event.response.url.clone()),
                            failure_reason: None,
                        });
                    }
                }
            }
            mark_monitor_failed(&state);
        }));
    }
    {
        let state = Arc::clone(&state);
        tasks.push(tokio::spawn(async move {
            let mut events = exception_events;
            while events.next().await.is_some() {
                let mut state = state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                state.javascript_exceptions = state.javascript_exceptions.saturating_add(1);
            }
            mark_monitor_failed(&state);
        }));
    }

    Ok(BrowserEvidenceMonitor { state, tasks })
}

fn mark_monitor_failed(state: &Mutex<BrowserRuntimeEvidence>) {
    state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .monitor_failed = true;
}

fn is_relevant_resource_type(resource_type: &ResourceType) -> bool {
    matches!(
        resource_type,
        ResourceType::Document
            | ResourceType::Stylesheet
            | ResourceType::Image
            | ResourceType::Font
            | ResourceType::Script
            | ResourceType::Xhr
            | ResourceType::Fetch
    )
}

async fn apply_dark_theme(page: &Page) -> Result<(), String> {
    let style_id = serde_json::to_string(YARXI_DARK_THEME_STYLE_ID)
        .map_err(|error| format!("dark theme style id serialization: {error}"))?;
    let style_text = serde_json::to_string(YARXI_DARK_THEME_STYLE)
        .map_err(|error| format!("dark theme stylesheet serialization: {error}"))?;
    let script = format!(
        r#"() => {{
            const root = document.documentElement;
            if (!root) return {{ error: 'document root is missing' }};
            root.dataset.theme = 'dark';
            const styleId = {style_id};
            const styleText = {style_text};
            let style = document.getElementById(styleId);
            if (style && style.textContent !== styleText)
                return {{ error: 'dark theme stylesheet id is already in use' }};
            if (!style) {{
                style = document.createElement('style');
                style.id = styleId;
                style.textContent = styleText;
                (document.head || root).appendChild(style);
            }}
            const luminance = value => {{
                const components = (String(value).match(/[0-9]+(?:\.[0-9]+)?/g) || [])
                    .slice(0, 3).map(Number);
                if (components.length !== 3) return null;
                const linear = components.map(channel => {{
                    const normalized = channel / 255;
                    return normalized <= 0.04045 ? normalized / 12.92
                        : ((normalized + 0.055) / 1.055) ** 2.4;
                }});
                return 0.2126 * linear[0] + 0.7152 * linear[1] + 0.0722 * linear[2];
            }};
            const rootStyle = getComputedStyle(root);
            const app = document.querySelector('#app');
            const appStyle = app && getComputedStyle(app);
            return {{
                theme: root.dataset.theme || '',
                prefers_dark: matchMedia('(prefers-color-scheme: dark)').matches,
                color_scheme: rootStyle.colorScheme,
                page_background: rootStyle.backgroundColor,
                page_background_luminance: luminance(rootStyle.backgroundColor),
                app_foreground: appStyle?.color || '',
                app_foreground_luminance: appStyle ? luminance(appStyle.color) : null,
                style_id: style.id,
                style_text: style.textContent,
            }};
        }}"#
    );
    let state: Value = page
        .evaluate(script)
        .await
        .map_err(|error| format!("yarxi_dark_theme_setup: {error}"))?
        .into_value()
        .map_err(|error| format!("yarxi_dark_theme_setup_result: {error}"))?;
    let dark_state = state["theme"].as_str() == Some("dark")
        && state["prefers_dark"].as_bool() == Some(true)
        && state["color_scheme"]
            .as_str()
            .is_some_and(|value| value.contains("dark"))
        && state["page_background_luminance"]
            .as_f64()
            .is_some_and(|value| value < 0.20)
        && state["app_foreground_luminance"]
            .as_f64()
            .is_some_and(|value| value > 0.75)
        && state["style_id"].as_str() == Some(YARXI_DARK_THEME_STYLE_ID)
        && state["style_text"].as_str() == Some(YARXI_DARK_THEME_STYLE);
    if !dark_state {
        return Err(format!(
            "yarxi_dark_theme_unavailable: Yarxi base palette or page-level text inheritance did not enter the verified dark state: {state}"
        ));
    }
    Ok(())
}

async fn async_error_or_tls_interstitial(
    page: &Page,
    allow_insecure_tls: bool,
    original: impl std::fmt::Display,
) -> std::result::Result<&Page, String> {
    if !allow_insecure_tls {
        return Err(format!(
            "TLS-сертификат Yarxi отклонён браузером: {original}; повторите только после явного --allow-insecure-tls"
        ));
    }
    let interstitial: Value = page
        .evaluate(
            "() => ({ code: document.querySelector('#error-code')?.textContent?.trim(), proceed: Boolean(document.querySelector('#proceed-link')) })",
        )
        .await
        .map_err(|error| format!("не удалось проверить TLS interstitial: {error}"))?
        .into_value()
        .map_err(|error| format!("TLS interstitial вернул некорректный ответ: {error}"))?;
    let code = interstitial["code"].as_str().unwrap_or_default();
    if let Err(reason) = check_tls_exception(
        allow_insecure_tls,
        Url::parse(SITE_URL)
            .ok()
            .and_then(|url| url.host_str().map(str::to_owned))
            .as_deref()
            .unwrap_or_default(),
        code,
        &original.to_string(),
        interstitial["proceed"].as_bool().unwrap_or(false),
    ) {
        return Err(format!(
            "TLS exception отклонён: {reason}; original={original}; interstitial={interstitial}"
        ));
    }
    // Chrome's interstitial `proceed` is origin-scoped. No global
    // ignore-certificate-errors switch is enabled for the browser process.
    page.evaluate("() => document.querySelector('#proceed-link').click()")
        .await
        .map_err(|error| format!("не удалось принять исключение Yarxi: {error}"))?;
    wait_until(page, Duration::from_secs(20), || {
        "location.hostname === 'www.yarxi.su' && Boolean(document.querySelector('.kanji-search-form input[placeholder=\"Чтение\"]'))"
    })
    .await?;
    Ok(page)
}

fn check_tls_exception(
    explicitly_allowed: bool,
    interstitial_host: &str,
    interstitial_code: &str,
    original_error: &str,
    proceed_link_present: bool,
) -> Result<(), String> {
    if !explicitly_allowed {
        return Err("нужен явный --allow-insecure-tls".into());
    }
    if !interstitial_host.eq_ignore_ascii_case(SITE_HOST) {
        return Err(format!("неожиданный host {interstitial_host:?}"));
    }
    if !interstitial_code.eq_ignore_ascii_case("NET::ERR_CERT_AUTHORITY_INVALID")
        || !original_error
            .to_ascii_uppercase()
            .contains("ERR_CERT_AUTHORITY_INVALID")
    {
        return Err("разрешена только ERR_CERT_AUTHORITY_INVALID".into());
    }
    if !proceed_link_present {
        return Err("Chrome interstitial не предоставляет ссылку перехода".into());
    }
    Ok(())
}

async fn acquire_one(
    page: &Page,
    character: &str,
    target: AcquisitionTarget,
    evidence_monitor: &BrowserEvidenceMonitor,
) -> Result<AcquiredMedia, String> {
    if character.chars().count() != 1 {
        return Err("expected_one_kanji: ensure принимает один Unicode-символ".into());
    }
    let character_json = serde_json::to_string(character)
        .map_err(|error| format!("character serialization: {error}"))?;
    let search_script = format!(
        "() => {{ const visible = node => node.getClientRects().length > 0 && getComputedStyle(node).visibility !== 'hidden' && getComputedStyle(node).display !== 'none'; const container = [...document.querySelectorAll('.kanji-search-form')].find(visible); const input = container && [...container.querySelectorAll('input[placeholder=\\\"Чтение\\\"]')].find(visible); if (!input) return false; const set = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value').set; set.call(input, {character_json}); input.dispatchEvent(new Event('input', {{ bubbles: true }})); input.dispatchEvent(new Event('change', {{ bubbles: true }})); const button = [...container.querySelectorAll('button,[role=button],input[type=submit],input[type=button],a')].find(node => visible(node) && (node.innerText || node.value || node.getAttribute('aria-label') || '').trim() === 'Найти' && !node.disabled); if (button) {{ button.click(); return true; }} input.dispatchEvent(new KeyboardEvent('keydown', {{ key: 'Enter', code: 'Enter', keyCode: 13, which: 13, bubbles: true }})); return true; }}"
    );
    let submitted: bool = page
        .evaluate(search_script)
        .await
        .map_err(|error| format!("yarxi_search_ui: {error}"))?
        .into_value()
        .map_err(|error| format!("yarxi_search_ui_result: {error}"))?;
    if !submitted {
        return Err("yarxi_search_ui: поле «Чтение» или кнопка «Найти» отсутствует".into());
    }
    let expected_code = character
        .chars()
        .next()
        .map(|ch| format!("{:X}", u32::from(ch)))
        .expect("one character checked above");
    wait_until(page, Duration::from_secs(20), || {
        "Boolean([...document.querySelectorAll('button,[role=tab]')].find(node => node.getClientRects().length > 0 && getComputedStyle(node).visibility !== 'hidden' && node.innerText.trim() === 'Информация'))"
    })
    .await?;
    let info_tab: bool = page
        .evaluate(
            "() => { const tab = [...document.querySelectorAll('button,[role=tab]')].find(node => node.getClientRects().length > 0 && getComputedStyle(node).visibility !== 'hidden' && node.innerText.trim() === 'Информация'); if (!tab) return false; tab.click(); return true; }",
        )
        .await
        .map_err(|error| format!("yarxi_information_tab: {error}"))?
        .into_value()
        .map_err(|error| format!("yarxi_information_tab_result: {error}"))?;
    if !info_tab {
        return Err("yarxi_information_tab: вкладка «Информация» отсутствует".into());
    }
    let article = wait_for_article(page, &expected_code).await?;

    let mut snapshot =
        wait_for_media_snapshot(page, character, &expected_code, evidence_monitor, target).await?;
    let mut choice = choose_media_source_for_target(
        snapshot.primary.clone(),
        &snapshot.font_samples,
        character,
        target,
    )?;
    if target == AcquisitionTarget::PreferredSource
        && matches!(choice, MediaSourceChoice::RenderedFontSample { .. })
    {
        apply_dark_theme(page).await?;
        snapshot =
            wait_for_media_snapshot(page, character, &expected_code, evidence_monitor, target)
                .await?;
        choice = choose_media_source_for_target(
            snapshot.primary.clone(),
            &snapshot.font_samples,
            character,
            target,
        )?;
    }
    let (selection, source_url, absence_proof, rendered_font_sample, bytes) = match choice {
        MediaSourceChoice::Url {
            selection,
            url,
            absence_proof,
        } => {
            let bytes = resource_bytes(page, &url).await?;
            (selection, url, absence_proof, None, bytes)
        }
        MediaSourceChoice::RenderedFontSample {
            sample,
            absence_proof,
        } => {
            let (bytes, evidence) =
                render_font_sample_png(page, &sample, character, &expected_code).await?;
            (
                SelectionResult::RenderedFontSamplePng,
                SITE_URL.to_owned(),
                absence_proof,
                Some(evidence),
                bytes,
            )
        }
    };
    if bytes.is_empty() {
        return Err("media_empty: browser вернул пустые bytes".into());
    }
    let evidence = AcquisitionEvidence {
        provider: PROVIDER_ID.to_owned(),
        provider_version: PROVIDER_VERSION.to_owned(),
        article_unicode: article.unicode.clone(),
        article_number: article.article_number,
        frequency_index: article.frequency_index,
        selection,
        target,
        fallback_absence_proof: absence_proof,
        rendered_font_sample,
        source_url: source_url.clone(),
    };
    Ok(AcquiredMedia {
        character: character.to_owned(),
        source_url,
        article_unicode: article.unicode,
        selection,
        bytes,
        evidence,
    })
}

#[derive(Debug)]
struct MediaSnapshot {
    primary: PrimaryGifState,
    font_samples: Vec<FontSampleState>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct FontSampleState {
    index: usize,
    visible: bool,
    text: String,
    class_name: String,
    title: String,
    images: Vec<ImageElementState>,
    left: f64,
    top: f64,
    width: f64,
    height: f64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ArticleEvidence {
    unicode: String,
    article_number: Option<u32>,
    frequency_index: Option<u32>,
}

async fn wait_for_article(page: &Page, expected_code: &str) -> Result<ArticleEvidence, String> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let state: Value = page
            .evaluate(
                "() => { const text = document.body?.innerText || ''; return { text, unicodeCount: (text.match(/Unicode:/g) || []).length }; }",
            )
            .await
            .map_err(|error| format!("yarxi_article_read: {error}"))?
            .into_value()
            .map_err(|error| format!("yarxi_article_read_result: {error}"))?;
        let text = state["text"].as_str().unwrap_or_default();
        if let Some(found) = article_unicode(text)
            && found.eq_ignore_ascii_case(expected_code)
        {
            let count = state["unicodeCount"].as_u64().unwrap_or(0);
            if count > 1 {
                return Err(format!(
                    "ambiguous_article: найдено {count} результатов для U+{expected_code}"
                ));
            }
            return Ok(ArticleEvidence {
                unicode: found,
                article_number: article_number(text),
                frequency_index: frequency_index(text),
            });
        }
        if tokio::time::Instant::now() >= deadline {
            let body = text.chars().take(200).collect::<String>();
            return Err(format!(
                "article_identity_mismatch: нет Unicode U+{expected_code}; article_text={body}"
            ));
        }
        sleep(Duration::from_millis(200)).await;
    }
}

fn article_unicode(text: &str) -> Option<String> {
    let marker = text.find("Unicode:")?;
    text[marker + "Unicode:".len()..]
        .trim_start()
        .split(|ch: char| !ch.is_ascii_hexdigit())
        .next()
        .filter(|digits| !digits.is_empty())
        .map(str::to_owned)
}

fn article_number(text: &str) -> Option<u32> {
    let heading = text.lines().find(|line| line.contains('№'))?;
    let number = heading.split_once('№')?.1.trim_start();
    let digits: String = number.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

fn frequency_index(text: &str) -> Option<u32> {
    let marker = text.find("Частотность")?;
    let value = text[marker + "Частотность".len()..].trim_start();
    let digits: String = value.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

async fn wait_for_media_snapshot(
    page: &Page,
    expected_character: &str,
    expected_code: &str,
    evidence_monitor: &BrowserEvidenceMonitor,
    target: AcquisitionTarget,
) -> Result<MediaSnapshot, String> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    let mut previous_stable: Option<(String, std::time::Instant)> = None;
    loop {
        let state: Value = page
            .evaluate(
                r#"() => {
                    const text = document.body?.innerText || '';
                    const visible = node => node.getClientRects().length > 0
                        && getComputedStyle(node).visibility !== 'hidden'
                        && getComputedStyle(node).display !== 'none';
                    const infoTab = [...document.querySelectorAll('button,[role=tab]')]
                        .find(node => visible(node) && node.innerText.trim() === 'Информация');
                    const samples = [...document.querySelectorAll('.font-sample')].map((node,index) => {
                        const rect = node.getBoundingClientRect();
                        const images = [...node.querySelectorAll('img')].map(img => ({
                            url: img.currentSrc || img.src,
                            complete: img.complete,
                            width: img.naturalWidth,
                            height: img.naturalHeight,
                        }));
                        return {
                            index,
                            visible: visible(node),
                            text: (node.textContent || '').trim(),
                            class_name: typeof node.className === 'string' ? node.className : '',
                            title: node.getAttribute('title') || '',
                            images,
                            left: rect.left,
                            top: rect.top,
                            width: rect.width,
                            height: rect.height,
                        };
                    });
                    const row = [...document.querySelectorAll('.font-sample')].filter(visible);
                    const primary = document.querySelector('img.kakijun-gif');
                    const primaryUrl = primary ? (primary.currentSrc || primary.src) : '';
                    return {
                        text,
                        ready: document.readyState === 'complete' && document.fonts.status === 'loaded' && Boolean(infoTab) && Boolean(row.length),
                        samples,
                        primary: primary ? {
                            url: primaryUrl,
                            complete: primary.complete,
                            width: primary.naturalWidth,
                            height: primary.naturalHeight,
                        } : null,
                        pending: [...document.images].some(img => !img.complete),
                    };
                }"#,
            )
            .await
            .map_err(|error| format!("yarxi_media_dom: {error}"))?
            .into_value()
            .map_err(|error| format!("yarxi_media_dom_result: {error}"))?;
        let current_code = article_unicode(state["text"].as_str().unwrap_or_default());
        if current_code
            .as_deref()
            .is_none_or(|value| !value.eq_ignore_ascii_case(expected_code))
        {
            return Err(format!(
                "article_identity_changed: media относится не к U+{expected_code}"
            ));
        }
        let ready = state["ready"].as_bool().unwrap_or(false);
        let pending = state["pending"].as_bool().unwrap_or(true);
        let font_samples: Vec<FontSampleState> =
            serde_json::from_value(state["samples"].clone())
                .map_err(|error| format!("yarxi_font_samples_result: {error}"))?;
        let primary = state["primary"].as_object().map(|node| ImageElementState {
            url: node["url"].as_str().unwrap_or_default().to_owned(),
            complete: node["complete"].as_bool().unwrap_or(false),
            width: node["width"].as_u64().unwrap_or(0) as u32,
            height: node["height"].as_u64().unwrap_or(0) as u32,
        });
        let stable_key = serde_json::to_string(&(&font_samples, &current_code))
            .expect("serializing DOM snapshot cannot fail");
        let selected_font_sample = (target == AcquisitionTarget::RenderedFontSamplePng)
            .then(|| choose_font_sample(&font_samples, expected_character).ok())
            .flatten();
        let selected_sample_image_urls: HashSet<String> = selected_font_sample
            .as_ref()
            .into_iter()
            .flat_map(|sample| sample.images.iter().map(|image| image.url.clone()))
            .collect();
        let runtime = if target == AcquisitionTarget::RenderedFontSamplePng {
            evidence_monitor.font_sample_readiness(&selected_sample_image_urls)
        } else {
            evidence_monitor.readiness(None)
        };
        if runtime.monitor_failed || runtime.network_failures > 0 || runtime.http_errors > 0 {
            let network_details = if target == AcquisitionTarget::RenderedFontSamplePng {
                evidence_monitor.font_sample_network_failure_details(&selected_sample_image_urls)
            } else {
                evidence_monitor.network_failure_details(None)
            };
            return Err(format!(
                "browser_network_runtime_failure: {}; network_failures=[{network_details}]",
                runtime_failure_reason(runtime),
            ));
        }
        if runtime.javascript_exceptions > 0 {
            return Err(format!(
                "browser_javascript_failure: {}",
                runtime_failure_reason(runtime)
            ));
        }
        let relevant_images_complete = if target == AcquisitionTarget::RenderedFontSamplePng {
            selected_font_sample.as_ref().is_some_and(|sample| {
                sample.images.iter().all(|image| {
                    image.complete && image.width > 0 && image.height > 0 && !image.url.is_empty()
                })
            })
        } else {
            !pending
        };
        let dom_stable =
            if ready && relevant_images_complete && runtime.pending_relevant_requests == 0 {
                match &previous_stable {
                    Some((previous, observed_at)) if previous == &stable_key => {
                        observed_at.elapsed() >= Duration::from_secs(2)
                    }
                    _ => {
                        previous_stable = Some((stable_key, std::time::Instant::now()));
                        false
                    }
                }
            } else {
                previous_stable = None;
                false
            };
        let primary_state = classify_primary_state(
            primary.as_ref(),
            ready,
            dom_stable,
            !pending,
            evidence_monitor.readiness(None),
        );
        if target == AcquisitionTarget::RenderedFontSamplePng
            && ready
            && relevant_images_complete
            && runtime.pending_relevant_requests == 0
            && dom_stable
        {
            return Ok(MediaSnapshot {
                primary: primary_state,
                font_samples,
            });
        }
        match &primary_state {
            PrimaryGifState::PresentLoaded { .. }
                if ready && !pending && runtime.pending_relevant_requests == 0 =>
            {
                return Ok(MediaSnapshot {
                    primary: primary_state,
                    font_samples,
                });
            }
            PrimaryGifState::PresentLoaded { .. } => {}
            PrimaryGifState::AbsentConfirmed { .. } => {
                return Ok(MediaSnapshot {
                    primary: primary_state,
                    font_samples,
                });
            }
            PrimaryGifState::Unknown { reason } if reason.contains("zero natural dimensions") => {
                return Err(format!("gif_present_but_failed: {reason}"));
            }
            PrimaryGifState::Unknown { .. } => {}
        }
        if tokio::time::Instant::now() >= deadline {
            let reason = runtime_failure_reason(evidence_monitor.readiness(None));
            if primary.as_ref().is_some_and(|image| !image.complete) {
                return Err(format!(
                    "gif_pending: primary GIF did not finish loading; {reason}"
                ));
            }
            return Err(format!(
                "gif_unknown: media area did not reach a complete, stable state; {reason}"
            ));
        }
        sleep(Duration::from_millis(200)).await;
    }
}

async fn resource_bytes(page: &Page, resource_url: &str) -> Result<Vec<u8>, String> {
    if resource_url.starts_with("data:") || resource_url.starts_with("blob:") {
        let url_json = serde_json::to_string(resource_url)
            .map_err(|error| format!("resource URL serialization: {error}"))?;
        let script = format!(
            "async () => {{ const response = await fetch({url_json}); if (!response.ok) throw new Error('HTTP ' + response.status); const bytes = new Uint8Array(await response.arrayBuffer()); let binary = ''; for (let i = 0; i < bytes.length; i += 0x8000) binary += String.fromCharCode(...bytes.subarray(i, i + 0x8000)); return btoa(binary); }}"
        );
        let encoded: String = page
            .evaluate(script)
            .await
            .map_err(|error| format!("browser managed resource fetch failed: {error}"))?
            .into_value()
            .map_err(|error| format!("browser managed resource returned invalid bytes: {error}"))?;
        return base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(|error| format!("browser managed resource base64: {error}"));
    }
    let url = Url::parse(resource_url).map_err(|error| format!("invalid media URL: {error}"))?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || url.host_str().is_some_and(is_kakijun_host)
    {
        return Err("source_url_rejected: expected a non-kakijun HTTPS resource".into());
    }
    let tree = page
        .execute(GetResourceTreeParams::default())
        .await
        .map_err(|error| format!("CDP Page.getResourceTree: {error}"))?;
    let frame_id = tree.result.frame_tree.frame.id.clone();
    let content = page
        .execute(GetResourceContentParams::new(frame_id, resource_url))
        .await
        .map_err(|error| format!("CDP Page.getResourceContent: {error}"))?;
    if !content.result.base64_encoded {
        return Err("media_response_not_binary: browser did not preserve original bytes".into());
    }
    base64::engine::general_purpose::STANDARD
        .decode(content.result.content)
        .map_err(|error| format!("media base64 decode: {error}"))
}

async fn render_font_sample_png(
    page: &Page,
    sample: &FontSampleState,
    expected_character: &str,
    expected_code: &str,
) -> Result<(Vec<u8>, RenderedFontSampleEvidence), String> {
    let expected_json = serde_json::to_string(expected_character)
        .map_err(|error| format!("font sample character serialization: {error}"))?;
    let expected_code_json = serde_json::to_string(expected_code)
        .map_err(|error| format!("font sample Unicode serialization: {error}"))?;
    let expected_class_json = serde_json::to_string(&sample.class_name)
        .map_err(|error| format!("font sample class serialization: {error}"))?;
    let expected_title_json = serde_json::to_string(&sample.title)
        .map_err(|error| format!("font sample title serialization: {error}"))?;
    let expected_text_json = serde_json::to_string(&sample.text)
        .map_err(|error| format!("font sample text serialization: {error}"))?;
    let script = r#"() => {
        const expected = __EXPECTED__;
        const expectedCode = __EXPECTED_CODE__.toUpperCase();
        const expectedIndex = __INDEX__;
        const expectedClass = __CLASS__;
        const expectedTitle = __TITLE__;
        const expectedText = __TEXT__;
        const body = document.body?.innerText || '';
        const unicodeMatches = [...body.matchAll(/Unicode:\s*([0-9a-f]+)/ig)];
        if (unicodeMatches.length !== 1 || unicodeMatches[0][1].toUpperCase() !== expectedCode)
            return { error: 'article Unicode changed or is ambiguous' };
        const visible = node => node.getClientRects().length > 0
            && getComputedStyle(node).visibility !== 'hidden'
            && getComputedStyle(node).display !== 'none';
        const all = [...document.querySelectorAll('.font-sample')];
        const candidates = all.map((node, index) => {
            const rect = node.getBoundingClientRect();
            const className = typeof node.className === 'string' ? node.className : '';
            const text = (node.textContent || '').trim();
            const images = [...node.querySelectorAll('img')];
            const validClass = !className.split(/\s+/).some(token => token.startsWith('stroke-order'));
            const identity = text === expected || (images.length > 0 && text === '');
            return { node, index, rect, className, text, validClass,
                identity, visible: visible(node) };
        }).filter(item => item.validClass && item.identity && item.visible
            && item.rect.width > 0 && item.rect.height > 0)
          .sort((left, right) => left.rect.left - right.rect.left
            || left.rect.top - right.rect.top || left.index - right.index);
        const selected = candidates[0];
        if (!selected) return { error: 'no visible exact-character font sample' };
        if (selected.index !== expectedIndex || selected.className !== expectedClass
            || selected.text !== expectedText
            || (selected.node.getAttribute('title') || '') !== expectedTitle)
            return { error: 'leftmost font sample identity changed before capture' };
        const parseColor = value => {
            const parts = (value.match(/[0-9]+(?:\.[0-9]+)?/g) || []).slice(0, 4).map(Number);
            return parts.length >= 3 ? [parts[0], parts[1], parts[2], parts.length > 3 ? parts[3] : 1] : null;
        };
        const composite = node => {
            const chain = [];
            for (let current = node; current instanceof Element; current = current.parentElement)
                chain.unshift(current);
            let result = [255, 255, 255];
            for (const current of chain) {
                const color = parseColor(getComputedStyle(current).backgroundColor);
                if (!color) continue;
                const alpha = Math.max(0, Math.min(1, color[3]));
                result = [0, 1, 2].map(channel => color[channel] * alpha + result[channel] * (1 - alpha));
            }
            return `rgb(${result.map(Math.round).join(', ')})`;
        };
        const luminance = value => {
            const color = parseColor(value);
            if (!color) return null;
            const linear = color.slice(0, 3).map(channel => {
                const normalized = channel / 255;
                return normalized <= 0.04045 ? normalized / 12.92 : ((normalized + 0.055) / 1.055) ** 2.4;
            });
            return 0.2126 * linear[0] + 0.7152 * linear[1] + 0.0722 * linear[2];
        };
        const root = document.documentElement;
        const bodyNode = document.body || root;
        const style = getComputedStyle(selected.node);
        const pageBackground = composite(bodyNode);
        const tileBackground = composite(selected.node);
        const foreground = style.color;
        const borderWidth = style.borderTopWidth;
        const borderStyle = style.borderTopStyle;
        const borderColor = style.borderTopColor;
        const outlineWidth = style.outlineWidth;
        const outlineStyle = style.outlineStyle;
        const outlineColor = style.outlineColor;
        const hasPaint = value => (parseColor(value)?.[3] ?? 0) > 0;
        const borderVisible = (Number.parseFloat(borderWidth) > 0 && borderStyle !== 'none'
            && hasPaint(borderColor)) || (Number.parseFloat(outlineWidth) > 0
            && outlineStyle !== 'none' && hasPaint(outlineColor));
        const visibleBorderWidth = Number.parseFloat(borderWidth) > 0 ? borderWidth : outlineWidth;
        const visibleBorderStyle = borderStyle !== 'none' && Number.parseFloat(borderWidth) > 0
            ? borderStyle : outlineStyle;
        const visibleBorderColor = borderStyle !== 'none' && Number.parseFloat(borderWidth) > 0
            ? borderColor : outlineColor;
        const styleAttribute = selected.node.getAttribute('style');
        if (styleAttribute && styleAttribute.length > 4096)
            return { error: 'font sample inline style exceeds bounded evidence limit' };
        if (root.dataset.theme !== 'dark' || !matchMedia('(prefers-color-scheme: dark)').matches
            || luminance(pageBackground) === null || luminance(pageBackground) >= 0.20
            || luminance(tileBackground) === null || luminance(tileBackground) >= 0.20
            || luminance(foreground) === null || luminance(foreground) <= 0.75 || !borderVisible)
            return { error: `rendered tile setup failed (theme_class=${String(root.className).slice(0, 64)}; prefers_dark=${matchMedia('(prefers-color-scheme: dark)').matches}; page_bg=${String(pageBackground).slice(0, 64)}; tile_bg=${String(tileBackground).slice(0, 64)}; foreground=${String(foreground).slice(0, 64)}; border=${String(visibleBorderWidth).slice(0, 32)} ${String(visibleBorderStyle).slice(0, 32)} ${String(visibleBorderColor).slice(0, 64)}; border_visible=${borderVisible})` };
        const bounded = (value, limit) => String(value || '').slice(0, limit);
        const fingerprint = value => {
            const text = value === null ? '<null>' : value;
            let hash = 2166136261;
            for (let index = 0; index < text.length; index++) {
                hash ^= text.charCodeAt(index);
                hash = Math.imul(hash, 16777619);
            }
            return (hash >>> 0).toString(16).padStart(8, '0');
        };
        const rect = selected.rect;
        return {
            index: selected.index,
            selector: '.font-sample (leftmost visible exact-character sample)',
            class_name: bounded(selected.className, 200),
            title: bounded(selected.node.getAttribute('title') || '', 200),
            text: bounded(selected.text, 16),
            font_family: bounded(style.fontFamily, 256),
            font_size: bounded(style.fontSize, 32),
            css_rect: { x: rect.x, y: rect.y, width: rect.width, height: rect.height },
            viewport_width: window.innerWidth,
            viewport_height: window.innerHeight,
            device_scale_factor: window.devicePixelRatio,
            page_scale_factor: window.visualViewport?.scale || 1,
            prefers_color_scheme_dark: matchMedia('(prefers-color-scheme: dark)').matches,
            document_theme: bounded(root.dataset.theme || '', 32),
            foreground_color: bounded(foreground, 64),
            tile_background_color: bounded(style.backgroundColor, 64),
            page_background_color: bounded(pageBackground, 64),
            effective_background_color: bounded(tileBackground, 64),
            border_color: bounded(visibleBorderColor, 64),
            border_style: bounded(visibleBorderStyle, 32),
            border_width: bounded(visibleBorderWidth, 32),
            border_visible: borderVisible,
            box_shadow: bounded(style.boxShadow, 256),
            inline_style_json: styleAttribute,
        };
    }"#
        .replace("__EXPECTED__", &expected_json)
        .replace("__EXPECTED_CODE__", &expected_code_json)
        .replace("__INDEX__", &sample.index.to_string())
        .replace("__CLASS__", &expected_class_json)
        .replace("__TITLE__", &expected_title_json)
        .replace("__TEXT__", &expected_text_json);
    let elements = page
        .find_elements(".font-sample")
        .await
        .map_err(|error| format!("font_sample_query: {error}"))?;
    let element = elements
        .get(sample.index)
        .ok_or_else(|| "font_sample_changed: выбранный элемент больше не существует".to_owned())?;
    element
        .scroll_into_view()
        .await
        .map_err(|error| format!("font_sample_scroll: {error}"))?;
    let initial: Value = page
        .evaluate(script)
        .await
        .map_err(|error| format!("font_sample_evidence: {error}"))?
        .into_value()
        .map_err(|error| format!("font_sample_evidence_result: {error}"))?;
    if let Some(error) = initial["error"].as_str() {
        return Err(format!("font_sample_capture_contract: {error}"));
    }
    validate_capture_environment(
        initial["viewport_width"].as_u64().unwrap_or(0) as u32,
        initial["viewport_height"].as_u64().unwrap_or(0) as u32,
        initial["device_scale_factor"].as_f64().unwrap_or(0.0),
        initial["page_scale_factor"].as_f64().unwrap_or(0.0),
    )?;
    let bytes = element
        .screenshot(CaptureScreenshotFormat::Png)
        .await
        .map_err(|error| format!("font_sample_png_capture: {error}"))?;
    if !bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Err("font_sample_png_invalid: element capture did not return PNG bytes".into());
    }
    let decoded = image::load_from_memory_with_format(&bytes, image::ImageFormat::Png)
        .map_err(|error| format!("font_sample_png_decode: {error}"))?;
    let (pixel_width, pixel_height) = decoded.dimensions();
    validate_capture_dimensions(pixel_width, pixel_height)?;
    let pixel_evidence = analyze_capture_pixels(&decoded.to_rgba8())?;
    let style_json = serde_json::to_string(&initial["inline_style_json"].as_str())
        .map_err(|error| format!("font sample style serialization: {error}"))?;
    let document_theme_json = serde_json::to_string(&initial["document_theme"].as_str())
        .map_err(|error| format!("document theme serialization: {error}"))?;
    let style_id_json = serde_json::to_string(YARXI_DARK_THEME_STYLE_ID)
        .map_err(|error| format!("dark theme style id serialization: {error}"))?;
    let style_text_json = serde_json::to_string(YARXI_DARK_THEME_STYLE)
        .map_err(|error| format!("dark theme stylesheet serialization: {error}"))?;
    let post_capture_script = format!(
        "() => {{ const node=[...document.querySelectorAll('.font-sample')][{}]; const root=document.documentElement; const style=document.getElementById({style_id_json}); return Boolean(node && node.getAttribute('style') === {style_json} && root.dataset.theme === {document_theme_json} && root.dataset.theme === 'dark' && style && style.textContent === {style_text_json} && matchMedia('(prefers-color-scheme: dark)').matches); }}",
        sample.index
    );
    let inline_style_unchanged: bool = page
        .evaluate(post_capture_script)
        .await
        .map_err(|error| format!("font_sample_post_capture_check: {error}"))?
        .into_value()
        .map_err(|error| format!("font_sample_post_capture_check_result: {error}"))?;
    if !inline_style_unchanged {
        return Err(
            "font_sample_style_changed: selected tile style or dark theme changed during capture"
                .into(),
        );
    }
    let evidence = RenderedFontSampleEvidence {
        index: sample.index,
        selector: initial["selector"].as_str().unwrap_or_default().to_owned(),
        class_name: initial["class_name"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
        title: initial["title"].as_str().unwrap_or_default().to_owned(),
        text: initial["text"].as_str().unwrap_or_default().to_owned(),
        font_family: initial["font_family"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
        font_size: initial["font_size"].as_str().unwrap_or_default().to_owned(),
        capture: "Chromium element screenshot of the unmodified rendered .font-sample tile under Yarxi data-theme=dark".into(),
        css_rect: serde_json::from_value(initial["css_rect"].clone())
            .map_err(|error| format!("font_sample_css_rect: {error}"))?,
        pixel_width,
        pixel_height,
        viewport_width: initial["viewport_width"].as_u64().unwrap_or(0) as u32,
        viewport_height: initial["viewport_height"].as_u64().unwrap_or(0) as u32,
        device_scale_factor: initial["device_scale_factor"].as_f64().unwrap_or(0.0),
        page_scale_factor: initial["page_scale_factor"].as_f64().unwrap_or(0.0),
        prefers_color_scheme_dark: initial["prefers_color_scheme_dark"]
            .as_bool()
            .unwrap_or(false),
        dark_environment: "Yarxi data-theme=dark; color-scheme=dark; page-level #app color inherits --w-base-color-rgb".into(),
        document_theme: initial["document_theme"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
        foreground_color: initial["foreground_color"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
        tile_background_color: initial["tile_background_color"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
        page_background_color: initial["page_background_color"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
        effective_background_color: initial["effective_background_color"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
        border_color: initial["border_color"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
        border_style: initial["border_style"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
        border_width: initial["border_width"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
        border_visible: initial["border_visible"].as_bool().unwrap_or(false),
        box_shadow: initial["box_shadow"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
        inline_style_unchanged,
        dark_background_pixels: pixel_evidence.dark_background_pixels,
        light_glyph_pixels: pixel_evidence.light_glyph_pixels,
        frame_pixels: pixel_evidence.frame_pixels,
        measured_dark_background_rgb: pixel_evidence.dark_background_rgb,
        measured_light_glyph_rgb: pixel_evidence.light_glyph_rgb,
        measured_frame_rgb: pixel_evidence.frame_rgb,
    };
    Ok((bytes, evidence))
}

fn validate_capture_dimensions(width: u32, height: u32) -> Result<(), String> {
    if (CAPTURE_MIN_EDGE_PX..=CAPTURE_MAX_EDGE_PX).contains(&width)
        && (CAPTURE_MIN_EDGE_PX..=CAPTURE_MAX_EDGE_PX).contains(&height)
    {
        Ok(())
    } else {
        Err(format!(
            "font_sample_scale_out_of_bounds: capture is {width}x{height}px; expected {CAPTURE_MIN_EDGE_PX}–{CAPTURE_MAX_EDGE_PX}px per edge"
        ))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TilePixelEvidence {
    dark_background_pixels: u32,
    light_glyph_pixels: u32,
    frame_pixels: u32,
    dark_background_rgb: String,
    light_glyph_rgb: String,
    frame_rgb: String,
}

#[derive(Debug, Default)]
struct PixelColorSummary {
    count: u32,
    red: u64,
    green: u64,
    blue: u64,
}

impl PixelColorSummary {
    fn add(&mut self, pixel: image::Rgba<u8>) {
        self.count = self.count.saturating_add(1);
        self.red += u64::from(pixel[0]);
        self.green += u64::from(pixel[1]);
        self.blue += u64::from(pixel[2]);
    }

    fn average_rgb(&self) -> String {
        if self.count == 0 {
            return "unavailable".into();
        }
        format!(
            "rgb({}, {}, {})",
            self.red / u64::from(self.count),
            self.green / u64::from(self.count),
            self.blue / u64::from(self.count)
        )
    }
}

fn analyze_capture_pixels(image: &RgbaImage) -> Result<TilePixelEvidence, String> {
    let (width, height) = image.dimensions();
    validate_capture_dimensions(width, height)?;
    let min_edge = width.min(height);
    let inset = (min_edge / 12).max(1);
    let frame_band = (min_edge / 20).max(1);
    let mut dark = PixelColorSummary::default();
    let mut glyph = PixelColorSummary::default();
    let mut frame = PixelColorSummary::default();

    for (x, y, pixel) in image.enumerate_pixels() {
        if pixel[3] < 240 {
            continue;
        }
        let channels = [pixel[0], pixel[1], pixel[2]].map(|channel| {
            let normalized = f64::from(channel) / 255.0;
            if normalized <= 0.04045 {
                normalized / 12.92
            } else {
                ((normalized + 0.055) / 1.055).powf(2.4)
            }
        });
        let luminance = 0.2126 * channels[0] + 0.7152 * channels[1] + 0.0722 * channels[2];
        let inside = x >= inset && x < width - inset && y >= inset && y < height - inset;
        let at_frame =
            x < frame_band || x >= width - frame_band || y < frame_band || y >= height - frame_band;
        if luminance <= 0.10 {
            dark.add(*pixel);
        }
        if inside && luminance >= CAPTURE_LIGHT_GLYPH_MIN_LUMINANCE {
            glyph.add(*pixel);
        }
        if at_frame && (0.10..0.75).contains(&luminance) {
            frame.add(*pixel);
        }
    }

    let total_pixels = width.saturating_mul(height);
    if dark.count < total_pixels / 4 {
        return Err(format!(
            "font_sample_pixel_contract: too few dark tile pixels ({}/{total_pixels})",
            dark.count
        ));
    }
    if glyph.count < 16 {
        return Err(format!(
            "font_sample_pixel_contract: no sufficient light glyph pixels ({})",
            glyph.count
        ));
    }
    if frame.count < 8 {
        return Err(format!(
            "font_sample_pixel_contract: no visible mid-tone frame pixels ({})",
            frame.count
        ));
    }
    Ok(TilePixelEvidence {
        dark_background_pixels: dark.count,
        light_glyph_pixels: glyph.count,
        frame_pixels: frame.count,
        dark_background_rgb: dark.average_rgb(),
        light_glyph_rgb: glyph.average_rgb(),
        frame_rgb: frame.average_rgb(),
    })
}

fn validate_capture_environment(
    viewport_width: u32,
    viewport_height: u32,
    device_scale_factor: f64,
    page_scale_factor: f64,
) -> Result<(), String> {
    if viewport_width == CAPTURE_VIEWPORT_WIDTH as u32
        && viewport_height == CAPTURE_VIEWPORT_HEIGHT as u32
        && (device_scale_factor - CAPTURE_DEVICE_SCALE_FACTOR).abs() < 0.01
        && (page_scale_factor - 1.0).abs() < 0.01
    {
        Ok(())
    } else {
        Err(format!(
            "font_sample_capture_environment_mismatch: viewport={viewport_width}x{viewport_height}, device_scale_factor={device_scale_factor}, page_scale_factor={page_scale_factor}"
        ))
    }
}

fn is_kakijun_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("kakijun.jp") || host.to_ascii_lowercase().ends_with(".kakijun.jp")
}

async fn wait_until(
    page: &Page,
    timeout_after: Duration,
    expression: impl Fn() -> &'static str,
) -> Result<(), String> {
    let deadline = tokio::time::Instant::now() + timeout_after;
    loop {
        let ready: bool = page
            .evaluate(expression())
            .await
            .map_err(|error| format!("browser readiness check: {error}"))?
            .into_value()
            .map_err(|error| format!("browser readiness response: {error}"))?;
        if ready {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err("browser readiness timeout".into());
        }
        sleep(Duration::from_millis(200)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dark_theme_setup_changes_page_environment_without_restyling_font_sample() {
        assert!(YARXI_DARK_THEME_STYLE.contains(":root { color-scheme: dark"));
        assert!(YARXI_DARK_THEME_STYLE.contains("#app { color: rgb(var(--w-base-color-rgb))"));
        assert!(!YARXI_DARK_THEME_STYLE.contains(".font-sample"));
        assert!(!YARXI_DARK_THEME_STYLE.contains("background"));
    }

    #[test]
    fn present_primary_wins_and_unknown_never_falls_back() {
        assert_eq!(
            choose_media_source(
                PrimaryGifState::PresentLoaded {
                    url: "https://yosida.com/left.gif".into(),
                },
                &[],
                "柘"
            )
            .unwrap(),
            MediaSourceChoice::Url {
                selection: SelectionResult::PrimaryGif,
                url: "https://yosida.com/left.gif".into(),
                absence_proof: None,
            }
        );
        assert!(
            choose_media_source(
                PrimaryGifState::Unknown {
                    reason: "request pending".into(),
                },
                &[font_sample(0, 0.0, "柘", "font-sample serif", false)],
                "柘"
            )
            .unwrap_err()
            .contains("gif_unknown")
        );
        assert_eq!(
            choose_media_source(
                PrimaryGifState::PresentLoaded {
                    url: "https://yosida.com/left.gif".into(),
                },
                &[],
                "柘"
            )
            .unwrap(),
            MediaSourceChoice::Url {
                selection: SelectionResult::PrimaryGif,
                url: "https://yosida.com/left.gif".into(),
                absence_proof: None,
            },
            "unrelated right-side Kakijun link never replaces the left GIF"
        );
    }

    fn font_sample(
        index: usize,
        left: f64,
        text: &str,
        class_name: &str,
        stroke_order: bool,
    ) -> FontSampleState {
        FontSampleState {
            index,
            visible: true,
            text: text.into(),
            class_name: if stroke_order {
                format!("{class_name} stroke-orders")
            } else {
                class_name.into()
            },
            title: "Шрифт с засечками".into(),
            images: Vec::new(),
            left,
            top: 0.0,
            width: 80.0,
            height: 100.0,
        }
    }

    fn clean_runtime() -> RuntimeReadiness {
        RuntimeReadiness {
            pending_relevant_requests: 0,
            network_failures: 0,
            http_errors: 0,
            javascript_exceptions: 0,
            monitor_failed: false,
        }
    }

    #[test]
    fn delayed_primary_and_unready_media_area_are_unknown() {
        let delayed = ImageElementState {
            url: "https://yosida.com/images/kanji/5143.gif".into(),
            complete: false,
            width: 0,
            height: 0,
        };
        let state = classify_primary_state(Some(&delayed), true, true, false, clean_runtime());
        assert!(matches!(state, PrimaryGifState::Unknown { .. }));
        assert!(choose_media_source(state, &[], "柘").is_err());

        let unready_absence = classify_primary_state(None, false, false, false, clean_runtime());
        assert!(matches!(unready_absence, PrimaryGifState::Unknown { .. }));
        assert!(choose_media_source(unready_absence, &[], "柘").is_err());
    }

    #[test]
    fn failed_present_gif_is_not_absence_and_kakijun_is_never_an_admissible_source() {
        let failed = ImageElementState {
            url: "https://yosida.com/images/kanji/5143.gif".into(),
            complete: true,
            width: 0,
            height: 0,
        };
        let state = classify_primary_state(Some(&failed), true, true, true, clean_runtime());
        assert!(matches!(state, PrimaryGifState::Unknown { .. }));
        assert!(choose_media_source(state, &[], "柘").is_err());
        assert!(is_kakijun_host("kakijun.jp"));
        assert!(is_kakijun_host("www.kakijun.jp"));
        assert!(!is_kakijun_host("yosida.com"));
    }

    #[test]
    fn fallback_uses_the_leftmost_visual_sample_and_never_kakijun() {
        let later_image = FontSampleState {
            images: vec![ImageElementState {
                url: "https://example.test/second.png".into(),
                complete: true,
                width: 80,
                height: 100,
            }],
            ..font_sample(1, 20.0, "", "font-sample sans", false)
        };
        let left_text = font_sample(0, 10.0, "柘", "font-sample serif", false);
        assert_eq!(
            choose_media_source(
                PrimaryGifState::AbsentConfirmed {
                    proof: "stable complete DOM".into(),
                },
                &[left_text.clone(), later_image],
                "柘",
            )
            .unwrap(),
            MediaSourceChoice::RenderedFontSample {
                sample: left_text,
                absence_proof: Some("stable complete DOM".into()),
            },
            "a PNG inside a tile to the right must not replace the leftmost font sample"
        );

        let left_image = FontSampleState {
            images: vec![ImageElementState {
                url: "https://example.test/first.png".into(),
                complete: true,
                width: 80,
                height: 100,
            }],
            ..font_sample(0, 10.0, "柘", "font-sample serif", false)
        };
        let later_text = font_sample(1, 20.0, "柘", "font-sample sans", false);
        assert_eq!(
            choose_media_source(
                PrimaryGifState::AbsentConfirmed {
                    proof: "stable complete DOM".into(),
                },
                &[left_image, later_text],
                "柘",
            )
            .unwrap(),
            MediaSourceChoice::Url {
                selection: SelectionResult::LeftmostPngFallback,
                url: "https://example.test/first.png".into(),
                absence_proof: Some("stable complete DOM".into()),
            }
        );

        let kakijun_sample = FontSampleState {
            images: vec![ImageElementState {
                url: "https://kakijun.jp/gif/right.png".into(),
                complete: true,
                width: 80,
                height: 100,
            }],
            ..font_sample(0, 10.0, "柘", "font-sample serif", false)
        };
        assert!(
            choose_media_source(
                PrimaryGifState::AbsentConfirmed {
                    proof: "stable complete DOM".into(),
                },
                &[kakijun_sample],
                "柘",
            )
            .unwrap_err()
            .contains("kakijun_source_forbidden")
        );
    }

    #[test]
    fn absent_primary_renders_leftmost_exact_font_sample_when_no_png_file_exists() {
        let samples = [
            font_sample(0, 30.0, "柘", "font-sample sans", false),
            font_sample(1, 10.0, "柘", "font-sample serif", false),
            font_sample(2, 0.0, "柘", "font-sample", true),
            font_sample(3, 5.0, "栃", "font-sample serif", false),
        ];
        assert_eq!(
            choose_media_source(
                PrimaryGifState::AbsentConfirmed {
                    proof: "stable complete DOM".into(),
                },
                &samples,
                "柘",
            )
            .unwrap(),
            MediaSourceChoice::RenderedFontSample {
                sample: samples[1].clone(),
                absence_proof: Some("stable complete DOM".into()),
            }
        );
        assert!(
            choose_media_source(
                PrimaryGifState::AbsentConfirmed {
                    proof: "stable complete DOM".into(),
                },
                &[],
                "柘",
            )
            .unwrap_err()
            .contains("fallback_font_sample_missing")
        );
    }

    #[test]
    fn explicit_png_target_uses_the_leftmost_tile_even_when_primary_gif_is_loaded() {
        let samples = [
            font_sample(0, 24.0, "柘", "font-sample sans", false),
            font_sample(1, 10.0, "柘", "font-sample serif", false),
            font_sample(2, 0.0, "柘", "font-sample stroke-orders", false),
        ];
        let primary = PrimaryGifState::PresentLoaded {
            url: "https://example.test/primary.gif".into(),
        };
        assert!(matches!(
            choose_media_source(primary.clone(), &samples, "柘").unwrap(),
            MediaSourceChoice::Url {
                selection: SelectionResult::PrimaryGif,
                ..
            }
        ));
        assert_eq!(
            choose_media_source_for_target(
                primary,
                &samples,
                "柘",
                AcquisitionTarget::RenderedFontSamplePng,
            )
            .unwrap(),
            MediaSourceChoice::RenderedFontSample {
                sample: samples[1].clone(),
                absence_proof: None,
            }
        );
    }

    #[test]
    fn explicit_png_target_can_ignore_primary_gif_only_by_exact_image_url() {
        let mut browser = BrowserRuntimeEvidence::default();
        browser.relevant_requests.insert(
            "gif".into(),
            NetworkRequestState {
                resource_type: ResourceType::Image,
                url: "https://example.test/primary.gif".into(),
            },
        );
        browser.relevant_requests.insert(
            "script".into(),
            NetworkRequestState {
                resource_type: ResourceType::Script,
                url: "https://example.test/article.js".into(),
            },
        );
        browser.network_failures.push(NetworkOutcome {
            resource_type: ResourceType::Image,
            url: Some("https://example.test/primary.gif".into()),
            failure_reason: Some("net::ERR_FAILED".into()),
        });
        browser.http_errors.push(NetworkOutcome {
            resource_type: ResourceType::Image,
            url: Some("https://example.test/primary.gif".into()),
            failure_reason: None,
        });
        assert_eq!(browser.readiness(None).pending_relevant_requests, 2);
        let explicit = browser.readiness(Some("https://example.test/primary.gif"));
        assert_eq!(explicit.pending_relevant_requests, 1);
        assert_eq!(explicit.network_failures, 0);
        assert_eq!(explicit.http_errors, 0);
        assert_eq!(
            browser
                .readiness(Some("https://example.test/other.gif"))
                .network_failures,
            1
        );
    }

    #[test]
    fn rendered_sample_readiness_tracks_its_image_but_ignores_sibling_images() {
        let selected_url = "https://example.test/sample.png";
        let sibling_url = "https://yosida.example/stroke-order.gif";
        let selected_images = HashSet::from([selected_url.to_owned()]);
        let browser = BrowserRuntimeEvidence {
            relevant_requests: HashMap::from([
                (
                    "selected-image".into(),
                    NetworkRequestState {
                        resource_type: ResourceType::Image,
                        url: selected_url.into(),
                    },
                ),
                (
                    "sibling-image".into(),
                    NetworkRequestState {
                        resource_type: ResourceType::Image,
                        url: sibling_url.into(),
                    },
                ),
                (
                    "script".into(),
                    NetworkRequestState {
                        resource_type: ResourceType::Script,
                        url: "https://example.test/article.js".into(),
                    },
                ),
            ]),
            network_failures: vec![
                NetworkOutcome {
                    resource_type: ResourceType::Image,
                    url: Some(sibling_url.into()),
                    failure_reason: Some("net::ERR_ABORTED".into()),
                },
                NetworkOutcome {
                    resource_type: ResourceType::Image,
                    url: Some(selected_url.into()),
                    failure_reason: Some("net::ERR_FAILED".into()),
                },
                NetworkOutcome {
                    resource_type: ResourceType::Script,
                    url: Some("https://example.test/article.js".into()),
                    failure_reason: Some("net::ERR_FAILED".into()),
                },
            ],
            ..BrowserRuntimeEvidence::default()
        };

        let readiness = browser.font_sample_readiness(&selected_images);
        assert_eq!(readiness.pending_relevant_requests, 2);
        assert_eq!(readiness.network_failures, 2);
        assert_eq!(readiness.http_errors, 0);
    }

    #[test]
    fn tls_exception_clears_only_the_approved_exact_document_failure() {
        let browser = BrowserRuntimeEvidence {
            network_failures: vec![
                NetworkOutcome {
                    resource_type: ResourceType::Document,
                    url: Some(SITE_URL.into()),
                    failure_reason: Some("net::ERR_CERT_AUTHORITY_INVALID".into()),
                },
                NetworkOutcome {
                    resource_type: ResourceType::Document,
                    url: Some("https://attacker.example/".into()),
                    failure_reason: Some("net::ERR_CERT_AUTHORITY_INVALID".into()),
                },
                NetworkOutcome {
                    resource_type: ResourceType::Document,
                    url: Some(SITE_URL.into()),
                    failure_reason: Some("net::ERR_CERT_DATE_INVALID".into()),
                },
            ],
            ..BrowserRuntimeEvidence::default()
        };
        let monitor = BrowserEvidenceMonitor {
            state: Arc::new(Mutex::new(browser)),
            tasks: Vec::new(),
        };
        monitor.clear_explicitly_approved_tls_interstitial_failure();
        let state = monitor
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(state.network_failures.len(), 2);
        assert!(
            state
                .network_failures
                .iter()
                .any(|failure| { failure.url.as_deref() == Some("https://attacker.example/") })
        );
        assert!(state.network_failures.iter().any(|failure| {
            failure.failure_reason.as_deref() == Some("net::ERR_CERT_DATE_INVALID")
        }));
    }

    #[test]
    fn network_failure_diagnostics_strip_query_fragment_and_credentials() {
        let browser = BrowserRuntimeEvidence {
            network_failures: vec![
                NetworkOutcome {
                    resource_type: ResourceType::Script,
                    url: Some(
                        "https://user:password@example.test/assets/app.js?token=secret#fragment"
                            .into(),
                    ),
                    failure_reason: Some("net::ERR_CONNECTION_RESET".into()),
                },
                NetworkOutcome {
                    resource_type: ResourceType::Image,
                    url: Some("https://example.test/primary.gif?cache=1".into()),
                    failure_reason: Some("net::ERR_FAILED".into()),
                },
            ],
            ..BrowserRuntimeEvidence::default()
        };
        let monitor = BrowserEvidenceMonitor {
            state: Arc::new(Mutex::new(browser)),
            tasks: Vec::new(),
        };
        let details = monitor.network_failure_details(None);
        assert!(
            details.contains("Script https://example.test/assets/app.js net::ERR_CONNECTION_RESET")
        );
        assert!(!details.contains("user"));
        assert!(!details.contains("password"));
        assert!(!details.contains("token"));
        assert!(!details.contains("secret"));
        assert!(!details.contains("fragment"));

        let exact_primary =
            monitor.network_failure_details(Some("https://example.test/primary.gif?cache=1"));
        assert!(!exact_primary.contains("primary.gif"));
        assert!(exact_primary.contains("ERR_CONNECTION_RESET"));
        let other_primary =
            monitor.network_failure_details(Some("https://example.test/primary.gif?cache=other"));
        assert!(other_primary.contains("primary.gif"));
    }

    #[test]
    fn network_failure_diagnostics_limit_entries_and_redact_unrecognized_reason_text() {
        let failures = (0..5)
            .map(|index| NetworkOutcome {
                resource_type: ResourceType::Fetch,
                url: Some(format!("https://example.test/api/{index}?secret=hidden")),
                failure_reason: Some(if index == 0 {
                    "connection details include a secret".into()
                } else {
                    "net::ERR_FAILED".into()
                }),
            })
            .collect();
        let monitor = BrowserEvidenceMonitor {
            state: Arc::new(Mutex::new(BrowserRuntimeEvidence {
                network_failures: failures,
                ..BrowserRuntimeEvidence::default()
            })),
            tasks: Vec::new(),
        };
        let details = monitor.network_failure_details(None);
        assert_eq!(
            details.matches("Fetch ").count(),
            MAX_NETWORK_DIAGNOSTIC_ITEMS
        );
        assert!(details.contains("+2 more network failures"));
        assert!(details.contains("unclassified network error"));
        assert!(!details.contains("secret"));
        assert!(!details.contains("hidden"));
    }

    #[test]
    fn pending_network_js_or_monitor_failure_never_confirms_gif_absence() {
        let dirty_states = [
            RuntimeReadiness {
                pending_relevant_requests: 1,
                ..clean_runtime()
            },
            RuntimeReadiness {
                network_failures: 1,
                ..clean_runtime()
            },
            RuntimeReadiness {
                http_errors: 1,
                ..clean_runtime()
            },
            RuntimeReadiness {
                javascript_exceptions: 1,
                ..clean_runtime()
            },
            RuntimeReadiness {
                monitor_failed: true,
                ..clean_runtime()
            },
        ];
        for runtime in dirty_states {
            let state = classify_primary_state(None, true, true, true, runtime);
            assert!(matches!(state, PrimaryGifState::Unknown { .. }));
            assert!(choose_media_source(state, &[], "柘").is_err());
        }
        assert!(matches!(
            classify_primary_state(None, true, true, true, clean_runtime()),
            PrimaryGifState::AbsentConfirmed { .. }
        ));
    }

    #[test]
    fn capture_environment_and_dimensions_stay_near_reference_scale() {
        assert!(validate_capture_environment(1280, 900, 2.175, 1.0).is_ok());
        assert!(validate_capture_environment(1280, 900, 2.2, 1.0).is_err());
        assert!(validate_capture_environment(1280, 900, 1.0, 1.0).is_err());
        assert!(validate_capture_environment(1024, 768, 2.0, 1.0).is_err());
        assert!(validate_capture_dimensions(174, 174).is_ok());
        assert!(validate_capture_dimensions(140, 220).is_ok());
        assert!(validate_capture_dimensions(139, 175).is_err());
        assert!(validate_capture_dimensions(173, 500).is_err());
    }

    #[test]
    fn decoded_png_evidence_requires_dark_background_light_glyph_and_frame_pixels() {
        let mut tile = RgbaImage::from_pixel(160, 160, image::Rgba([36, 39, 41, 255]));
        for y in 0..160 {
            for x in 0..160 {
                if !(3..157).contains(&x) || !(3..157).contains(&y) {
                    tile.put_pixel(x, y, image::Rgba([126, 137, 141, 255]));
                }
            }
        }
        for coordinate in 38..122 {
            tile.put_pixel(80, coordinate, image::Rgba([245, 245, 245, 255]));
            tile.put_pixel(coordinate, 80, image::Rgba([245, 245, 245, 255]));
        }
        let evidence = analyze_capture_pixels(&tile).unwrap();
        assert!(evidence.dark_background_pixels > 10_000);
        assert!(evidence.light_glyph_pixels > 100);
        assert!(evidence.frame_pixels > 100);
        assert_eq!(evidence.dark_background_rgb, "rgb(36, 39, 41)");
        assert_eq!(evidence.frame_rgb, "rgb(126, 137, 141)");

        let mut chromium_dark_tile =
            RgbaImage::from_pixel(160, 160, image::Rgba([36, 39, 41, 255]));
        for y in 0..160 {
            for x in 0..160 {
                if !(3..157).contains(&x) || !(3..157).contains(&y) {
                    chromium_dark_tile.put_pixel(x, y, image::Rgba([126, 137, 141, 255]));
                }
            }
        }
        for coordinate in 38..122 {
            chromium_dark_tile.put_pixel(80, coordinate, image::Rgba([195, 214, 236, 255]));
            chromium_dark_tile.put_pixel(coordinate, 80, image::Rgba([195, 214, 236, 255]));
        }
        let chromium_dark_evidence = analyze_capture_pixels(&chromium_dark_tile).unwrap();
        assert!(chromium_dark_evidence.light_glyph_pixels > 100);
        assert_eq!(chromium_dark_evidence.light_glyph_rgb, "rgb(195, 214, 236)");

        let blank_dark = RgbaImage::from_pixel(160, 160, image::Rgba([36, 39, 41, 255]));
        assert!(
            analyze_capture_pixels(&blank_dark)
                .unwrap_err()
                .contains("light glyph")
        );
        let white = RgbaImage::from_pixel(160, 160, image::Rgba([255, 255, 255, 255]));
        assert!(
            analyze_capture_pixels(&white)
                .unwrap_err()
                .contains("dark tile pixels")
        );
    }

    #[test]
    fn article_unicode_parser_does_not_accept_a_different_article() {
        let text = "Статья №773\nUnicode: 5143\nЧастотность\n192";
        assert_eq!(article_unicode(text), Some("5143".into()));
        assert_ne!(article_unicode(text).as_deref(), Some("672A"));
        assert_eq!(article_unicode("no article metadata"), None);
        assert_eq!(article_number(text), Some(773));
        assert_eq!(frequency_index(text), Some(192));
        assert_eq!(article_number("№773\nUnicode: 5143"), Some(773));
        assert_eq!(frequency_index("Частотность\n192 / JLPT 2"), Some(192));
        assert_eq!(frequency_index("Частотность\nJLPT 2"), None);
    }

    #[test]
    fn tls_exception_requires_explicit_flag_exact_host_and_exact_error() {
        let expected_error = "net::ERR_CERT_AUTHORITY_INVALID";
        assert!(
            check_tls_exception(
                true,
                "www.yarxi.su",
                "NET::ERR_CERT_AUTHORITY_INVALID",
                expected_error,
                true,
            )
            .is_ok()
        );
        assert!(
            check_tls_exception(
                false,
                "www.yarxi.su",
                "NET::ERR_CERT_AUTHORITY_INVALID",
                expected_error,
                true,
            )
            .is_err()
        );
        assert!(
            check_tls_exception(
                true,
                "attacker.example",
                "NET::ERR_CERT_AUTHORITY_INVALID",
                expected_error,
                true,
            )
            .is_err()
        );
        assert!(
            check_tls_exception(
                true,
                "www.yarxi.su",
                "NET::ERR_CERT_DATE_INVALID",
                "net::ERR_CERT_DATE_INVALID",
                true,
            )
            .is_err()
        );
        assert!(
            check_tls_exception(
                true,
                "www.yarxi.su",
                "NET::ERR_CERT_AUTHORITY_INVALID",
                expected_error,
                false,
            )
            .is_err()
        );
    }
}
