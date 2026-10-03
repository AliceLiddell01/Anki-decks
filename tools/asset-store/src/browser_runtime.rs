//! Повторно используемая среда Chromium и техническая телеметрия CDP.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chromiumoxide::{
    Browser, BrowserConfig, Page,
    cdp::browser_protocol::{
        emulation::{MediaFeature, SetDeviceMetricsOverrideParams, SetEmulatedMediaParams},
        network::{
            EnableParams as NetworkEnableParams, EventLoadingFailed, EventLoadingFinished,
            EventRequestWillBeSent, EventResponseReceived, ResourceType,
        },
        page::{EnableParams, GetFrameTreeParams},
    },
    cdp::js_protocol::runtime::{EnableParams as RuntimeEnableParams, EventExceptionThrown},
};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::time::timeout;
use url::Url;

use crate::temp_workspace::TempWorkspace;

const MAX_TRACKED_REQUESTS: usize = 256;
const MAX_NETWORK_OUTCOMES: usize = 256;
const MAX_NETWORK_DIAGNOSTIC_ITEMS: usize = 3;
const MAX_NETWORK_DIAGNOSTIC_PATH_CHARS: usize = 160;

/// Способ выбора исполняемого файла браузера без сохранения абсолютного пути.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BrowserExecutableSource {
    ChromeBinEnvironment,
    ChromiumBinEnvironment,
    PathLookup,
    PlaywrightCache,
    ChromiumoxideDefault,
}

/// Версия среды браузера; абсолютные пути, профиль и cookie-файлы не записываются.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BrowserRuntimeProvenance {
    pub product: String,
    pub protocol_version: String,
    pub revision: String,
    pub user_agent: String,
    pub js_version: String,
    pub executable_source: BrowserExecutableSource,
}

/// Размер области просмотра и масштаб устройства для CDP `Emulation.setDeviceMetricsOverride`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DeviceMetrics {
    pub width: i64,
    pub height: i64,
    pub device_scale_factor: f64,
    pub mobile: bool,
}

impl DeviceMetrics {
    pub fn new(width: i64, height: i64, device_scale_factor: f64) -> Result<Self, String> {
        let metrics = Self {
            width,
            height,
            device_scale_factor,
            mobile: false,
        };
        metrics.validate()?;
        Ok(metrics)
    }

    pub fn validate(self) -> Result<(), String> {
        if self.width <= 0 || self.height <= 0 {
            return Err(
                "browser_device_metrics_invalid: размеры области просмотра должны быть положительными".into(),
            );
        }
        if !self.device_scale_factor.is_finite() || self.device_scale_factor <= 0.0 {
            return Err(
                "browser_device_metrics_invalid: масштаб устройства должен быть конечным и положительным".into(),
            );
        }
        Ok(())
    }

    fn cdp_params(self) -> SetDeviceMetricsOverrideParams {
        SetDeviceMetricsOverrideParams::new(
            self.width,
            self.height,
            self.device_scale_factor,
            self.mobile,
        )
    }
}

/// Настройки изолированного сеанса Chromium. Переходы по сайтам и правила работы с DOM
/// остаются ответственностью вызывающего провайдера.
#[derive(Debug, Clone)]
pub struct BrowserRuntimeConfig {
    pub device_metrics: Option<DeviceMetrics>,
    pub prefers_color_scheme: Option<String>,
    pub launch_timeout: Duration,
    pub request_timeout: Duration,
}

impl Default for BrowserRuntimeConfig {
    fn default() -> Self {
        Self {
            device_metrics: None,
            prefers_color_scheme: None,
            launch_timeout: Duration::from_secs(30),
            request_timeout: Duration::from_secs(30),
        }
    }
}

impl BrowserRuntimeConfig {
    fn validate(&self) -> Result<(), String> {
        if let Some(metrics) = self.device_metrics {
            metrics.validate()?;
        }
        if self.launch_timeout.is_zero() || self.request_timeout.is_zero() {
            return Err(
                "browser_runtime_timeout_invalid: значения таймаутов должны быть положительными"
                    .into(),
            );
        }
        if self
            .prefers_color_scheme
            .as_deref()
            .is_some_and(|scheme| scheme.trim().is_empty())
        {
            return Err(
                "browser_media_preference_invalid: цветовая схема не должна быть пустой".into(),
            );
        }
        Ok(())
    }
}

/// Найденный исполняемый файл Chromium и безопасная для записи категория его происхождения.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserExecutableSelection {
    path: PathBuf,
    pub source: BrowserExecutableSource,
}

/// Только созданная этим объектом подпапка может быть удалена при закрытии.
struct BrowserProfile {
    path: PathBuf,
    temp_path: PathBuf,
    workspace_path: PathBuf,
    workspace: Option<TempWorkspace>,
    closed: bool,
}

impl BrowserProfile {
    fn standalone() -> io::Result<Self> {
        let workspace = TempWorkspace::create("browser-session")?;
        let parent = workspace.path().to_path_buf();
        Self::create(&parent, Some(workspace))
    }

    fn in_workspace(workspace: &Path) -> io::Result<Self> {
        Self::create(workspace, None)
    }

