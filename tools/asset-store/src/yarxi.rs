//! Провайдер браузера для динамических статей Yarxi и изображений слева от них.

use std::collections::HashSet;
use std::future::Future;
use std::time::Duration;

#[cfg(test)]
use crate::browser_runtime::TrackedRequest;
use crate::browser_runtime::{
    self, BrowserRuntimeConfig, BrowserSession, CdpRuntimeMonitor, DeviceMetrics, NetworkOutcome,
    RetryTrigger, RuntimeSnapshot,
};
pub use crate::browser_runtime::{BrowserExecutableSource, BrowserRuntimeProvenance};
use base64::Engine as _;
use chromiumoxide::{
    Page,
    cdp::browser_protocol::network::ResourceType,
    cdp::browser_protocol::page::{
        CaptureScreenshotFormat, GetResourceContentParams, GetResourceTreeParams,
    },
};
use image::{GenericImageView, RgbaImage};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::time::{Instant, sleep, timeout_at};
use url::Url;

const PROVIDER_ID: &str = "yarxi-suu-browser";
const PROVIDER_VERSION: &str = "7";
const SITE_URL: &str = "https://www.yarxi.su/";
const SITE_HOST: &str = "www.yarxi.su";
const OPERATION_TIMEOUT: Duration = Duration::from_secs(20 * 60);
const BROWSER_SETUP_TIMEOUT_MESSAGE: &str =
    "browser_setup_timeout: истёк срок подготовки сеанса браузера";
const ITEM_TIMEOUT: Duration = Duration::from_secs(90);
const TLS_EVIDENCE_TIMEOUT: Duration = Duration::from_secs(2);
const BATCH_PACING: Duration = Duration::from_millis(900);
const RETRY_BACKOFF: Duration = Duration::from_millis(300);
const MAX_ACQUISITION_ATTEMPTS: u8 = 2;
const CAPTURE_VIEWPORT_WIDTH: i64 = 1280;
const CAPTURE_VIEWPORT_HEIGHT: i64 = 900;
const CAPTURE_DEVICE_SCALE_FACTOR: f64 = 2.175;
const CAPTURE_MIN_EDGE_PX: u32 = 140;
const CAPTURE_MAX_EDGE_PX: u32 = 220;
const CAPTURE_LIGHT_GLYPH_MIN_LUMINANCE: f64 = 0.60;
const YARXI_DARK_THEME_STYLE_ID: &str = "asset-store-yarxi-dark-theme";
const YARXI_DARK_THEME_STYLE: &str = ":root { color-scheme: dark !important; } #app { color: rgb(var(--w-base-color-rgb)) !important; }";

/// Источник байтов и ограниченный набор свидетельств при работе браузера.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SelectionResult {
    PrimaryGif,
    LeftmostPngFallback,
    RenderedFontSamplePng,
}

/// Явный выбор источника. `PreferredSource` используется по умолчанию;
/// `RenderedFontSamplePng` предназначен для отдельной приёмки рендеринга в браузере.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AcquisitionTarget {
    #[default]
    PreferredSource,
    RenderedFontSamplePng,
}

/// Полученный исходный бинарный файл.
#[derive(Debug, Clone, PartialEq)]
pub struct AcquiredMedia {
    pub character: String,
    pub source_url: String,
    pub article_unicode: String,
    pub selection: SelectionResult,
    pub bytes: Vec<u8>,
    pub evidence: AcquisitionEvidence,
}

/// Сохраняемые свидетельства выбора источника без состояния браузера и сеанса.
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
    #[serde(default)]
    pub browser_runtime: Option<BrowserRuntimeProvenance>,
    #[serde(default)]
    pub tls_exception: Option<TlsExceptionProvenance>,
    #[serde(default = "one_acquisition_attempt")]
    pub acquisition_attempts: u8,
}

fn one_acquisition_attempt() -> u8 {
    1
}

/// Принятое по явному флагу TLS-исключение для точного имени узла.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TlsExceptionProvenance {
    pub host: String,
    pub error_code: String,
}

#[derive(Debug, Clone)]
struct ApprovedTlsException {
    request_id: String,
    blocked_url: String,
    provenance: TlsExceptionProvenance,
}

/// Свидетельства для PNG, созданного рендерингом видимого элемента DOM `font-sample` Yarxi.
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

/// Чёткое состояние основного GIF для проверки политики и имитационных тестов.
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RuntimeReadiness {
    pending_relevant_requests: usize,
    network_failures: u32,
    http_errors: u32,
    javascript_exceptions: u32,
    monitor_failed: bool,
}

fn is_expected_yarxi_tls_url(raw_url: &str) -> bool {
    Url::parse(raw_url).is_ok_and(|url| {
        url.scheme() == "https"
            && url
                .host_str()
                .is_some_and(|host| host.eq_ignore_ascii_case(SITE_HOST))
            && url.port_or_known_default() == Some(443)
            && url.username().is_empty()
            && url.password().is_none()
    })
}

fn is_relevant_font_sample_resource(
    resource_type: &ResourceType,
    url: Option<&str>,
    selected_image_urls: &HashSet<String>,
) -> bool {
    resource_type != &ResourceType::Image
        || url.is_none_or(|url| url.is_empty() || selected_image_urls.contains(url))
}

#[derive(Debug, Clone)]
struct BrowserEvidenceMonitor {
    telemetry: CdpRuntimeMonitor,
}

impl BrowserEvidenceMonitor {
    fn new(telemetry: CdpRuntimeMonitor) -> Self {
        Self { telemetry }
    }

    fn begin_acquisition(&self) -> u64 {
        self.telemetry.begin_epoch()
    }

    fn readiness(&self, epoch: u64, excluded_primary_gif: Option<&str>) -> RuntimeReadiness {
        let snapshot = self.telemetry.snapshot(epoch);
        readiness_for_snapshot(&snapshot, |resource_type, url| {
            excluded_primary_gif.is_some_and(|excluded| {
                resource_type == &ResourceType::Image && url == Some(excluded)
            })
        })
    }

    fn font_sample_readiness(
        &self,
        epoch: u64,
        selected_image_urls: &HashSet<String>,
    ) -> RuntimeReadiness {
        let snapshot = self.telemetry.snapshot(epoch);
        readiness_for_snapshot(&snapshot, |resource_type, url| {
            !is_relevant_font_sample_resource(resource_type, url, selected_image_urls)
        })
    }

    fn tls_navigation_failure(&self) -> Option<NetworkOutcome> {
        self.telemetry
            .snapshot(0)
            .network_failures
            .into_iter()
            .rev()
            .find(|failure| {
                failure.resource_type == ResourceType::Document
                    && failure.is_top_level
                    && failure
                        .url
                        .as_deref()
                        .is_some_and(is_expected_yarxi_tls_url)
                    && failure.failure_reason.as_deref().is_some_and(|reason| {
                        reason.eq_ignore_ascii_case("net::ERR_CERT_AUTHORITY_INVALID")
                    })
            })
    }