    fn create(parent: &Path, workspace: Option<TempWorkspace>) -> io::Result<Self> {
        if !fs::symlink_metadata(parent)?.is_dir() {
            return Err(io::Error::other(
                "browser profile parent не является каталогом",
            ));
        }
        let parent = fs::canonicalize(parent)?;
        let mut random = [0_u8; 18];
        for _ in 0..32 {
            File::open("/dev/urandom")?.read_exact(&mut random)?;
            let path = parent.join(format!(
                "browser-profile-{}",
                crate::hashing::encode_lower_hex(&random[..16])
            ));
            match fs::create_dir(&path) {
                Ok(()) => {
                    if let Err(error) = set_private_directory(&path) {
                        let _ = fs::remove_dir(&path);
                        return Err(error);
                    }
                    let temp_path =
                        parent.join(format!("t{:x}{:02x}", random[16] & 0x0f, random[17]));
                    match fs::create_dir(&temp_path) {
                        Ok(()) => {
                            if let Err(error) = set_private_directory(&temp_path) {
                                let _ = fs::remove_dir(&temp_path);
                                let _ = fs::remove_dir(&path);
                                return Err(error);
                            }
                            return Ok(Self {
                                path,
                                temp_path,
                                workspace_path: parent,
                                workspace,
                                closed: false,
                            });
                        }
                        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                            fs::remove_dir(&path)?;
                            continue;
                        }
                        Err(error) => {
                            let _ = fs::remove_dir(&path);
                            return Err(error);
                        }
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::other(
            "не удалось создать уникальный browser profile",
        ))
    }

    fn close(mut self) -> io::Result<()> {
        self.finish_close()
    }

    async fn close_after_browser_stop(mut self) -> io::Result<()> {
        wait_for_workspace_processes(&self.workspace_path).await?;
        self.finish_close()
    }

    fn finish_close(&mut self) -> io::Result<()> {
        self.cleanup()?;
        self.closed = true;
        self.workspace.take().map_or(Ok(()), TempWorkspace::close)
    }

    fn cleanup(&self) -> io::Result<()> {
        TempWorkspace::ensure_no_live_process_references(&self.workspace_path)?;
        let profile = remove_browser_directory(&self.path);
        let temp = remove_browser_directory(&self.temp_path);
        profile.and(temp)
    }
}

fn remove_browser_directory(path: &Path) -> io::Result<()> {
    match fs::remove_dir_all(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        result => result,
    }
}

async fn wait_for_workspace_processes(workspace: &Path) -> io::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let result = TempWorkspace::has_live_process_references(workspace);
        if matches!(result, Ok(false)) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return match result {
                Ok(true) => Err(io::Error::other(
                    "browser process всё ещё использует workspace",
                )),
                Err(error) => Err(error),
                Ok(false) => Ok(()),
            };
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn set_private_directory(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
}

fn build_browser_config(
    profile: &BrowserProfile,
    config: &BrowserRuntimeConfig,
    executable: Option<BrowserExecutableSelection>,
) -> Result<BrowserConfig, String> {
    let mut builder = BrowserConfig::builder()
        .user_data_dir(&profile.path)
        .env("TMPDIR", profile.temp_path.to_string_lossy().into_owned())
        .incognito()
        .respect_https_errors()
        .launch_timeout(config.launch_timeout)
        .request_timeout(config.request_timeout);
    if let Some(executable) = executable {
        builder = builder.chrome_executable(executable.path);
    }
    builder
        .build()
        .map_err(|error| format!("настройка браузера: {error}"))
}

fn profile_setup_failure(profile: BrowserProfile, message: String) -> String {
    match profile.close() {
        Ok(()) => message,
        Err(error) => {
            tracing::error!(stage = "temp_cleanup", code = "browser_profile_cleanup_failed", path_category = "browser_workspace", message = %crate::diagnostics::safe_message(&error.to_string()), "Ошибка уборки после отказа запуска браузера");
            format!("{message}; browser_profile_cleanup_failed: {error}")
        }
    }
}

impl Drop for BrowserProfile {
    fn drop(&mut self) {
        if !self.closed {
            if let Err(error) = self.cleanup() {
                tracing::error!(
                    stage = "temp_cleanup",
                    code = "browser_profile_cleanup_failed",
                    path_category = "browser_workspace",
                    message = %crate::diagnostics::safe_message(&error.to_string()),
                    "Не удалось безопасно удалить профиль и временный каталог браузера"
                );
            }
        }
    }
}

/// Ресурсы создаются до асинхронного setup: отмена setup также закрывает браузер.
struct BrowserResources {
    browser: Option<Browser>,
    handler_task: Option<tokio::task::JoinHandle<()>>,
    profile: Option<BrowserProfile>,
}

async fn stop_browser(browser: &mut Browser) -> Result<(), String> {
    let close = timeout(Duration::from_secs(2), browser.close()).await;
    let wait = timeout(Duration::from_secs(2), browser.wait()).await;
    if matches!(wait, Ok(Ok(_))) {
        return match close {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(error)) => Err(format!("browser_close_failed: {error}")),
            Err(_) => Err("browser_close_timeout".into()),
        };
    }
    match browser.kill().await {
        Some(Ok(())) | None => {
            Err("browser_close_failed: потребовалось принудительное завершение".into())
        }
        Some(Err(error)) => Err(format!("browser_kill_failed: {error}")),
    }
}

impl BrowserResources {
    async fn close(mut self) -> Result<(), String> {
        let browser_result = match self.browser.as_mut() {
            Some(browser) => stop_browser(browser).await,
            None => Ok(()),
        };
        if let Some(handler) = self.handler_task.take() {
            handler.abort();
            let _ = handler.await;
        }
        drop(self.browser.take());
        let cleanup = match self.profile.take() {
            Some(profile) => profile
                .close_after_browser_stop()
                .await
                .map_err(|error| format!("browser_profile_cleanup_failed: {error}")),
            None => Ok(()),
        };
        let result = combine_browser_close_results(browser_result, cleanup);
        if let Err(error) = &result {
            tracing::error!(stage = "temp_cleanup", code = "browser_cleanup_failed", path_category = "browser_workspace", message = %crate::diagnostics::safe_message(error), "Ошибка закрытия браузера или уборки профиля");
        }
        result
    }
}

fn combine_browser_close_results(
    browser_result: Result<(), String>,
    cleanup_result: Result<(), String>,
) -> Result<(), String> {
    match (browser_result, cleanup_result) {
        (Err(browser), Err(cleanup)) => Err(format!("{browser}; {cleanup}")),
        (Err(error), _) | (_, Err(error)) => Err(error),
        _ => Ok(()),
    }
}

impl Drop for BrowserResources {
    fn drop(&mut self) {
        let browser = self.browser.take();
        let handler = self.handler_task.take();
        let profile = self.profile.take();
        if browser.is_none() && handler.is_none() && profile.is_none() {
            return;
        }
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let mut browser = browser;
                if let Some(browser) = browser.as_mut() {
                    let _ = stop_browser(browser).await;
                }
                if let Some(handler) = handler {
                    handler.abort();
                    let _ = handler.await;
                }
                drop(browser);
                drop(profile);
            });
        } else {
            if let Some(handler) = handler {
                handler.abort();
            }
            drop(browser);
            drop(profile);
        }
    }
}

/// Изолированный Chromium-процесс, его начальная страница и CDP-монитор.
pub struct BrowserSession {
    resources: Option<BrowserResources>,
    page: Page,
    provenance: BrowserRuntimeProvenance,
    telemetry: CdpRuntimeMonitor,
}

impl BrowserSession {
    pub async fn launch(config: BrowserRuntimeConfig) -> Result<Self, String> {
        config.validate()?;
        let profile = BrowserProfile::standalone()
            .map_err(|error| format!("browser_profile_create_failed: {error}"))?;
        Self::launch_with_profile(config, profile).await
    }

    /// Профиль принадлежит сеансу; переданный run workspace никогда не удаляется.
    pub async fn launch_in_workspace(
        config: BrowserRuntimeConfig,
        workspace: &Path,
    ) -> Result<Self, String> {
        config.validate()?;
        let profile = BrowserProfile::in_workspace(workspace)
            .map_err(|error| format!("browser_profile_create_failed: {error}"))?;
        Self::launch_with_profile(config, profile).await
    }

    async fn launch_with_profile(
        config: BrowserRuntimeConfig,
        profile: BrowserProfile,
    ) -> Result<Self, String> {
        let executable = find_browser_executable();
        let executable_source = executable
            .as_ref()
            .map(|selection| selection.source)
            .unwrap_or(BrowserExecutableSource::ChromiumoxideDefault);
        let browser_config = match build_browser_config(&profile, &config, executable) {
            Ok(config) => config,
            Err(message) => {
                return Err(profile_setup_failure(profile, message));
            }
        };
        let (browser, mut handler) = match Browser::launch(browser_config).await {
            Ok(launched) => launched,
            Err(error) => {
                return Err(profile_setup_failure(
                    profile,
                    format!("запуск браузера: {error}"),
                ));
            }
        };
        let handler_task = tokio::spawn(async move {
            while let Some(event) = handler.next().await {
                if event.is_err() {
                    break;
                }
            }
        });

        let mut resources = BrowserResources {
            browser: Some(browser),
            handler_task: Some(handler_task),
            profile: Some(profile),
        };
        let setup = async {
            let browser = resources.browser.as_mut().expect("браузер запущен");
            let version = browser
                .version()
                .await
                .map_err(|error| format!("CDP Browser.getVersion: {error}"))?;
            let provenance = BrowserRuntimeProvenance {
                product: version.product,
                protocol_version: version.protocol_version,
                revision: version.revision,
                user_agent: version.user_agent,
                js_version: version.js_version,
                executable_source,
            };
            let page = browser
                .new_page("about:blank")
                .await
                .map_err(|error| format!("создание страницы браузера: {error}"))?;
            page.execute(EnableParams::default())
                .await
                .map_err(|error| format!("CDP Page.enable: {error}"))?;
            if let Some(metrics) = config.device_metrics {
                page.execute(metrics.cdp_params())
                    .await
                    .map_err(|error| format!("CDP Emulation.setDeviceMetricsOverride: {error}"))?;
            }
            if let Some(scheme) = &config.prefers_color_scheme {
                page.execute(
                    SetEmulatedMediaParams::builder()
                        .feature(MediaFeature::new("prefers-color-scheme", scheme.clone()))
                        .build(),
                )
                .await
                .map_err(|error| format!("CDP Emulation.setEmulatedMedia: {error}"))?;
            }
            let telemetry = CdpRuntimeMonitor::start(&page).await?;
            Ok::<_, String>((page, provenance, telemetry))
        }
        .await;

        match setup {
            Ok((page, provenance, telemetry)) => Ok(Self {
                resources: Some(resources),
                page,
                provenance,
                telemetry,
            }),
            Err(error) => match resources.close().await {
                Ok(()) => Err(error),
                Err(cleanup) => Err(format!("{error}; {cleanup}")),
            },
        }
    }

    pub fn page(&self) -> &Page {
        &self.page
    }

    pub fn provenance(&self) -> &BrowserRuntimeProvenance {
        &self.provenance
    }

    pub fn telemetry(&self) -> &CdpRuntimeMonitor {
        &self.telemetry
    }

    pub async fn close(mut self) -> Result<(), String> {
        self.telemetry.abort();
        self.resources
            .take()
            .expect("ресурсы сеанса доступны")
            .close()
            .await
    }
}