    fn clear_explicitly_approved_tls_interstitial_failure(
        &self,
        request_id: &str,
        blocked_url: &str,
    ) {
        let exact_failure_is_approved =
            self.telemetry
                .snapshot(0)
                .network_failures
                .iter()
                .any(|failure| {
                    failure.resource_type == ResourceType::Document
                        && failure.is_top_level
                        && failure.request_id == request_id
                        && failure.url.as_deref() == Some(blocked_url)
                        && failure.failure_reason.as_deref().is_some_and(|reason| {
                            reason.eq_ignore_ascii_case("net::ERR_CERT_AUTHORITY_INVALID")
                        })
                });
        if exact_failure_is_approved {
            self.telemetry.clear_exact_network_failure(
                request_id,
                blocked_url,
                "net::ERR_CERT_AUTHORITY_INVALID",
            );
        }
    }

    fn retryable_acquisition(&self, epoch: u64, trigger: RetryTrigger) -> bool {
        self.telemetry.retryable_acquisition(epoch, trigger)
    }

    fn network_failure_details(&self, epoch: u64, excluded_primary_gif: Option<&str>) -> String {
        let failures: Vec<_> = self
            .telemetry
            .snapshot(epoch)
            .network_failures
            .into_iter()
            .filter(|failure| {
                !excluded_primary_gif.is_some_and(|excluded| {
                    failure.resource_type == ResourceType::Image
                        && failure.url.as_deref() == Some(excluded)
                })
            })
            .collect();
        browser_runtime::format_network_failure_details(&failures)
    }

    fn font_sample_network_failure_details(
        &self,
        epoch: u64,
        selected_image_urls: &HashSet<String>,
    ) -> String {
        let failures: Vec<_> = self
            .telemetry
            .snapshot(epoch)
            .network_failures
            .into_iter()
            .filter(|failure| {
                is_relevant_font_sample_resource(
                    &failure.resource_type,
                    failure.url.as_deref(),
                    selected_image_urls,
                )
            })
            .collect();
        browser_runtime::format_network_failure_details(&failures)
    }

    fn abort(&self) {
        self.telemetry.abort();
    }
}

fn readiness_for_snapshot(
    snapshot: &RuntimeSnapshot,
    mut excluded: impl FnMut(&ResourceType, Option<&str>) -> bool,
) -> RuntimeReadiness {
    RuntimeReadiness {
        pending_relevant_requests: snapshot
            .pending_requests
            .iter()
            .filter(|request| !excluded(&request.resource_type, Some(&request.url)))
            .count(),
        network_failures: snapshot
            .network_failures
            .iter()
            .filter(|failure| !excluded(&failure.resource_type, failure.url.as_deref()))
            .count() as u32,
        http_errors: snapshot
            .http_errors
            .iter()
            .filter(|failure| !excluded(&failure.resource_type, failure.url.as_deref()))
            .count() as u32,
        javascript_exceptions: snapshot.javascript_exceptions,
        monitor_failed: snapshot.monitor_failed,
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
                "элемент primary GIF есть, но загрузка завершилась без естественных размеров".into()
            } else {
                "элемент primary GIF существует, загрузка ещё не завершена".into()
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
                "область информации готова; Unicode статьи совпадает; все DOM-изображения загружены; DOM стабилен 2 секунды; img.kakijun-gif отсутствует; CDP Network/Runtime чисты (ожидают={}, ошибок сети={}, HTTP-ошибок={}, JS-исключений={})",
                runtime.pending_relevant_requests,
                runtime.network_failures,
                runtime.http_errors,
                runtime.javascript_exceptions
            ),
        }
    } else {
        PrimaryGifState::Unknown {
            reason: "область информации или медиа ещё не завершила загрузку и не стабилизировалась"
                .into(),
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
        "сетевая/runtime-среда не чиста (monitor_failed={}, ожидают={}, сетевых ошибок={}, HTTP-ошибок={}, JS-исключений={})",
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

/// Выбирает байты исходного изображения или PNG, отрендеренный из точного видимого `font-sample`.
/// URL Kakijun справа никогда не используется как источник.
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
        PrimaryGifState::PresentLoaded { .. } => {
            unreachable!("ветка для primary GIF обработана выше")
        }
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
            "gif_unknown: нельзя выбрать запасное изображение или образец шрифта: {reason}"
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

/// Выполняет изолированный пакет браузера. Страницу-предупреждение TLS можно
/// пропустить только с явным флагом и только для точного имени узла Yarxi.
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

/// Получает медиа с явно выбранной политикой источника. Инструменты приёмки
/// могут запросить PNG из `font-sample`; обычные вызовы используют [`acquire_many`]
/// с предпочтением основного GIF.
pub fn acquire_many_with_target(
    characters: &[String],
    allow_insecure_tls: bool,
    target: AcquisitionTarget,
) -> Result<Vec<Result<AcquiredMedia, String>>, String> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|error| format!("среда браузера: {error}"))?;
    runtime
        .block_on(async move { acquire_many_async(characters, allow_insecure_tls, target).await })
}