impl Drop for BrowserSession {
    fn drop(&mut self) {
        self.telemetry.abort();
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackedRequest {
    pub request_id: String,
    pub resource_type: ResourceType,
    pub url: String,
    pub epoch: u64,
    pub is_top_level: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkOutcome {
    pub resource_type: ResourceType,
    pub url: Option<String>,
    pub failure_reason: Option<String>,
    pub request_id: String,
    pub epoch: u64,
    pub status_code: Option<u16>,
    pub is_top_level: bool,
}

/// Неинтерпретированная провайдером телеметрия CDP одного этапа получения.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RuntimeSnapshot {
    pub pending_requests: Vec<TrackedRequest>,
    pub network_failures: Vec<NetworkOutcome>,
    pub http_errors: Vec<NetworkOutcome>,
    pub javascript_exceptions: u32,
    pub monitor_failed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RetryTrigger {
    ReadinessTimeout,
    ItemTimeout,
    RuntimeFailure,
}

#[derive(Debug, Default)]
struct RuntimeState {
    relevant_requests: HashMap<String, TrackedRequest>,
    network_failures: Vec<NetworkOutcome>,
    http_errors: Vec<NetworkOutcome>,
    javascript_exceptions: HashMap<u64, u32>,
    monitor_failed: bool,
    main_frame_id: String,
    active_epoch: u64,
    next_epoch: u64,
}

impl RuntimeState {
    fn begin_epoch(&mut self) -> u64 {
        self.next_epoch = self.next_epoch.saturating_add(1).max(1);
        self.active_epoch = self.next_epoch;
        self.active_epoch
    }

    fn record_request(
        &mut self,
        request_id: String,
        resource_type: ResourceType,
        url: String,
        frame_id: Option<&str>,
    ) {
        if !is_relevant_resource_type(&resource_type) {
            return;
        }
        if !self.relevant_requests.contains_key(&request_id)
            && self.relevant_requests.len() >= MAX_TRACKED_REQUESTS
        {
            self.monitor_failed = true;
            return;
        }
        let epoch = self
            .relevant_requests
            .get(&request_id)
            .map_or(self.active_epoch, |request| request.epoch);
        let is_top_level = frame_id.is_some_and(|id| id == self.main_frame_id);
        self.relevant_requests.insert(
            request_id.clone(),
            TrackedRequest {
                request_id,
                resource_type,
                url,
                epoch,
                is_top_level,
            },
        );
    }

    fn record_network_failure(
        &mut self,
        request_id: String,
        event_resource_type: ResourceType,
        failure_reason: String,
    ) {
        let tracked = self.relevant_requests.remove(&request_id);
        if tracked.is_none() && !is_relevant_resource_type(&event_resource_type) {
            return;
        }
        if self.network_failures.len() >= MAX_NETWORK_OUTCOMES {
            self.monitor_failed = true;
            return;
        }
        self.network_failures.push(NetworkOutcome {
            resource_type: tracked
                .as_ref()
                .map(|request| request.resource_type.clone())
                .unwrap_or(event_resource_type),
            url: tracked.as_ref().map(|request| request.url.clone()),
            failure_reason: Some(failure_reason),
            request_id,
            epoch: tracked
                .as_ref()
                .map_or(self.active_epoch, |request| request.epoch),
            status_code: None,
            is_top_level: tracked.is_some_and(|request| request.is_top_level),
        });
    }

    fn record_http_error(
        &mut self,
        request_id: String,
        resource_type: ResourceType,
        url: String,
        status_code: u16,
    ) {
        if !is_relevant_resource_type(&resource_type) || status_code < 400 {
            return;
        }
        if self.http_errors.len() >= MAX_NETWORK_OUTCOMES {
            self.monitor_failed = true;
            return;
        }
        let tracked = self.relevant_requests.get(&request_id);
        self.http_errors.push(NetworkOutcome {
            resource_type,
            url: Some(url),
            failure_reason: None,
            request_id,
            epoch: tracked.map_or(self.active_epoch, |request| request.epoch),
            status_code: Some(status_code),
            is_top_level: tracked.is_some_and(|request| request.is_top_level),
        });
    }

    fn record_javascript_exception(&mut self) {
        let current = self
            .javascript_exceptions
            .entry(self.active_epoch)
            .or_default();
        *current = current.saturating_add(1);
        if self.javascript_exceptions.len() > MAX_NETWORK_OUTCOMES {
            self.monitor_failed = true;
        }
    }

    fn snapshot(&self, epoch: u64) -> RuntimeSnapshot {
        RuntimeSnapshot {
            pending_requests: self
                .relevant_requests
                .values()
                .filter(|request| request_in_scope(request.epoch, epoch))
                .cloned()
                .collect(),
            network_failures: self
                .network_failures
                .iter()
                .filter(|failure| request_in_scope(failure.epoch, epoch))
                .cloned()
                .collect(),
            http_errors: self
                .http_errors
                .iter()
                .filter(|failure| request_in_scope(failure.epoch, epoch))
                .cloned()
                .collect(),
            javascript_exceptions: self
                .javascript_exceptions
                .get(&0)
                .copied()
                .unwrap_or(0)
                .saturating_add(self.javascript_exceptions.get(&epoch).copied().unwrap_or(0)),
            monitor_failed: self.monitor_failed,
        }
    }
}

/// Слушает события Network и Runtime CDP; состояние ограничено по размеру, а
/// при потере события всегда сообщает об ошибке мониторинга.
#[derive(Debug, Clone)]
pub struct CdpRuntimeMonitor {
    state: Arc<Mutex<RuntimeState>>,
    tasks: Arc<Vec<tokio::task::JoinHandle<()>>>,
}

impl CdpRuntimeMonitor {
    pub async fn start(page: &Page) -> Result<Self, String> {
        let main_frame_id = page
            .execute(GetFrameTreeParams::default())
            .await
            .map_err(|error| format!("CDP Page.getFrameTree: {error}"))?
            .frame_tree
            .frame
            .id
            .as_ref()
            .to_owned();
        let request_events = page
            .event_listener::<EventRequestWillBeSent>()
            .await
            .map_err(|error| format!("обработчик CDP Network.requestWillBeSent: {error}"))?;
        let finished_events = page
            .event_listener::<EventLoadingFinished>()
            .await
            .map_err(|error| format!("обработчик CDP Network.loadingFinished: {error}"))?;
        let failed_events = page
            .event_listener::<EventLoadingFailed>()
            .await
            .map_err(|error| format!("обработчик CDP Network.loadingFailed: {error}"))?;
        let response_events = page
            .event_listener::<EventResponseReceived>()
            .await
            .map_err(|error| format!("обработчик CDP Network.responseReceived: {error}"))?;
        let exception_events = page
            .event_listener::<EventExceptionThrown>()
            .await
            .map_err(|error| format!("обработчик CDP Runtime.exceptionThrown: {error}"))?;

        page.execute(NetworkEnableParams::default())
            .await
            .map_err(|error| format!("CDP Network.enable: {error}"))?;
        page.execute(RuntimeEnableParams::default())
            .await
            .map_err(|error| format!("CDP Runtime.enable: {error}"))?;

        let state = Arc::new(Mutex::new(RuntimeState {
            main_frame_id,
            ..RuntimeState::default()
        }));
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
                    lock_state(&state).record_request(
                        event.request_id.as_ref().to_owned(),
                        resource_type.clone(),
                        event.request.url.clone(),
                        event.frame_id.as_ref().map(|id| id.as_ref()),
                    );
                }
                mark_monitor_failed(&state);
            }));
        }
        {
            let state = Arc::clone(&state);
            tasks.push(tokio::spawn(async move {
                let mut events = finished_events;
                while let Some(event) = events.next().await {
                    lock_state(&state)
                        .relevant_requests
                        .remove(event.request_id.as_ref());
                }
                mark_monitor_failed(&state);
            }));
        }
        {
            let state = Arc::clone(&state);
            tasks.push(tokio::spawn(async move {
                let mut events = failed_events;
                while let Some(event) = events.next().await {
                    lock_state(&state).record_network_failure(
                        event.request_id.as_ref().to_owned(),
                        event.r#type.clone(),
                        event.error_text.clone(),
                    );
                }
                mark_monitor_failed(&state);
            }));
        }
        {
            let state = Arc::clone(&state);
            tasks.push(tokio::spawn(async move {
                let mut events = response_events;
                while let Some(event) = events.next().await {
                    lock_state(&state).record_http_error(
                        event.request_id.as_ref().to_owned(),
                        event.r#type.clone(),
                        event.response.url.clone(),
                        event.response.status as u16,
                    );
                }
                mark_monitor_failed(&state);
            }));
        }
        {
            let state = Arc::clone(&state);
            tasks.push(tokio::spawn(async move {
                let mut events = exception_events;
                while events.next().await.is_some() {
                    lock_state(&state).record_javascript_exception();
                }
                mark_monitor_failed(&state);
            }));
        }

        Ok(Self {
            state,
            tasks: Arc::new(tasks),
        })
    }

    pub fn begin_epoch(&self) -> u64 {
        lock_state(&self.state).begin_epoch()
    }

    pub fn snapshot(&self, epoch: u64) -> RuntimeSnapshot {
        lock_state(&self.state).snapshot(epoch)
    }

    pub fn monitor_failed(&self) -> bool {
        lock_state(&self.state).monitor_failed
    }

    pub(crate) fn retryable_acquisition(&self, epoch: u64, trigger: RetryTrigger) -> bool {
        let state = lock_state(&self.state);
        if state.monitor_failed {
            return false;
        }
        if trigger == RetryTrigger::ReadinessTimeout {
            return true;
        }

        let bootstrap_failure = state
            .network_failures
            .iter()
            .any(|failure| failure.epoch == 0)
            || state.http_errors.iter().any(|failure| failure.epoch == 0)
            || state.javascript_exceptions.get(&0).copied().unwrap_or(0) > 0;
        let has_epoch_network_failure = state
            .network_failures
            .iter()
            .any(|failure| failure.epoch == epoch);
        let has_epoch_http_error = state
            .http_errors
            .iter()
            .any(|failure| failure.epoch == epoch);
        let epoch_network_failures_are_retryable = state
            .network_failures
            .iter()
            .filter(|failure| failure.epoch == epoch)
            .all(|failure| {
                failure
                    .failure_reason
                    .as_deref()
                    .is_some_and(is_retryable_network_error)
            });
        let epoch_http_errors_are_retryable = state
            .http_errors
            .iter()
            .filter(|failure| failure.epoch == epoch)
            .all(|failure| failure.status_code.is_some_and(is_retryable_http_status));
        let has_epoch_failure = has_epoch_network_failure || has_epoch_http_error;

        !bootstrap_failure
            && state
                .javascript_exceptions
                .get(&epoch)
                .copied()
                .unwrap_or(0)
                == 0
            && epoch_network_failures_are_retryable
            && epoch_http_errors_are_retryable
            && (trigger == RetryTrigger::ItemTimeout || has_epoch_failure)
    }

    /// Удаляет только запись с совпадающими CDP request id, URL и причиной.
    /// Provider сам проверяет, что такое исключение разрешено его политикой.
    pub fn clear_exact_network_failure(
        &self,
        request_id: &str,
        url: &str,
        failure_reason: &str,
    ) -> bool {
        let mut state = lock_state(&self.state);
        let Some(index) = state.network_failures.iter().position(|failure| {
            failure.request_id == request_id
                && failure.url.as_deref() == Some(url)
                && failure
                    .failure_reason
                    .as_deref()
                    .is_some_and(|reason| reason.eq_ignore_ascii_case(failure_reason))
        }) else {
            return false;
        };
        state.network_failures.remove(index);
        true
    }

    pub fn abort(&self) {
        for task in self.tasks.iter() {
            task.abort();
        }
    }

    #[cfg(test)]
    pub(crate) fn from_snapshot_for_test(snapshot: RuntimeSnapshot) -> Self {
        let mut javascript_exceptions = HashMap::new();
        if snapshot.javascript_exceptions > 0 {
            javascript_exceptions.insert(0, snapshot.javascript_exceptions);
        }
        Self {
            state: Arc::new(Mutex::new(RuntimeState {
                relevant_requests: snapshot
                    .pending_requests
                    .into_iter()
                    .map(|request| (request.request_id.clone(), request))
                    .collect(),
                network_failures: snapshot.network_failures,
                http_errors: snapshot.http_errors,
                javascript_exceptions,
                monitor_failed: snapshot.monitor_failed,
                ..RuntimeState::default()
            })),
            tasks: Arc::new(Vec::new()),
        }
    }
}