async fn acquire_many_async(
    characters: &[String],
    allow_insecure_tls: bool,
    target: AcquisitionTarget,
) -> Result<Vec<Result<AcquiredMedia, String>>, String> {
    if characters.is_empty() {
        return Ok(Vec::new());
    }
    for character in characters {
        crate::kanji_domain::parse_kanji_character(character)
            .map_err(|error| format!("invalid_kanji_identity: {error}"))?;
    }

    let session_deadline = Instant::now() + OPERATION_TIMEOUT;
    let runtime_config = BrowserRuntimeConfig {
        device_metrics: Some(
            DeviceMetrics::new(
                CAPTURE_VIEWPORT_WIDTH,
                CAPTURE_VIEWPORT_HEIGHT,
                CAPTURE_DEVICE_SCALE_FACTOR,
            )
            .map_err(|error| format!("настройка viewport Yarxi: {error}"))?,
        ),
        prefers_color_scheme: Some("dark".into()),
        ..BrowserRuntimeConfig::default()
    };
    let session =
        run_setup_before_deadline(session_deadline, BrowserSession::launch(runtime_config)).await?;
    let evidence_monitor = BrowserEvidenceMonitor::new(session.telemetry().clone());
    let browser_runtime = session.provenance().clone();

    let setup_result = run_setup_before_deadline(session_deadline, async {
        let page = session.page();
        let tls_exception = match page.goto(SITE_URL).await {
            Err(error) => {
                Some(
                    async_error_or_tls_interstitial(
                        page,
                        &evidence_monitor,
                        allow_insecure_tls,
                        error,
                    )
                    .await?,
                )
            }
            Ok(_) => {
                let tls_probe: Value = page
                    .evaluate("() => ({ code: document.querySelector('#error-code')?.textContent?.trim() || '', proceed: Boolean(document.querySelector('#proceed-link')) })")
                    .await
                    .map_err(|error| format!("проверка TLS-страницы: {error}"))?
                    .into_value()
                    .map_err(|error| format!("ответ проверки TLS-страницы: {error}"))?;
                if !tls_probe["code"].as_str().unwrap_or_default().is_empty() {
                    Some(
                        async_error_or_tls_interstitial(
                            page,
                            &evidence_monitor,
                            allow_insecure_tls,
                            tls_probe["code"].as_str().unwrap_or_default(),
                        )
                        .await?,
                    )
                } else {
                    None
                }
            }
        };
        let site_host: bool = page
            .evaluate(format!("location.hostname === {SITE_HOST:?}"))
            .await
            .map_err(|error| format!("проверка host Yarxi: {error}"))?
            .into_value()
            .map_err(|error| format!("ответ проверки host Yarxi: {error}"))?;
        if !site_host {
            return Err("provider_host_mismatch: загрузка ушла с www.yarxi.su".into());
        }
        wait_until(page, Duration::from_secs(20), || {
            "Boolean([...document.querySelectorAll('.kanji-search-form input[placeholder=\"Чтение\"]')].find(node => node.getClientRects().length > 0 && getComputedStyle(node).visibility !== 'hidden'))"
        })
        .await?;
        if let Some(approved) = &tls_exception {
            evidence_monitor.clear_explicitly_approved_tls_interstitial_failure(
                &approved.request_id,
                &approved.blocked_url,
            );
        }
        if target == AcquisitionTarget::RenderedFontSamplePng {
            apply_dark_theme(page).await?;
        }
        Ok::<_, String>(tls_exception)
    })
    .await;

    let tls_exception = match setup_result {
        Ok(setup) => setup,
        Err(error) => {
            session.close().await;
            return Err(error);
        }
    };

    let mut outcomes = Vec::with_capacity(characters.len());
    for (index, character) in characters.iter().enumerate() {
        if index > 0
            && timeout_at(session_deadline, sleep(BATCH_PACING))
                .await
                .is_err()
        {
            append_session_deadline_outcomes(&mut outcomes, characters.len() - index);
            break;
        }
        if Instant::now() >= session_deadline {
            append_session_deadline_outcomes(&mut outcomes, characters.len() - index);
            break;
        }
        let (outcome, stop_reason) = acquire_one_with_retries(
            session.page(),
            character,
            target,
            &evidence_monitor,
            &browser_runtime,
            tls_exception
                .as_ref()
                .map(|approved| approved.provenance.clone()),
            session_deadline,
        )
        .await;
        outcomes.push(outcome);
        if let Some(stop_reason) = stop_reason {
            append_batch_stopped_outcomes(
                &mut outcomes,
                characters.len().saturating_sub(index + 1),
                &stop_reason,
            );
            break;
        }
    }
    evidence_monitor.abort();
    session.close().await;
    Ok(outcomes)
}

fn append_session_deadline_outcomes<T>(outcomes: &mut Vec<Result<T, String>>, count: usize) {
    append_batch_stopped_outcomes(outcomes, count, &BatchStopReason::SessionDeadline);
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum BatchStopReason {
    SessionDeadline,
    ItemTimeout,
    RetryRecoveryFailed(String),
}

fn append_batch_stopped_outcomes<T>(
    outcomes: &mut Vec<Result<T, String>>,
    count: usize,
    reason: &BatchStopReason,
) {
    let error = match reason {
        BatchStopReason::SessionDeadline => {
            "browser_session_deadline: общий срок пакета истёк".to_owned()
        }
        BatchStopReason::ItemTimeout => "browser_batch_stopped_after_item_timeout: обработка предыдущего символа отменена по таймауту; сеанс браузера остановлен, чтобы поздний ответ не изменил страницу".to_owned(),
        BatchStopReason::RetryRecoveryFailed(detail) => format!(
            "browser_batch_stopped_after_retry_recovery_failure: не удалось безопасно восстановить страницу: {detail}"
        ),
    };
    outcomes.extend((0..count).map(|_| Err(error.clone())));
}

async fn acquire_one_with_retries(
    page: &Page,
    character: &str,
    target: AcquisitionTarget,
    evidence_monitor: &BrowserEvidenceMonitor,
    browser_runtime: &BrowserRuntimeProvenance,
    tls_exception: Option<TlsExceptionProvenance>,
    session_deadline: Instant,
) -> (Result<AcquiredMedia, String>, Option<BatchStopReason>) {
    for attempt in 1..=MAX_ACQUISITION_ATTEMPTS {
        if Instant::now() >= session_deadline {
            return (
                Err("browser_session_deadline: общий срок пакета истёк".into()),
                Some(BatchStopReason::SessionDeadline),
            );
        }
        let epoch = evidence_monitor.begin_acquisition();
        let item_deadline = (Instant::now() + ITEM_TIMEOUT).min(session_deadline);
        let acquisition = timeout_at(
            item_deadline,
            acquire_one(
                page,
                character,
                target,
                epoch,
                evidence_monitor,
                browser_runtime,
                tls_exception.clone(),
            ),
        )
        .await;
        let (result, timed_out) = match acquisition {
            Ok(result) => (result, false),
            Err(_) if Instant::now() >= session_deadline => {
                return (
                    Err("browser_session_deadline: общий срок пакета истёк".into()),
                    Some(BatchStopReason::SessionDeadline),
                );
            }
            Err(_) => (
                Err("browser_item_timeout: превышен ограниченный срок обработки символа".into()),
                true,
            ),
        };

        match result {
            Ok(mut media) => {
                media.evidence.acquisition_attempts = attempt;
                return (Ok(media), None);
            }
            Err(error) => {
                if should_retry_acquisition(&error, evidence_monitor, epoch, attempt) {
                    if timeout_at(session_deadline, sleep(RETRY_BACKOFF))
                        .await
                        .is_err()
                    {
                        return (
                            Err("browser_session_deadline: общий срок пакета истёк".into()),
                            Some(BatchStopReason::SessionDeadline),
                        );
                    }
                    let recovery_deadline = (Instant::now()
                        + ITEM_TIMEOUT.min(Duration::from_secs(30)))
                    .min(session_deadline);
                    match timeout_at(
                        recovery_deadline,
                        recover_page_for_retry(page, target, evidence_monitor, recovery_deadline),
                    )
                    .await
                    {
                        Ok(Ok(())) => {}
                        Err(_) if Instant::now() >= session_deadline => {
                            return (
                                Err("browser_session_deadline: общий срок пакета истёк при восстановлении страницы".into()),
                                Some(BatchStopReason::SessionDeadline),
                            );
                        }
                        Ok(Err(recovery_error)) => {
                            return (
                                Err(format!(
                                    "{error}; browser_retry_recovery_failed: {recovery_error}"
                                )),
                                Some(BatchStopReason::RetryRecoveryFailed(recovery_error)),
                            );
                        }
                        Err(_) => {
                            let error = "восстановление страницы превысило ограниченный срок";
                            return (
                                Err(format!("{error}; browser_retry_recovery_failed")),
                                Some(BatchStopReason::RetryRecoveryFailed(error.to_owned())),
                            );
                        }
                    }
                    continue;
                }
                if timed_out {
                    return (Err(error), Some(BatchStopReason::ItemTimeout));
                }
                return (Err(error), None);
            }
        }
    }
    (
        Err("browser_retry_exhausted: лимит попыток acquisition исчерпан".into()),
        None,
    )
}

fn is_retryable_acquisition_error(
    error: &str,
    evidence_monitor: &BrowserEvidenceMonitor,
    epoch: u64,
) -> bool {
    let trigger = if error.starts_with("browser_item_timeout:") {
        RetryTrigger::ItemTimeout
    } else if error.starts_with("browser_readiness_timeout:") {
        RetryTrigger::ReadinessTimeout
    } else if error.starts_with("browser_network_runtime_failure:") {
        RetryTrigger::RuntimeFailure
    } else {
        return false;
    };
    evidence_monitor.retryable_acquisition(epoch, trigger)
}

fn should_retry_acquisition(
    error: &str,
    evidence_monitor: &BrowserEvidenceMonitor,
    epoch: u64,
    attempt: u8,
) -> bool {
    attempt < MAX_ACQUISITION_ATTEMPTS
        && is_retryable_acquisition_error(error, evidence_monitor, epoch)
}

async fn recover_page_for_retry(
    page: &Page,
    target: AcquisitionTarget,
    evidence_monitor: &BrowserEvidenceMonitor,
    recovery_deadline: Instant,
) -> Result<(), String> {
    let recovery_epoch = evidence_monitor.begin_acquisition();
    page.goto("about:blank")
        .await
        .map_err(|error| format!("browser_retry_recovery_blank: {error}"))?;
    page.goto(SITE_URL)
        .await
        .map_err(|error| format!("browser_retry_recovery_navigation: {error}"))?;
    let site_host: bool = page
        .evaluate(format!("location.hostname === {SITE_HOST:?}"))
        .await
        .map_err(|error| format!("browser_retry_recovery_host_check: {error}"))?
        .into_value()
        .map_err(|error| format!("browser_retry_recovery_host_result: {error}"))?;
    if !site_host {
        return Err("browser_retry_recovery_host_mismatch: загрузка ушла с www.yarxi.su".into());
    }
    wait_until(page, Duration::from_secs(20), || {
        "Boolean([...document.querySelectorAll('.kanji-search-form input[placeholder=\"Чтение\"]')].find(node => node.getClientRects().length > 0 && getComputedStyle(node).visibility !== 'hidden'))"
    })
    .await
    .map_err(|error| format!("browser_retry_recovery_form: {error}"))?;
    if target == AcquisitionTarget::RenderedFontSamplePng {
        apply_dark_theme(page)
            .await
            .map_err(|error| format!("browser_retry_recovery_dark_theme: {error}"))?;
    }

    loop {
        if Instant::now() >= recovery_deadline {
            return Err(
                "browser_retry_recovery_timeout: истёк срок восстановления страницы".into(),
            );
        }
        let runtime = evidence_monitor.readiness(recovery_epoch, None);
        if runtime.monitor_failed
            || runtime.network_failures > 0
            || runtime.http_errors > 0
            || runtime.javascript_exceptions > 0
        {
            return Err(format!(
                "browser_retry_recovery_runtime_failure: {}",
                runtime_failure_reason(runtime)
            ));
        }
        if runtime.pending_relevant_requests == 0 {
            return Ok(());
        }
        sleep(Duration::from_millis(50)).await;
    }
}

async fn apply_dark_theme(page: &Page) -> Result<(), String> {
    let style_id = serde_json::to_string(YARXI_DARK_THEME_STYLE_ID)
        .map_err(|error| format!("сериализация ID стиля тёмной темы: {error}"))?;
    let style_text = serde_json::to_string(YARXI_DARK_THEME_STYLE)
        .map_err(|error| format!("сериализация stylesheet тёмной темы: {error}"))?;
    let script = format!(
        r#"() => {{
            const root = document.documentElement;
            if (!root) return {{ error: 'корневой элемент document отсутствует' }};
            root.dataset.theme = 'dark';
            const styleId = {style_id};
            const styleText = {style_text};
            let style = document.getElementById(styleId);
            if (style && style.textContent !== styleText)
                return {{ error: 'идентификатор stylesheet тёмной темы уже занят' }};
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
            "yarxi_dark_theme_unavailable: палитра Yarxi или наследование цвета текста страницы не перешли в проверенное тёмное состояние: {state}"
        ));
    }
    Ok(())
}

async fn async_error_or_tls_interstitial(
    page: &Page,
    evidence_monitor: &BrowserEvidenceMonitor,
    allow_insecure_tls: bool,
    original: impl std::fmt::Display,
) -> Result<ApprovedTlsException, String> {
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
    let failed_request = wait_for_tls_navigation_failure(evidence_monitor).await?;
    let blocked_url = failed_request
        .url
        .as_deref()
        .ok_or_else(|| "TLS interstitial не связан с URL ошибочного Document request".to_owned())?;
    if let Err(reason) = check_tls_exception(
        allow_insecure_tls,
        blocked_url,
        code,
        &original.to_string(),
        interstitial["proceed"].as_bool().unwrap_or(false),
    ) {
        return Err(format!(
            "TLS exception отклонён: {reason}; original={original}; interstitial={interstitial}"
        ));
    }
    // Переход Chrome interstitial действует только для origin; глобальный
    // ignore-certificate-errors для процесса браузера не включён.
    page.evaluate("() => document.querySelector('#proceed-link').click()")
        .await
        .map_err(|error| format!("не удалось принять исключение Yarxi: {error}"))?;
    wait_until(page, Duration::from_secs(20), || {
        "location.hostname === 'www.yarxi.su' && Boolean(document.querySelector('.kanji-search-form input[placeholder=\"Чтение\"]'))"
    })
    .await?;
    Ok(ApprovedTlsException {
        request_id: failed_request.request_id,
        blocked_url: blocked_url.to_owned(),
        provenance: TlsExceptionProvenance {
            host: SITE_HOST.to_owned(),
            error_code: "NET::ERR_CERT_AUTHORITY_INVALID".to_owned(),
        },
    })
}

async fn wait_for_tls_navigation_failure(
    evidence_monitor: &BrowserEvidenceMonitor,
) -> Result<NetworkOutcome, String> {
    let deadline = Instant::now() + TLS_EVIDENCE_TIMEOUT;
    loop {
        if let Some(failure) = evidence_monitor.tls_navigation_failure() {
            return Ok(failure);
        }
        if Instant::now() >= deadline {
            return Err(
                "TLS interstitial не связан с ошибкой top-level Document event Yarxi".into(),
            );
        }
        sleep(Duration::from_millis(20)).await;
    }
}