pub fn is_relevant_resource_type(resource_type: &ResourceType) -> bool {
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

pub fn is_retryable_http_status(status: u16) -> bool {
    status == 429 || (500..=599).contains(&status)
}

pub fn is_retryable_network_error(reason: &str) -> bool {
    matches!(
        reason.trim().to_ascii_uppercase().as_str(),
        "NET::ERR_TIMED_OUT"
            | "ERR_TIMED_OUT"
            | "NET::ERR_CONNECTION_RESET"
            | "ERR_CONNECTION_RESET"
            | "NET::ERR_CONNECTION_CLOSED"
            | "ERR_CONNECTION_CLOSED"
            | "NET::ERR_CONNECTION_REFUSED"
            | "ERR_CONNECTION_REFUSED"
            | "NET::ERR_NETWORK_CHANGED"
            | "ERR_NETWORK_CHANGED"
            | "NET::ERR_ABORTED"
            | "ERR_ABORTED"
    )
}

pub fn format_network_failure_details(failures: &[NetworkOutcome]) -> String {
    if failures.is_empty() {
        return "нет".into();
    }
    let mut details = failures
        .iter()
        .take(MAX_NETWORK_DIAGNOSTIC_ITEMS)
        .map(|failure| {
            let location = failure
                .url
                .as_deref()
                .map(sanitized_network_location)
                .unwrap_or_else(|| "<URL недоступен>".into());
            let reason = failure
                .failure_reason
                .as_deref()
                .map(sanitized_network_failure_reason)
                .unwrap_or_else(|| "не классифицированная сетевая ошибка".into());
            format!("{:?} {location} {reason}", failure.resource_type)
        })
        .collect::<Vec<_>>();
    if failures.len() > MAX_NETWORK_DIAGNOSTIC_ITEMS {
        details.push(format!(
            "ещё сетевых ошибок: {}",
            failures.len() - MAX_NETWORK_DIAGNOSTIC_ITEMS
        ));
    }
    details.join("; ")
}

fn request_in_scope(request_epoch: u64, requested_epoch: u64) -> bool {
    request_epoch == 0 || request_epoch == requested_epoch
}

fn lock_state(state: &Mutex<RuntimeState>) -> std::sync::MutexGuard<'_, RuntimeState> {
    state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn mark_monitor_failed(state: &Mutex<RuntimeState>) {
    lock_state(state).monitor_failed = true;
}

pub fn find_browser_executable() -> Option<BrowserExecutableSelection> {
    for (variable, source) in [
        ("CHROME_BIN", BrowserExecutableSource::ChromeBinEnvironment),
        (
            "CHROMIUM_BIN",
            BrowserExecutableSource::ChromiumBinEnvironment,
        ),
    ] {
        if let Some(path) = std::env::var_os(variable).map(PathBuf::from)
            && is_executable_file(&path)
        {
            return Some(BrowserExecutableSelection { path, source });
        }
    }
    if let Some(path_entries) = std::env::var_os("PATH") {
        for directory in std::env::split_paths(&path_entries) {
            for name in ["chromium", "chromium-browser", "google-chrome", "chrome"] {
                let path = directory.join(name);
                if is_executable_file(&path) {
                    return Some(BrowserExecutableSelection {
                        path,
                        source: BrowserExecutableSource::PathLookup,
                    });
                }
            }
        }
    }
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    let cache = home.join(".cache/ms-playwright");
    let mut candidates = Vec::new();
    let entries = std::fs::read_dir(cache).ok()?;
    for entry in entries.flatten() {
        if !entry.file_name().to_string_lossy().starts_with("chromium-") {
            continue;
        }
        candidates.push(entry.path().join("chrome-linux64/chrome"));
        candidates.push(entry.path().join("chrome-linux/chrome"));
    }
    candidates.sort();
    candidates
        .into_iter()
        .find(|path| is_executable_file(path))
        .map(|path| BrowserExecutableSelection {
            path,
            source: BrowserExecutableSource::PlaywrightCache,
        })
}

fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

fn sanitized_network_location(raw_url: &str) -> String {
    let Ok(url) = Url::parse(raw_url) else {
        return "<URL не удалось разобрать>".into();
    };
    if !matches!(url.scheme(), "http" | "https") || !url.origin().is_tuple() {
        return format!("<{} ресурс>", url.scheme());
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
        return "не классифицированная сетевая ошибка".into();
    };
    if suffix.is_empty()
        || !suffix
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
    {
        return "не классифицированная сетевая ошибка".into();
    }
    format!("net::ERR_{suffix}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::ffi::OsStrExt;

    #[test]
    fn borrowed_profile_drop_removes_only_its_child() {
        let parent = TempWorkspace::create("borrowed-browser-profile-test").unwrap();
        let sentinel = parent.path().join(".runtime");
        fs::create_dir(&sentinel).unwrap();
        fs::write(sentinel.join("keep"), "sentinel").unwrap();
        let path;
        let temp_path;
        {
            let profile = BrowserProfile::in_workspace(parent.path()).unwrap();
            path = profile.path.clone();
            temp_path = profile.temp_path.clone();
            fs::write(path.join("data"), "browser").unwrap();
            fs::write(temp_path.join("data"), "browser temp").unwrap();
        }
        assert!(!path.exists());
        assert!(!temp_path.exists());
        assert_eq!(
            fs::read_to_string(sentinel.join("keep")).unwrap(),
            "sentinel"
        );
        parent.close().unwrap();
    }

    #[test]
    fn standalone_profile_close_removes_its_owned_workspace() {
        let profile = BrowserProfile::standalone().unwrap();
        let root = profile.workspace.as_ref().unwrap().path().to_path_buf();
        fs::write(profile.path.join("data"), "browser").unwrap();
        fs::write(profile.temp_path.join("data"), "browser temp").unwrap();
        let profile_path = profile.path.clone();
        let temp_path = profile.temp_path.clone();
        profile.close().unwrap();
        assert!(!root.exists());
        assert!(!profile_path.exists());
        assert!(!temp_path.exists());
    }

    #[test]
    fn standalone_profile_is_cleaned_after_early_error() {
        fn failing_setup(observed_root: &mut PathBuf) -> io::Result<()> {
            let profile = BrowserProfile::standalone()?;
            *observed_root = profile.workspace.as_ref().unwrap().path().to_path_buf();
            fs::write(profile.path.join("data"), "browser")?;
            fs::write(profile.temp_path.join("data"), "browser temp")?;
            Err(io::Error::other("injected setup failure"))
        }
        let mut root = PathBuf::new();
        assert!(failing_setup(&mut root).is_err());
        assert!(!root.exists());
    }

    #[test]
    fn cleanup_runs_and_removes_browser_tree_after_kill_fallback_error() {
        let parent = TempWorkspace::create("browser-kill-fallback-cleanup-test").unwrap();
        let profile = BrowserProfile::in_workspace(parent.path()).unwrap();
        let profile_path = profile.path.clone();
        let temp_path = profile.temp_path.clone();
        fs::write(profile_path.join("profile-data"), b"profile").unwrap();
        fs::write(temp_path.join("temp-data"), b"temp").unwrap();

        let stop = Err("browser_close_failed: потребовалось принудительное завершение".into());
        let cleanup = profile
            .close()
            .map_err(|error| format!("browser_profile_cleanup_failed: {error}"));
        let result = combine_browser_close_results(stop, cleanup);
        assert!(result.is_err());
        assert!(!profile_path.exists());
        assert!(!temp_path.exists());
        parent.close().unwrap();
    }

    #[test]
    fn profile_cleanup_failure_is_returned_without_deleting_borrowed_parent() {
        let parent = TempWorkspace::create("browser-profile-error-test").unwrap();
        let profile = BrowserProfile::in_workspace(parent.path()).unwrap();
        fs::remove_dir(&profile.path).unwrap();
        fs::write(&profile.path, "injected filesystem error").unwrap();
        assert!(profile.close().is_err());
        assert!(parent.path().is_dir());
        parent.close().unwrap();
    }

    #[test]
    fn simultaneous_profiles_under_one_parent_do_not_conflict() {
        let parent = TempWorkspace::create("browser-profile-parallel-test").unwrap();
        std::thread::scope(|scope| {
            let first = scope.spawn(|| BrowserProfile::in_workspace(parent.path()).unwrap());
            let second = scope.spawn(|| BrowserProfile::in_workspace(parent.path()).unwrap());
            let first = first.join().unwrap();
            let second = second.join().unwrap();
            assert_ne!(first.path, second.path);
            assert_ne!(first.temp_path, second.temp_path);
            assert_eq!(first.path.parent(), first.temp_path.parent());
            assert_eq!(second.path.parent(), second.temp_path.parent());
            for profile in [&first, &second] {
                let socket_path = profile
                    .temp_path
                    .join("org.chromium.Chromium.XXXXXX")
                    .join("SingletonSocket");
                assert!(socket_path.as_os_str().as_bytes().len() < 108);
            }
            first.close().unwrap();
            second.close().unwrap();
        });
        parent.close().unwrap();
    }

    #[test]
    fn browser_config_scopes_tmpdir_to_profile_owner_without_global_change() {
        let parent = TempWorkspace::create("browser-scoped-tmpdir-test").unwrap();
        let profile = BrowserProfile::in_workspace(parent.path()).unwrap();
        let before = std::env::var_os("TMPDIR");
        let executable = BrowserExecutableSelection {
            path: PathBuf::from("/bin/true"),
            source: BrowserExecutableSource::PathLookup,
        };
        let config =
            build_browser_config(&profile, &BrowserRuntimeConfig::default(), Some(executable))
                .unwrap();
        assert_eq!(
            config.user_data_dir.as_deref(),
            Some(profile.path.as_path())
        );
        assert_eq!(
            config
                .process_envs
                .as_ref()
                .and_then(|environment| environment.get("TMPDIR"))
                .map(String::as_str),
            profile.temp_path.to_str()
        );
        assert!(profile.temp_path.starts_with(parent.path()));
        assert_eq!(std::env::var_os("TMPDIR"), before);
        assert!(profile.temp_path.as_os_str().as_bytes().len() < 108);
        profile.close().unwrap();
        parent.close().unwrap();
    }

    #[tokio::test]
    async fn invalid_browser_setup_creates_no_profile_in_borrowed_workspace() {
        let parent = TempWorkspace::create("browser-invalid-setup-test").unwrap();
        let before = fs::read_dir(parent.path()).unwrap().count();
        let config = BrowserRuntimeConfig {
            launch_timeout: Duration::ZERO,
            ..Default::default()
        };
        assert!(
            BrowserSession::launch_in_workspace(config, parent.path())
                .await
                .is_err()
        );
        assert_eq!(fs::read_dir(parent.path()).unwrap().count(), before);
        parent.close().unwrap();
    }

    fn outcome(
        resource_type: ResourceType,
        url: Option<&str>,
        reason: Option<&str>,
        request_id: &str,
        epoch: u64,
        status: Option<u16>,
    ) -> NetworkOutcome {
        NetworkOutcome {
            resource_type,
            url: url.map(str::to_owned),
            failure_reason: reason.map(str::to_owned),
            request_id: request_id.to_owned(),
            epoch,
            status_code: status,
            is_top_level: false,
        }
    }

    #[test]
    fn provenance_serialization_keeps_runtime_and_selection_without_local_paths() {
        let provenance = BrowserRuntimeProvenance {
            product: "Chrome/140.0.0.0".into(),
            protocol_version: "1.3".into(),
            revision: "abc123".into(),
            user_agent: "Mozilla/5.0 Chrome/140".into(),
            js_version: "14.0".into(),
            executable_source: BrowserExecutableSource::PathLookup,
        };
        let encoded = serde_json::to_value(&provenance).unwrap();
        assert_eq!(encoded["executable_source"], "path_lookup");
        assert_eq!(encoded["product"], "Chrome/140.0.0.0");
        assert_eq!(encoded.as_object().unwrap().len(), 6);
        let text = encoded.to_string();
        assert!(!text.contains("/home/"));
        assert!(!text.contains("C:\\"));
    }

    #[test]
    fn device_metrics_accept_scale_three_without_baking_in_provider_scale() {
        let metrics = DeviceMetrics::new(1280, 900, 3.0).unwrap();
        let config = BrowserRuntimeConfig {
            device_metrics: Some(metrics),
            ..BrowserRuntimeConfig::default()
        };
        config.validate().unwrap();
        assert_eq!(config.device_metrics.unwrap().device_scale_factor, 3.0);
        let params = serde_json::to_value(metrics.cdp_params()).unwrap();
        assert_eq!(params["deviceScaleFactor"].as_f64(), Some(3.0));
        assert!(DeviceMetrics::new(0, 900, 3.0).is_err());
        assert!(DeviceMetrics::new(1280, 900, f64::NAN).is_err());
    }

    #[test]
    fn raw_snapshot_tracks_pending_failures_http_javascript_and_epoch_scope() {
        let mut state = RuntimeState {
            main_frame_id: "main".into(),
            ..RuntimeState::default()
        };
        let epoch = state.begin_epoch();
        state.record_request(
            "pending".into(),
            ResourceType::Fetch,
            "https://example.test/pending".into(),
            Some("main"),
        );
        state.record_request(
            "failed".into(),
            ResourceType::Script,
            "https://example.test/script.js".into(),
            Some("main"),
        );
        state.record_network_failure(
            "failed".into(),
            ResourceType::Script,
            "net::ERR_CONNECTION_RESET".into(),
        );
        state.record_http_error(
            "pending".into(),
            ResourceType::Fetch,
            "https://example.test/pending".into(),
            503,
        );
        state.record_javascript_exception();

        let snapshot = state.snapshot(epoch);
        assert_eq!(snapshot.pending_requests.len(), 1);
        assert_eq!(snapshot.pending_requests[0].request_id, "pending");
        assert!(snapshot.pending_requests[0].is_top_level);
        assert_eq!(snapshot.network_failures.len(), 1);
        assert_eq!(snapshot.http_errors[0].status_code, Some(503));
        assert_eq!(snapshot.javascript_exceptions, 1);
        assert!(state.snapshot(epoch + 1).pending_requests.is_empty());
        assert_eq!(state.snapshot(epoch + 1).javascript_exceptions, 0);
    }

    #[test]
    fn telemetry_overflow_fails_closed() {
        let mut state = RuntimeState::default();
        for index in 0..=MAX_TRACKED_REQUESTS {
            state.record_request(
                format!("request-{index}"),
                ResourceType::Fetch,
                format!("https://example.test/{index}"),
                None,
            );
        }
        assert!(state.monitor_failed);
        assert_eq!(state.relevant_requests.len(), MAX_TRACKED_REQUESTS);

        for index in 0..=MAX_NETWORK_OUTCOMES {
            state.record_network_failure(
                format!("failed-{index}"),
                ResourceType::Fetch,
                "net::ERR_FAILED".into(),
            );
        }
        assert!(state.monitor_failed);
        assert_eq!(state.network_failures.len(), MAX_NETWORK_OUTCOMES);
    }

    #[test]
    fn retry_classification_rejects_javascript_and_permanent_failures() {
        assert!(is_retryable_network_error("net::ERR_TIMED_OUT"));
        assert!(is_retryable_network_error("net::ERR_ABORTED"));
        assert!(!is_retryable_network_error("net::ERR_CERT_DATE_INVALID"));
        assert!(!is_retryable_network_error("details: ERR_CONNECTION_RESET"));
        assert!(is_retryable_http_status(429));
        assert!(is_retryable_http_status(503));
        assert!(!is_retryable_http_status(404));

        let mut state = RuntimeState::default();
        let epoch = state.begin_epoch();
        state.network_failures.push(outcome(
            ResourceType::Fetch,
            Some("https://example.test/retry"),
            Some("net::ERR_CONNECTION_RESET"),
            "retryable",
            epoch,
            None,
        ));
        let monitor = CdpRuntimeMonitor {
            state: Arc::new(Mutex::new(state)),
            tasks: Arc::new(Vec::new()),
        };
        assert!(monitor.retryable_acquisition(epoch, RetryTrigger::RuntimeFailure));
        assert!(monitor.retryable_acquisition(epoch, RetryTrigger::ItemTimeout));
        assert!(monitor.retryable_acquisition(epoch, RetryTrigger::ReadinessTimeout));

        let mut state = lock_state(&monitor.state);
        state.record_javascript_exception();
        drop(state);
        assert!(!monitor.retryable_acquisition(epoch, RetryTrigger::ItemTimeout));
        assert!(monitor.retryable_acquisition(epoch, RetryTrigger::ReadinessTimeout));

        lock_state(&monitor.state).http_errors.push(outcome(
            ResourceType::Fetch,
            Some("https://example.test/permanent"),
            None,
            "permanent",
            epoch,
            Some(404),
        ));
        assert!(!monitor.retryable_acquisition(epoch, RetryTrigger::RuntimeFailure));
        assert!(!monitor.retryable_acquisition(epoch, RetryTrigger::ItemTimeout));
    }

    #[test]
    fn retry_classification_rejects_bootstrap_failure_except_readiness_timeout() {
        let mut state = RuntimeState::default();
        let epoch = state.begin_epoch();
        state.network_failures.push(outcome(
            ResourceType::Fetch,
            Some("https://example.test/bootstrap"),
            Some("net::ERR_CONNECTION_RESET"),
            "bootstrap",
            0,
            None,
        ));
        let monitor = CdpRuntimeMonitor {
            state: Arc::new(Mutex::new(state)),
            tasks: Arc::new(Vec::new()),
        };

        assert!(!monitor.retryable_acquisition(epoch, RetryTrigger::ItemTimeout));
        assert!(!monitor.retryable_acquisition(epoch, RetryTrigger::RuntimeFailure));
        assert!(monitor.retryable_acquisition(epoch, RetryTrigger::ReadinessTimeout));
    }

    #[test]
    fn retry_classification_preserves_timeout_without_failures_and_rejects_failed_monitor() {
        let mut state = RuntimeState::default();
        let epoch = state.begin_epoch();
        let monitor = CdpRuntimeMonitor {
            state: Arc::new(Mutex::new(state)),
            tasks: Arc::new(Vec::new()),
        };
        assert!(monitor.retryable_acquisition(epoch, RetryTrigger::ItemTimeout));
        assert!(!monitor.retryable_acquisition(epoch, RetryTrigger::RuntimeFailure));
        assert!(monitor.retryable_acquisition(epoch, RetryTrigger::ReadinessTimeout));

        lock_state(&monitor.state).monitor_failed = true;
        for trigger in [
            RetryTrigger::ItemTimeout,
            RetryTrigger::RuntimeFailure,
            RetryTrigger::ReadinessTimeout,
        ] {
            assert!(!monitor.retryable_acquisition(epoch, trigger));
        }
    }

    #[test]
    fn network_diagnostics_redact_credentials_query_fragment_and_unknown_error_text() {
        let details = format_network_failure_details(&[
            outcome(
                ResourceType::Script,
                Some("https://user:password@example.test/a.js?token=secret#fragment"),
                Some("net::ERR_CONNECTION_RESET"),
                "one",
                1,
                None,
            ),
            outcome(
                ResourceType::Fetch,
                Some("https://example.test/private?secret=hidden"),
                Some("connection details include secret"),
                "two",
                1,
                None,
            ),
        ]);
        assert!(details.contains("https://example.test/a.js net::ERR_CONNECTION_RESET"));
        assert!(details.contains("не классифицированная сетевая ошибка"));
        for secret in ["user", "password", "token", "secret", "hidden", "fragment"] {
            assert!(!details.contains(secret));
        }
    }
}