fn check_tls_exception(
    explicitly_allowed: bool,
    blocked_url: &str,
    interstitial_code: &str,
    original_error: &str,
    proceed_link_present: bool,
) -> Result<(), String> {
    if !explicitly_allowed {
        return Err("нужен явный --allow-insecure-tls".into());
    }
    let parsed_blocked_url =
        Url::parse(blocked_url).map_err(|_| "URL ошибочного Document некорректен")?;
    if !is_expected_yarxi_tls_url(blocked_url) {
        return Err(format!(
            "неожиданный URL ошибочного Document {parsed_blocked_url}"
        ));
    }
    if !interstitial_code.eq_ignore_ascii_case("NET::ERR_CERT_AUTHORITY_INVALID")
        || !original_error
            .to_ascii_uppercase()
            .contains("ERR_CERT_AUTHORITY_INVALID")
    {
        return Err("разрешена только ERR_CERT_AUTHORITY_INVALID".into());
    }
    if !proceed_link_present {
        return Err("interstitial Chrome не предоставляет ссылку перехода".into());
    }
    Ok(())
}

async fn acquire_one(
    page: &Page,
    character: &str,
    target: AcquisitionTarget,
    epoch: u64,
    evidence_monitor: &BrowserEvidenceMonitor,
    browser_runtime: &BrowserRuntimeProvenance,
    tls_exception: Option<TlsExceptionProvenance>,
) -> Result<AcquiredMedia, String> {
    crate::kanji_domain::parse_kanji_character(character)
        .map_err(|error| format!("invalid_kanji_identity: {error}"))?;
    let character_json = serde_json::to_string(character)
        .map_err(|error| format!("сериализация символа: {error}"))?;
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
        .expect("выше проверен один символ");
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

    let mut snapshot = wait_for_media_snapshot(
        page,
        character,
        &expected_code,
        epoch,
        evidence_monitor,
        target,
    )
    .await?;
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
        snapshot = wait_for_media_snapshot(
            page,
            character,
            &expected_code,
            epoch,
            evidence_monitor,
            target,
        )
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
        return Err("media_empty: браузер вернул пустые байты".into());
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
        browser_runtime: Some(browser_runtime.clone()),
        tls_exception,
        acquisition_attempts: 1,
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
    epoch: u64,
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
            .expect("сериализация DOM-снимка не должна завершаться ошибкой");
        let selected_font_sample = (target == AcquisitionTarget::RenderedFontSamplePng)
            .then(|| choose_font_sample(&font_samples, expected_character).ok())
            .flatten();
        let selected_sample_image_urls: HashSet<String> = selected_font_sample
            .as_ref()
            .into_iter()
            .flat_map(|sample| sample.images.iter().map(|image| image.url.clone()))
            .collect();
        let runtime = if target == AcquisitionTarget::RenderedFontSamplePng {
            evidence_monitor.font_sample_readiness(epoch, &selected_sample_image_urls)
        } else {
            evidence_monitor.readiness(epoch, None)
        };
        if runtime.monitor_failed || runtime.network_failures > 0 || runtime.http_errors > 0 {
            let network_details = if target == AcquisitionTarget::RenderedFontSamplePng {
                evidence_monitor
                    .font_sample_network_failure_details(epoch, &selected_sample_image_urls)
            } else {
                evidence_monitor.network_failure_details(epoch, None)
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
            evidence_monitor.readiness(epoch, None),
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
            PrimaryGifState::Unknown { reason } if reason.contains("без естественных размеров") =>
            {
                return Err(format!("gif_present_but_failed: {reason}"));
            }
            PrimaryGifState::Unknown { .. } => {}
        }
        if tokio::time::Instant::now() >= deadline {
            let reason = runtime_failure_reason(evidence_monitor.readiness(epoch, None));
            if primary.as_ref().is_some_and(|image| !image.complete) {
                return Err(format!(
                    "gif_pending: загрузка primary GIF не завершилась; {reason}"
                ));
            }
            return Err(format!(
                "gif_unknown: область медиа не достигла завершённого и стабильного состояния; {reason}"
            ));
        }
        sleep(Duration::from_millis(200)).await;
    }
}

async fn resource_bytes(page: &Page, resource_url: &str) -> Result<Vec<u8>, String> {
    if resource_url.starts_with("data:") || resource_url.starts_with("blob:") {
        let url_json = serde_json::to_string(resource_url)
            .map_err(|error| format!("сериализация URL ресурса: {error}"))?;
        let script = format!(
            "async () => {{ const response = await fetch({url_json}); if (!response.ok) throw new Error('HTTP ' + response.status); const bytes = new Uint8Array(await response.arrayBuffer()); let binary = ''; for (let i = 0; i < bytes.length; i += 0x8000) binary += String.fromCharCode(...bytes.subarray(i, i + 0x8000)); return btoa(binary); }}"
        );
        let encoded: String = page
            .evaluate(script)
            .await
            .map_err(|error| {
                format!("не удалось получить ресурс, обслуживаемый браузером: {error}")
            })?
            .into_value()
            .map_err(|error| {
                format!("обслуживаемый браузером ресурс вернул некорректные bytes: {error}")
            })?;
        return base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(|error| format!("декодирование base64 для ресурса браузера: {error}"));
    }
    let url =
        Url::parse(resource_url).map_err(|error| format!("некорректный media URL: {error}"))?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || url.host_str().is_some_and(is_kakijun_host)
    {
        return Err("source_url_rejected: ожидается HTTPS-ресурс, не связанный с Kakijun".into());
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
        return Err("media_response_not_binary: браузер не сохранил исходные байты".into());
    }
    base64::engine::general_purpose::STANDARD
        .decode(content.result.content)
        .map_err(|error| format!("декодирование media из base64: {error}"))
}

async fn render_font_sample_png(
    page: &Page,
    sample: &FontSampleState,
    expected_character: &str,
    expected_code: &str,
) -> Result<(Vec<u8>, RenderedFontSampleEvidence), String> {
    let expected_json = serde_json::to_string(expected_character)
        .map_err(|error| format!("сериализация символа font-sample: {error}"))?;
    let expected_code_json = serde_json::to_string(expected_code)
        .map_err(|error| format!("сериализация Unicode font-sample: {error}"))?;
    let expected_class_json = serde_json::to_string(&sample.class_name)
        .map_err(|error| format!("сериализация класса font-sample: {error}"))?;
    let expected_title_json = serde_json::to_string(&sample.title)
        .map_err(|error| format!("сериализация title font-sample: {error}"))?;
    let expected_text_json = serde_json::to_string(&sample.text)
        .map_err(|error| format!("сериализация текста font-sample: {error}"))?;
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
            return { error: 'Unicode статьи изменился или неоднозначен' };
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
        if (!selected) return { error: 'нет видимого font-sample с точным символом' };
        if (selected.index !== expectedIndex || selected.className !== expectedClass
            || selected.text !== expectedText
            || (selected.node.getAttribute('title') || '') !== expectedTitle)
            return { error: 'идентичность крайнего левого font-sample изменилась до capture' };
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
            return { error: 'inline style font-sample превышает ограничение evidence' };
        if (root.dataset.theme !== 'dark' || !matchMedia('(prefers-color-scheme: dark)').matches
            || luminance(pageBackground) === null || luminance(pageBackground) >= 0.20
            || luminance(tileBackground) === null || luminance(tileBackground) >= 0.20
            || luminance(foreground) === null || luminance(foreground) <= 0.75 || !borderVisible)
            return { error: `не удалось подготовить плитку к рендерингу (theme_class=${String(root.className).slice(0, 64)}; prefers_dark=${matchMedia('(prefers-color-scheme: dark)').matches}; page_bg=${String(pageBackground).slice(0, 64)}; tile_bg=${String(tileBackground).slice(0, 64)}; foreground=${String(foreground).slice(0, 64)}; border=${String(visibleBorderWidth).slice(0, 32)} ${String(visibleBorderStyle).slice(0, 32)} ${String(visibleBorderColor).slice(0, 64)}; border_visible=${borderVisible})` };
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
        return Err("font_sample_png_invalid: снимок элемента не вернул PNG bytes".into());
    }
    let decoded = image::load_from_memory_with_format(&bytes, image::ImageFormat::Png)
        .map_err(|error| format!("font_sample_png_decode: {error}"))?;
    let (pixel_width, pixel_height) = decoded.dimensions();
    validate_capture_dimensions(pixel_width, pixel_height)?;
    let pixel_evidence = analyze_capture_pixels(&decoded.to_rgba8())?;
    let style_json = serde_json::to_string(&initial["inline_style_json"].as_str())
        .map_err(|error| format!("сериализация стиля font-sample: {error}"))?;
    let document_theme_json = serde_json::to_string(&initial["document_theme"].as_str())
        .map_err(|error| format!("сериализация темы document: {error}"))?;
    let style_id_json = serde_json::to_string(YARXI_DARK_THEME_STYLE_ID)
        .map_err(|error| format!("сериализация ID стиля тёмной темы: {error}"))?;
    let style_text_json = serde_json::to_string(YARXI_DARK_THEME_STYLE)
        .map_err(|error| format!("сериализация stylesheet тёмной темы: {error}"))?;
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
            "font_sample_style_changed: стиль выбранной плитки или тёмная тема изменились во время снимка"
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
        capture: "Снимок Chromium самого отрендеренного элемента .font-sample без изменений при Yarxi data-theme=dark".into(),
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
        dark_environment: "Yarxi data-theme=dark; color-scheme=dark; цвет #app наследуется от --w-base-color-rgb на уровне страницы".into(),
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
            "font_sample_pixel_contract: недостаточно тёмных пикселей плитки ({}/{total_pixels})",
            dark.count
        ));
    }
    if glyph.count < 16 {
        return Err(format!(
            "font_sample_pixel_contract: недостаточно светлых пикселей glyph ({})",
            glyph.count
        ));
    }
    if frame.count < 8 {
        return Err(format!(
            "font_sample_pixel_contract: недостаточно видимых пикселей рамки среднего тона ({})",
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
            .map_err(|error| format!("проверка готовности браузера: {error}"))?
            .into_value()
            .map_err(|error| format!("ответ проверки готовности браузера: {error}"))?;
        if ready {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(
                "browser_readiness_timeout: истёк срок ожидания готовности страницы".into(),
            );
        }
        sleep(Duration::from_millis(200)).await;
    }
}

async fn run_setup_before_deadline<T>(
    deadline: Instant,
    setup: impl Future<Output = Result<T, String>>,
) -> Result<T, String> {
    timeout_at(deadline, setup)
        .await
        .unwrap_or_else(|_| Err(BROWSER_SETUP_TIMEOUT_MESSAGE.into()))
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
                    reason: "запрос ещё выполняется".into(),
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
            "ссылка Kakijun справа не должна заменять GIF слева"
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

    fn test_outcome(
        resource_type: ResourceType,
        url: Option<&str>,
        failure_reason: Option<&str>,
        request_id: &str,
        epoch: u64,
        status_code: Option<u16>,
    ) -> NetworkOutcome {
        NetworkOutcome {
            resource_type,
            url: url.map(str::to_owned),
            failure_reason: failure_reason.map(str::to_owned),
            request_id: request_id.to_owned(),
            epoch,
            status_code,
            is_top_level: false,
        }
    }

    fn tracked_request(
        request_id: &str,
        resource_type: ResourceType,
        url: &str,
        epoch: u64,
    ) -> TrackedRequest {
        TrackedRequest {
            request_id: request_id.to_owned(),
            resource_type,
            url: url.to_owned(),
            epoch,
            is_top_level: false,
        }
    }

    #[test]
    fn acquisition_retry_requires_transient_runtime_evidence_and_stops_at_attempt_limit() {
        let epoch = 1;
        let telemetry = CdpRuntimeMonitor::from_snapshot_for_test(RuntimeSnapshot {
            network_failures: vec![test_outcome(
                ResourceType::Fetch,
                Some("https://example.test/retry"),
                Some("net::ERR_CONNECTION_RESET"),
                "retryable",
                epoch,
                None,
            )],
            ..RuntimeSnapshot::default()
        });
        let monitor = BrowserEvidenceMonitor::new(telemetry);
        assert!(is_retryable_acquisition_error(
            "browser_item_timeout: ограниченный срок истёк",
            &monitor,
            epoch,
        ));
        assert!(should_retry_acquisition(
            "browser_item_timeout: ограниченный срок истёк",
            &monitor,
            epoch,
            1,
        ));
        assert!(!should_retry_acquisition(
            "browser_item_timeout: ограниченный срок истёк",
            &monitor,
            epoch,
            2,
        ));
        assert!(!should_retry_acquisition(
            "browser_item_timeout: ограниченный срок истёк",
            &monitor,
            epoch,
            2,
        ));
    }

    #[test]
    fn acquisition_retry_preserves_trigger_specific_evidence_rules() {
        let epoch = 1;
        let monitor = BrowserEvidenceMonitor::new(CdpRuntimeMonitor::from_snapshot_for_test(
            RuntimeSnapshot {
                http_errors: vec![test_outcome(
                    ResourceType::Fetch,
                    Some("https://example.test/permanent"),
                    None,
                    "permanent",
                    epoch,
                    Some(404),
                )],
                ..RuntimeSnapshot::default()
            },
        ));

        assert!(!is_retryable_acquisition_error(
            "browser_item_timeout: ограниченный срок истёк",
            &monitor,
            epoch,
        ));
        assert!(is_retryable_acquisition_error(
            "browser_readiness_timeout: не готова страница",
            &monitor,
            epoch,
        ));
        assert!(!is_retryable_acquisition_error(
            "browser_network_runtime_failure: постоянная HTTP-ошибка",
            &monitor,
            epoch,
        ));
        assert!(!is_retryable_acquisition_error(
            "browser_network_runtime_failure: нет сетевой ошибки",
            &monitor,
            epoch + 1,
        ));
        assert!(!is_retryable_acquisition_error(
            "неповторяемая ошибка",
            &monitor,
            epoch,
        ));

        let clean_monitor = BrowserEvidenceMonitor::new(CdpRuntimeMonitor::from_snapshot_for_test(
            RuntimeSnapshot::default(),
        ));
        assert!(is_retryable_acquisition_error(
            "browser_item_timeout: ограниченный срок истёк",
            &clean_monitor,
            epoch,
        ));
    }

    #[tokio::test]
    async fn setup_deadline_rejects_work_after_expiry() {
        let deadline = Instant::now() - Duration::from_millis(1);
        let result =
            run_setup_before_deadline(deadline, std::future::pending::<Result<(), String>>()).await;

        assert_eq!(result, Err(BROWSER_SETUP_TIMEOUT_MESSAGE.into()));
    }

    #[tokio::test]
    async fn setup_deadline_accepts_work_completed_before_expiry() {
        let result = run_setup_before_deadline(Instant::now() + Duration::from_secs(1), async {
            Ok::<_, String>("готово")
        })
        .await;

        assert_eq!(result, Ok("готово"));
    }

    #[test]
    fn session_deadline_outcomes_keep_completed_prefix_and_fill_remaining_items() {
        let mut outcomes = vec![Ok("first"), Ok("second")];
        append_session_deadline_outcomes(&mut outcomes, 2);
        assert_eq!(outcomes.len(), 4);
        assert_eq!(outcomes[0], Ok("first"));
        assert_eq!(outcomes[1], Ok("second"));
        assert!(
            outcomes[2]
                .as_ref()
                .unwrap_err()
                .starts_with("browser_session_deadline:")
        );
        assert!(
            outcomes[3]
                .as_ref()
                .unwrap_err()
                .starts_with("browser_session_deadline:")
        );
    }

    #[test]
    fn item_timeout_keeps_completed_prefix_and_uses_a_distinct_batch_stop_reason() {
        let mut outcomes = vec![
            Ok("first"),
            Ok("second"),
            Err("browser_item_timeout: item".into()),
        ];
        append_batch_stopped_outcomes(&mut outcomes, 2, &BatchStopReason::ItemTimeout);

        assert_eq!(outcomes.len(), 5);
        assert_eq!(outcomes[0], Ok("first"));
        assert_eq!(outcomes[1], Ok("second"));
        assert!(
            outcomes[2]
                .as_ref()
                .unwrap_err()
                .starts_with("browser_item_timeout:")
        );
        for outcome in &outcomes[3..] {
            let error = outcome.as_ref().unwrap_err();
            assert!(error.starts_with("browser_batch_stopped_after_item_timeout:"));
            assert!(!error.starts_with("browser_session_deadline:"));
        }
    }

    #[test]
    fn browser_runtime_provenance_keeps_selection_method_without_local_paths() {
        let provenance = BrowserRuntimeProvenance {
            product: "Chrome/140.0.0.0".into(),
            protocol_version: "1.3".into(),
            revision: "abc123".into(),
            user_agent: "Mozilla/5.0 Chrome/140".into(),
            js_version: "14.0".into(),
            executable_source: BrowserExecutableSource::PathLookup,
        };
        let encoded = serde_json::to_string(&provenance).unwrap();
        assert!(encoded.contains("path_lookup"));
        assert!(encoded.contains("Chrome/140.0.0.0"));
        assert!(!encoded.contains("/home/"));
        assert!(!encoded.contains("C:\\"));
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
                    proof: "стабильный и полностью загруженный DOM".into(),
                },
                &[left_text.clone(), later_image],
                "柘",
            )
            .unwrap(),
            MediaSourceChoice::RenderedFontSample {
                sample: left_text,
                absence_proof: Some("стабильный и полностью загруженный DOM".into()),
            },
            "PNG в плитке справа не должен заменять крайний левый font-sample"
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
                    proof: "стабильный и полностью загруженный DOM".into(),
                },
                &[left_image, later_text],
                "柘",
            )
            .unwrap(),
            MediaSourceChoice::Url {
                selection: SelectionResult::LeftmostPngFallback,
                url: "https://example.test/first.png".into(),
                absence_proof: Some("стабильный и полностью загруженный DOM".into()),
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
                    proof: "стабильный и полностью загруженный DOM".into(),
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
                    proof: "стабильный и полностью загруженный DOM".into(),
                },
                &samples,
                "柘",
            )
            .unwrap(),
            MediaSourceChoice::RenderedFontSample {
                sample: samples[1].clone(),
                absence_proof: Some("стабильный и полностью загруженный DOM".into()),
            }
        );
        assert!(
            choose_media_source(
                PrimaryGifState::AbsentConfirmed {
                    proof: "стабильный и полностью загруженный DOM".into(),
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
        let snapshot = RuntimeSnapshot {
            pending_requests: vec![
                tracked_request(
                    "gif",
                    ResourceType::Image,
                    "https://example.test/primary.gif",
                    0,
                ),
                tracked_request(
                    "script",
                    ResourceType::Script,
                    "https://example.test/article.js",
                    0,
                ),
            ],
            network_failures: vec![test_outcome(
                ResourceType::Image,
                Some("https://example.test/primary.gif"),
                Some("net::ERR_FAILED"),
                "gif",
                0,
                None,
            )],
            http_errors: vec![test_outcome(
                ResourceType::Image,
                Some("https://example.test/primary.gif"),
                None,
                "gif",
                0,
                Some(404),
            )],
            ..RuntimeSnapshot::default()
        };
        let all = readiness_for_snapshot(&snapshot, |_, _| false);
        assert_eq!(all.pending_relevant_requests, 2);
        let explicit = readiness_for_snapshot(&snapshot, |resource_type, url| {
            resource_type == &ResourceType::Image && url == Some("https://example.test/primary.gif")
        });
        assert_eq!(explicit.pending_relevant_requests, 1);
        assert_eq!(explicit.network_failures, 0);
        assert_eq!(explicit.http_errors, 0);
        let other_primary = readiness_for_snapshot(&snapshot, |resource_type, url| {
            resource_type == &ResourceType::Image && url == Some("https://example.test/other.gif")
        });
        assert_eq!(other_primary.network_failures, 1);
    }

    #[test]
    fn rendered_sample_readiness_tracks_its_image_but_ignores_sibling_images() {
        let selected_url = "https://example.test/sample.png";
        let sibling_url = "https://yosida.example/stroke-order.gif";
        let selected_images = HashSet::from([selected_url.to_owned()]);
        let snapshot = RuntimeSnapshot {
            pending_requests: vec![
                tracked_request("selected-image", ResourceType::Image, selected_url, 1),
                tracked_request("sibling-image", ResourceType::Image, sibling_url, 1),
                tracked_request(
                    "script",
                    ResourceType::Script,
                    "https://example.test/article.js",
                    1,
                ),
            ],
            network_failures: vec![
                test_outcome(
                    ResourceType::Image,
                    Some(sibling_url),
                    Some("net::ERR_ABORTED"),
                    "sibling-image",
                    1,
                    None,
                ),
                test_outcome(
                    ResourceType::Image,
                    Some(selected_url),
                    Some("net::ERR_FAILED"),
                    "selected-image",
                    1,
                    None,
                ),
                test_outcome(
                    ResourceType::Script,
                    Some("https://example.test/article.js"),
                    Some("net::ERR_FAILED"),
                    "script",
                    1,
                    None,
                ),
            ],
            ..RuntimeSnapshot::default()
        };

        let readiness = readiness_for_snapshot(&snapshot, |resource_type, url| {
            !is_relevant_font_sample_resource(resource_type, url, &selected_images)
        });
        assert_eq!(readiness.pending_relevant_requests, 2);
        assert_eq!(readiness.network_failures, 2);
        assert_eq!(readiness.http_errors, 0);
    }

    #[test]
    fn tls_exception_clears_only_the_approved_exact_document_failure() {
        let mut approved_failure = test_outcome(
            ResourceType::Document,
            Some(SITE_URL),
            Some("net::ERR_CERT_AUTHORITY_INVALID"),
            "approved-request",
            0,
            None,
        );
        approved_failure.is_top_level = true;
        let mut attacker_failure = test_outcome(
            ResourceType::Document,
            Some("https://attacker.example/"),
            Some("net::ERR_CERT_AUTHORITY_INVALID"),
            "attacker-request",
            0,
            None,
        );
        attacker_failure.is_top_level = true;
        let telemetry = CdpRuntimeMonitor::from_snapshot_for_test(RuntimeSnapshot {
            network_failures: vec![
                approved_failure,
                attacker_failure,
                test_outcome(
                    ResourceType::Document,
                    Some(SITE_URL),
                    Some("net::ERR_CERT_AUTHORITY_INVALID"),
                    "iframe-request",
                    0,
                    None,
                ),
                test_outcome(
                    ResourceType::Document,
                    Some(SITE_URL),
                    Some("net::ERR_CERT_DATE_INVALID"),
                    "different-error",
                    0,
                    None,
                ),
            ],
            ..RuntimeSnapshot::default()
        });
        let monitor = BrowserEvidenceMonitor::new(telemetry);
        assert_eq!(
            monitor
                .tls_navigation_failure()
                .map(|failure| failure.request_id),
            Some("approved-request".into())
        );
        monitor.clear_explicitly_approved_tls_interstitial_failure("approved-request", SITE_URL);
        let failures = monitor.telemetry.snapshot(0).network_failures;
        assert_eq!(failures.len(), 3);
        assert!(
            failures
                .iter()
                .any(|failure| { failure.url.as_deref() == Some("https://attacker.example/") })
        );
        assert!(failures.iter().any(|failure| {
            failure.failure_reason.as_deref() == Some("net::ERR_CERT_DATE_INVALID")
        }));
    }

    #[test]
    fn network_failure_diagnostics_strip_query_fragment_and_credentials() {
        let telemetry = CdpRuntimeMonitor::from_snapshot_for_test(RuntimeSnapshot {
            network_failures: vec![
                test_outcome(
                    ResourceType::Script,
                    Some("https://user:password@example.test/assets/app.js?token=secret#fragment"),
                    Some("net::ERR_CONNECTION_RESET"),
                    "script",
                    1,
                    None,
                ),
                test_outcome(
                    ResourceType::Image,
                    Some("https://example.test/primary.gif?cache=1"),
                    Some("net::ERR_FAILED"),
                    "primary-image",
                    1,
                    None,
                ),
            ],
            ..RuntimeSnapshot::default()
        });
        let monitor = BrowserEvidenceMonitor::new(telemetry);
        let details = monitor.network_failure_details(1, None);
        assert!(
            details.contains("Script https://example.test/assets/app.js net::ERR_CONNECTION_RESET")
        );
        assert!(!details.contains("user"));
        assert!(!details.contains("password"));
        assert!(!details.contains("token"));
        assert!(!details.contains("secret"));
        assert!(!details.contains("fragment"));

        let exact_primary =
            monitor.network_failure_details(1, Some("https://example.test/primary.gif?cache=1"));
        assert!(!exact_primary.contains("primary.gif"));
        assert!(exact_primary.contains("ERR_CONNECTION_RESET"));
        let other_primary = monitor
            .network_failure_details(1, Some("https://example.test/primary.gif?cache=other"));
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
                request_id: format!("request-{index}"),
                epoch: 1,
                status_code: None,
                is_top_level: false,
            })
            .collect();
        let monitor = BrowserEvidenceMonitor::new(CdpRuntimeMonitor::from_snapshot_for_test(
            RuntimeSnapshot {
                network_failures: failures,
                ..RuntimeSnapshot::default()
            },
        ));
        let details = monitor.network_failure_details(1, None);
        assert_eq!(details.matches("Fetch ").count(), 3);
        assert!(details.contains("ещё сетевых ошибок: 2"));
        assert!(details.contains("не классифицированная сетевая ошибка"));
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
                .contains("светлых пикселей glyph")
        );
        let white = RgbaImage::from_pixel(160, 160, image::Rgba([255, 255, 255, 255]));
        assert!(
            analyze_capture_pixels(&white)
                .unwrap_err()
                .contains("тёмных пикселей плитки")
        );
    }

    #[test]
    fn article_unicode_parser_does_not_accept_a_different_article() {
        let text = "Статья №773\nUnicode: 5143\nЧастотность\n192";
        assert_eq!(article_unicode(text), Some("5143".into()));
        assert_ne!(article_unicode(text).as_deref(), Some("672A"));
        assert_eq!(article_unicode("нет метаданных статьи"), None);
        assert_eq!(article_number(text), Some(773));
        assert_eq!(frequency_index(text), Some(192));
        assert_eq!(article_number("№773\nUnicode: 5143"), Some(773));
        assert_eq!(frequency_index("Частотность\n192 / JLPT 2"), Some(192));
        assert_eq!(frequency_index("Частотность\nJLPT 2"), None);
    }

    #[test]
    fn tls_exception_requires_explicit_flag_exact_failed_url_and_exact_error() {
        let expected_error = "net::ERR_CERT_AUTHORITY_INVALID";
        assert!(
            check_tls_exception(
                true,
                SITE_URL,
                "NET::ERR_CERT_AUTHORITY_INVALID",
                expected_error,
                true,
            )
            .is_ok()
        );
        assert!(
            check_tls_exception(
                false,
                SITE_URL,
                "NET::ERR_CERT_AUTHORITY_INVALID",
                expected_error,
                true,
            )
            .is_err()
        );
        assert!(
            check_tls_exception(
                true,
                "https://attacker.example/",
                "NET::ERR_CERT_AUTHORITY_INVALID",
                expected_error,
                true,
            )
            .is_err()
        );
        assert!(
            check_tls_exception(
                true,
                "https://www.yarxi.su.evil.example/",
                "NET::ERR_CERT_AUTHORITY_INVALID",
                expected_error,
                true,
            )
            .is_err()
        );
        assert!(
            check_tls_exception(
                true,
                "https://www.yarxi.su:444/",
                "NET::ERR_CERT_AUTHORITY_INVALID",
                expected_error,
                true,
            )
            .is_err()
        );
        assert!(
            check_tls_exception(
                true,
                SITE_URL,
                "NET::ERR_CERT_DATE_INVALID",
                "net::ERR_CERT_DATE_INVALID",
                true,
            )
            .is_err()
        );
        assert!(
            check_tls_exception(
                true,
                SITE_URL,
                "NET::ERR_CERT_AUTHORITY_INVALID",
                expected_error,
                false,
            )
            .is_err()
        );
    }
}
