//! Предметные правила и семантическая проверка pitch-accent PNG с отказом при неполных данных.

use std::io::{Cursor, Read};

use image::GenericImageView;
use image::ImageDecoder;
use serde::{Deserialize, Serialize};
use serde_json::json;
use url::Url;

use crate::browser_runtime::BrowserRuntimeProvenance;
use crate::domain::{
    AssetDomainPolicy, CanonicalAssetLocation, TrustSemantics, extension_for_format,
    validate_safe_consumer_filename,
};
use crate::error::{AssetError, ErrorCode};
use crate::hashing::sha256_hex;
use crate::model::{
    AssetRecord, DetectedFormat, SemanticDecision, SemanticStatus, ValidationEvidence,
    ValidatorIdentity,
};
use crate::validation::{SemanticValidator, ValidatorFailure};

/// Максимальный размер сжатого PNG одного браузерного снимка графика акцента.
/// Лимит оставляет запас для PNG-снимка элемента при масштабе 3×, сохраняя
/// конечную границу для недоверенного входного потока.
pub(crate) const PITCH_ACCENT_MAX_ASSET_BYTES: u64 = 8 * 1024 * 1024;

/// Допуск размеров PNG из-за округления границ нативной области снимка CDP.
/// При DSF 3.0 он ограничивает расхождение одним CSS-пикселем.
pub(crate) const PITCH_ACCENT_CAPTURE_PIXEL_ROUNDING_TOLERANCE: f64 = 3.0;

/// Геометрический допуск для согласования DOM, области снимка и сохранённого свидетельства.
pub(crate) const PITCH_ACCENT_CAPTURE_GEOMETRY_TOLERANCE_CSS_PX: f64 = 0.25;

/// Сравнивает чтение JPDB, считая хирагану и катакану эквивалентной записью.
/// Остальные символы сравниваются буквально; написание не нормализуется.
pub fn jpdb_readings_equivalent(left: &str, right: &str) -> bool {
    fn hiragana_equivalent(character: char) -> char {
        let code_point = u32::from(character);
        if matches!(code_point, 0x30A1..=0x30F6 | 0x30FD..=0x30FE) {
            char::from_u32(code_point - 0x60).unwrap_or(character)
        } else {
            character
        }
    }

    left.chars()
        .map(hiragana_equivalent)
        .eq(right.chars().map(hiragana_equivalent))
}

pub(crate) fn capture_pixel_dimensions_match(
    pixel_width: u32,
    pixel_height: u32,
    clip_width_css_px: f64,
    clip_height_css_px: f64,
    device_scale_factor: f64,
) -> bool {
    let expected_width = clip_width_css_px * device_scale_factor;
    let expected_height = clip_height_css_px * device_scale_factor;
    expected_width.is_finite()
        && expected_height.is_finite()
        && (f64::from(pixel_width) - expected_width).abs()
            <= PITCH_ACCENT_CAPTURE_PIXEL_ROUNDING_TOLERANCE
        && (f64::from(pixel_height) - expected_height).abs()
            <= PITCH_ACCENT_CAPTURE_PIXEL_ROUNDING_TOLERANCE
}

/// Верхняя граница ширины и высоты PNG pitch-accent.
pub const PITCH_ACCENT_MAX_IMAGE_DIMENSION: u32 = 4096;

/// Предельный объём памяти для декодирования PNG pitch-accent.
pub const PITCH_ACCENT_MAX_DECODE_ALLOCATION_BYTES: u64 = 48 * 1024 * 1024;

const PITCH_ACCENT_BACKGROUND_RGB_TOLERANCE: u8 = 8;
const PITCH_ACCENT_GRAPH_PIXEL_MIN_RGB_DIFFERENCE: u8 = 16;
const PITCH_ACCENT_MIN_CONTRASTING_GRAPH_PIXELS: usize = 8;

/// Независимая политика публикации канонического PNG pitch-accent.
#[derive(Debug, Clone, Copy, Default)]
pub struct PitchAccentDomainPolicy;

impl AssetDomainPolicy for PitchAccentDomainPolicy {
    fn domain_id(&self) -> &'static str {
        "pitch_accent"
    }

    fn validate_identity(&self, identity: &crate::model::AssetIdentity) -> Result<(), AssetError> {
        identity
            .validate()
            .map_err(|message| AssetError::new(ErrorCode::InvalidIdentity, message))?;
        if identity.namespace != "pitch_accent" {
            return Err(AssetError::new(
                ErrorCode::InvalidIdentity,
                "домен pitch-accent принимает только пространство имён `pitch_accent`",
            ));
        }
        let filename = format!("{}.pitch.png", identity.key);
        validate_safe_consumer_filename(&filename, DetectedFormat::Png)?;
        Ok(())
    }

    fn canonical_location(
        &self,
        identity: &crate::model::AssetIdentity,
        sha256: &str,
        format: DetectedFormat,
    ) -> Result<CanonicalAssetLocation, AssetError> {
        self.validate_identity(identity)?;
        validate_hash(sha256)?;
        let extension = extension_for_format(format);
        let (category, storage_filename) = if self.is_publishable_format(format) {
            ("png", format!("{}.png", identity.key))
        } else {
            let identity_hash = sha256_hex(identity.key.as_bytes());
            (
                "unsupported",
                format!("{identity_hash}-{sha256}.{extension}"),
            )
        };
        let consumer_filename = if format == DetectedFormat::Png {
            format!("{}.pitch.png", identity.key)
        } else {
            format!("{}.{extension}", identity.key)
        };
        validate_safe_consumer_filename(&consumer_filename, format)?;
        Ok(CanonicalAssetLocation {
            storage_path: format!("assets/{category}/{storage_filename}"),
            consumer_filename,
        })
    }

    fn proves_legacy_consumer_filename(
        &self,
        identity: &crate::model::AssetIdentity,
        filename: &str,
    ) -> bool {
        self.validate_identity(identity).is_ok()
            && filename == format!("{}.png", identity.key)
            && validate_safe_consumer_filename(filename, DetectedFormat::Png).is_ok()
    }

    fn legacy_location(
        &self,
        _identity: &crate::model::AssetIdentity,
        _sha256: &str,
        _format: DetectedFormat,
    ) -> Option<CanonicalAssetLocation> {
        // Домен `pitch_accent` отсутствовал в схемах 3/4.
        None
    }

    fn is_publishable_format(&self, format: DetectedFormat) -> bool {
        format == DetectedFormat::Png
    }

    fn allows_verified_format(&self, format: DetectedFormat) -> bool {
        format == DetectedFormat::Png
    }

    fn max_asset_bytes(&self) -> Option<u64> {
        Some(PITCH_ACCENT_MAX_ASSET_BYTES)
    }

    fn content_addressed_storage(&self) -> bool {
        false
    }

    /// Доверие pitch-ресурсу даёт только текущее автоматическое решение
    /// ожидаемого валидатора: подтверждение человека само по себе не делает
    /// отрисовку пригодной для карточки.
    fn trust_semantics(&self) -> TrustSemantics {
        TrustSemantics::AUTOMATED_VERIFIED_ONLY
    }
}

/// Метаданные словарной записи JPDB, не участвующие в имени канонического файла.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PitchAccentDomainMetadata {
    /// Точное значение `surface`; оно должно совпадать с `AssetIdentity.key`.
    pub surface: String,
    /// Значение `reading` для карточки и сведений об источнике; оно не входит в имя файла.
    pub reading: String,
    /// Положительный идентификатор словарной записи JPDB.
    pub jpdb_vocabulary_id: u64,
    /// Структурные сведения об источнике, захвате и среде браузера.
    pub evidence: PitchAccentEvidence,
}

/// Одна реально наблюдённая JPDB пара написания и чтения.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PitchAccentResolvedForm {
    pub surface: String,
    pub reading: String,
}

/// Минимальные структурные свидетельства, достаточные для автоматической проверки.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PitchAccentEvidence {
    pub provider: PitchAccentProvider,
    pub source_url: String,
    /// Связанные пары написания и чтения, наблюдённые в словарной записи JPDB.
    pub resolved_forms: Vec<PitchAccentResolvedForm>,
    /// Число отдельных графиков, обнаруженных на странице источника.
    pub graph_count: u32,
    pub render: PitchAccentRenderEvidence,
    pub browser: BrowserRuntimeProvenance,
}

/// Источник получения изображения для этого домена.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PitchAccentProvider {
    Jpdb,
}

/// Способ получения канонического PNG.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PitchAccentRenderKind {
    ElementScreenshot,
    SvgElementScreenshot,
    CanvasElementScreenshot,
    BrowserRegionScreenshot,
}

/// Один узел графика на странице словарной записи, попавший в снимок браузера.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PitchAccentGraphEvidence {
    /// Нулевой индекс в порядке графиков страницы.
    pub index: u32,
    /// CSS-селектор конкретного узла графика.
    pub selector: String,
    /// Прямоугольник узла графика относительно области просмотра.
    pub viewport_rect: PitchAccentCaptureRect,
    /// Прямоугольник узла графика относительно начала документа.
    pub document_rect: PitchAccentCaptureRect,
}

/// Состояние документа и цветовая схема браузера, наблюдённые перед снимком.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PitchAccentDarkThemeProof {
    /// Токены `classList` элемента `document.documentElement`.
    pub document_element_classes: Vec<String>,
    /// Наблюдаемый результат `matchMedia('(prefers-color-scheme: dark)').matches`.
    pub prefers_color_scheme: String,
    /// Наблюдаемое `getComputedStyle(document.documentElement).colorScheme`.
    pub computed_color_scheme: String,
    /// CSS-селектор реально наблюдённого сплошного фона области pitch accent или её предка.
    pub background_selector: String,
    /// RGB вычисленного фона; один пиксель должен совпасть с допуском ±8 на каждый канал.
    pub background_rgb: [u8; 3],
}

/// Прямоугольник в CSS-пикселях; систему координат задаёт поле, в котором он хранится.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PitchAccentCaptureRect {
    /// Координата X в CSS-пикселях.
    pub x: f64,
    /// Координата Y в CSS-пикселях.
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// Координатная система снимка, зафиксированная в свидетельствах.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PitchAccentCoordinateSpace {
    /// Координаты относительно начала документа, а не текущей области просмотра.
    Document,
}

/// Фактические параметры отрисованной области.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PitchAccentRenderEvidence {
    pub kind: PitchAccentRenderKind,
    /// Селектор общей области, снятой одним снимком браузера.
    pub selector: String,
    /// Наблюдённые узлы графиков, попавшие в снимок, в порядке DOM.
    pub graphs: Vec<PitchAccentGraphEvidence>,
    /// Система координат объединения графиков и итоговой области снимка.
    pub coordinate_space: PitchAccentCoordinateSpace,
    pub viewport_width: u32,
    pub viewport_height: u32,
    pub document_width: u32,
    pub document_height: u32,
    /// Наблюдённые `window.scrollX` и `window.scrollY` в CSS-пикселях.
    pub scroll_x: f64,
    pub scroll_y: f64,
    pub pixel_width: u32,
    pub pixel_height: u32,
    pub device_scale_factor: f64,
    /// `visualViewport.scale`; значение должно быть ровно 1.0.
    pub page_scale_factor: f64,
    pub dark_theme: PitchAccentDarkThemeProof,
    /// Объединение фактических прямоугольников узлов графиков в координатах документа.
    pub graph_union_rect: PitchAccentCaptureRect,
    /// Итоговая область в координатах документа, переданная браузеру для снимка.
    pub capture_rect: PitchAccentCaptureRect,
}

/// Семантический валидатор PNG pitch-accent.
///
/// PNG декодируется полностью, а `VERIFIED` возвращается только при наличии
/// структурных свидетельств об источнике JPDB и отрисовке. Одной сигнатуры или
/// успешного декодирования изображения недостаточно.
#[derive(Debug, Clone, Copy, Default)]
pub struct PitchAccentImageValidator;

impl PitchAccentImageValidator {
    pub const VALIDATOR_ID: &'static str = "jpdb-pitch-accent-render";
    pub const VALIDATOR_VERSION: &'static str = "5";
    pub const REQUIRED_DEVICE_SCALE_FACTOR: f64 = 3.0;

    /// Устойчивый идентификатор валидатора для манифеста и проверок потребителя.
    pub fn validator_identity() -> ValidatorIdentity {
        Self.identity()
    }
}

impl SemanticValidator for PitchAccentImageValidator {
    fn identity(&self) -> ValidatorIdentity {
        ValidatorIdentity::new(Self::VALIDATOR_ID, Self::VALIDATOR_VERSION)
            .expect("статический идентификатор валидатора pitch-accent корректен")
    }

    fn validate(
        &self,
        asset: &AssetRecord,
        bytes: &mut dyn Read,
    ) -> Result<SemanticDecision, ValidatorFailure> {
        let mut contents = Vec::new();
        bytes
            .take(PITCH_ACCENT_MAX_ASSET_BYTES + 1)
            .read_to_end(&mut contents)
            .map_err(|error| {
                ValidatorFailure::new(
                    "read_failed",
                    format!("ошибка чтения входных данных: {error}"),
                )
            })?;
        if contents.len() as u64 > PITCH_ACCENT_MAX_ASSET_BYTES {
            return Ok(decision(
                SemanticStatus::Corrupt,
                "pitch_accent_png_too_large",
                "размер PNG превышает установленный предел",
                json!({"max_bytes": PITCH_ACCENT_MAX_ASSET_BYTES}),
            ));
        }

        if asset.format != DetectedFormat::Png
            || DetectedFormat::from_signature(&contents) != DetectedFormat::Png
        {
            return Ok(decision(
                SemanticStatus::Corrupt,
                "pitch_accent_png_required",
                "канонический файл домена pitch-accent должен быть PNG",
                json!({"format": format!("{:?}", asset.format)}),
            ));
        }
        let decoded_png = match decode_png(&contents) {
            Ok(image) => image,
            Err(error) => {
                return Ok(decision(
                    SemanticStatus::Corrupt,
                    "pitch_accent_png_decode_failed",
                    "PNG не прошёл полное декодирование",
                    json!({"message": error}),
                ));
            }
        };

        let domain = PitchAccentDomainPolicy;
        if let Err(error) = domain.validate_identity(&asset.identity) {
            return Ok(decision(
                SemanticStatus::Rejected,
                "pitch_accent_identity_mismatch",
                "идентификатор не принадлежит домену pitch-accent",
                json!({"message": error.message}),
            ));
        }

        let Some(value) = asset.domain_metadata.clone() else {
            return Ok(incomplete_evidence("поле domain_metadata отсутствует"));
        };
        let metadata: PitchAccentDomainMetadata = match serde_json::from_value(value) {
            Ok(metadata) => metadata,
            Err(error) => return Ok(incomplete_evidence(error.to_string())),
        };
        match validate_evidence(&asset.identity.key, &metadata, &decoded_png) {
            Ok(()) => Ok(decision(
                SemanticStatus::Verified,
                "pitch_accent_source_render_verified",
                "PNG полностью декодирован, структурные данные рендеринга JPDB согласованы",
                serde_json::to_value(metadata).unwrap_or_else(|_| json!({})),
            )),
            Err(EvidenceFailure::Contradiction(message)) => Ok(decision(
                SemanticStatus::Rejected,
                "pitch_accent_evidence_mismatch",
                "данные источника и рендеринга противоречат идентификатору или источнику JPDB",
                json!({"message": message}),
            )),
            Err(EvidenceFailure::Incomplete(message)) => Ok(incomplete_evidence(message)),
        }
    }
}

pub(crate) enum EvidenceFailure {
    Incomplete(String),
    Contradiction(String),
}

impl EvidenceFailure {
    pub(crate) fn into_message(self) -> String {
        match self {
            Self::Incomplete(message) | Self::Contradiction(message) => message,
        }
    }
}

fn validate_evidence(
    surface: &str,
    metadata: &PitchAccentDomainMetadata,
    image: &image::DynamicImage,
) -> Result<(), EvidenceFailure> {
    if metadata.surface != surface {
        return Err(EvidenceFailure::Contradiction(
            "surface в метаданных не совпадает с ключом идентификатора".into(),
        ));
    }
    if metadata.jpdb_vocabulary_id == 0 {
        return Err(EvidenceFailure::Incomplete(
            "идентификатор словаря JPDB должен быть положительным".into(),
        ));
    }
    if metadata.reading.trim().is_empty() {
        return Err(EvidenceFailure::Incomplete(
            "поле reading не заполнено".into(),
        ));
    }

    let evidence = &metadata.evidence;
    if evidence.provider != PitchAccentProvider::Jpdb {
        return Err(EvidenceFailure::Contradiction(
            "поставщик в подтверждении не соответствует JPDB".into(),
        ));
    }
    let route = parse_jpdb_vocabulary_route(&evidence.source_url).map_err(|_| {
        EvidenceFailure::Contradiction(
            "URL источника должен вести на точный HTTPS-маршрут словарной записи `jpdb.io`".into(),
        )
    })?;
    if route.vocabulary_id != metadata.jpdb_vocabulary_id {
        return Err(EvidenceFailure::Contradiction(
            "ID маршрута JPDB не совпадает с ID словарной записи в метаданных".into(),
        ));
    }
    validate_resolved_forms(&evidence.resolved_forms)?;
    if !evidence
        .resolved_forms
        .iter()
        .any(|form| form.surface == metadata.surface && form.reading == metadata.reading)
    {
        return Err(EvidenceFailure::Contradiction(
            "пара значений `surface` и `reading` в метаданных отсутствует среди разрешённых форм JPDB"
                .into(),
        ));
    }
    if !evidence.resolved_forms.iter().any(|form| {
        form.surface == route.surface
            && route
                .reading
                .as_ref()
                .is_none_or(|reading| form.reading == *reading)
    }) {
        return Err(EvidenceFailure::Contradiction(
            "пара написания и чтения из маршрута JPDB отсутствует среди разрешённых форм".into(),
        ));
    }
    if evidence.graph_count == 0 {
        return Err(EvidenceFailure::Incomplete(
            "подтверждение источника не содержит графиков акцента".into(),
        ));
    }

    let render = &evidence.render;
    if render.kind != PitchAccentRenderKind::BrowserRegionScreenshot {
        return Err(EvidenceFailure::Incomplete(
            "все графики pitch accent должны входить в один нативный снимок области браузера"
                .into(),
        ));
    }
    if render.selector.trim().is_empty() || render.selector.chars().any(char::is_control) {
        return Err(EvidenceFailure::Incomplete(
            "селектор render отсутствует или содержит управляющие символы".into(),
        ));
    }
    if render.graphs.len() != evidence.graph_count as usize
        || render.graphs.iter().enumerate().any(|(index, graph)| {
            graph.index != index as u32
                || graph.selector.trim().is_empty()
                || graph.selector.chars().any(char::is_control)
        })
    {
        return Err(EvidenceFailure::Contradiction(
            "список CSS-селекторов графиков не соответствует `graph_count` и порядку DOM".into(),
        ));
    }
    if render.viewport_width == 0
        || render.viewport_height == 0
        || render.document_width == 0
        || render.document_height == 0
        || render.pixel_width == 0
        || render.pixel_height == 0
    {
        return Err(EvidenceFailure::Incomplete(
            "размеры области просмотра и изображения должны быть положительными".into(),
        ));
    }
    if render.pixel_width != image.width() || render.pixel_height != image.height() {
        return Err(EvidenceFailure::Contradiction(
            "размеры снимка не совпадают с размерами декодированного PNG".into(),
        ));
    }
    if render.device_scale_factor != PitchAccentImageValidator::REQUIRED_DEVICE_SCALE_FACTOR {
        return Err(EvidenceFailure::Incomplete(
            "масштаб устройства должен быть ровно 3.0".into(),
        ));
    }
    if render.page_scale_factor != 1.0 {
        return Err(EvidenceFailure::Incomplete(
            "масштаб страницы должен быть ровно 1.0".into(),
        ));
    }
    validate_capture_geometry(render)?;
    validate_dark_theme(&render.dark_theme)?;
    validate_capture_background(&render.dark_theme, image)?;

    let browser = &evidence.browser;
    if [
        browser.product.as_str(),
        browser.protocol_version.as_str(),
        browser.revision.as_str(),
        browser.user_agent.as_str(),
        browser.js_version.as_str(),
    ]
    .iter()
    .any(|value| value.trim().is_empty() || value.chars().any(char::is_control))
    {
        return Err(EvidenceFailure::Incomplete(
            "сведения о среде выполнения браузера неполны".into(),
        ));
    }
    Ok(())
}

fn validate_resolved_forms(forms: &[PitchAccentResolvedForm]) -> Result<(), EvidenceFailure> {
    if forms.is_empty()
        || forms.iter().any(|form| {
            form.surface.trim().is_empty()
                || form.reading.trim().is_empty()
                || form.surface.chars().any(char::is_control)
                || form.reading.chars().any(char::is_control)
        })
    {
        return Err(EvidenceFailure::Incomplete(
            "список разрешённых пар написания и чтения JPDB пуст или некорректен".into(),
        ));
    }
    Ok(())
}

fn validate_dark_theme(proof: &PitchAccentDarkThemeProof) -> Result<(), EvidenceFailure> {
    if proof
        .document_element_classes
        .iter()
        .any(|class| class.trim().is_empty() || class.chars().any(char::is_control))
        || proof.prefers_color_scheme.chars().any(char::is_control)
        || proof.computed_color_scheme.trim().is_empty()
        || proof.computed_color_scheme.chars().any(char::is_control)
        || proof.background_selector.trim().is_empty()
        || proof.background_selector.chars().any(char::is_control)
    {
        return Err(EvidenceFailure::Incomplete(
            "наблюдаемое состояние тёмной темы неполно или некорректно".into(),
        ));
    }
    if !proof
        .document_element_classes
        .iter()
        .any(|class| class == "dark-mode")
        || proof.prefers_color_scheme != "dark"
    {
        return Err(EvidenceFailure::Incomplete(
            "документ или параметр темы браузера не подтверждают активную тёмную тему".into(),
        ));
    }
    if relative_luminance(proof.background_rgb) > 0.20 {
        return Err(EvidenceFailure::Contradiction(
            "наблюдённый сплошной фон RGB недостаточно тёмный для снимка JPDB".into(),
        ));
    }
    Ok(())
}

pub(crate) fn validate_capture_geometry(
    render: &PitchAccentRenderEvidence,
) -> Result<(), EvidenceFailure> {
    if render.coordinate_space != PitchAccentCoordinateSpace::Document {
        return Err(EvidenceFailure::Incomplete(
            "снимок браузера должен явно использовать координаты документа".into(),
        ));
    }
    if !render.scroll_x.is_finite()
        || !render.scroll_y.is_finite()
        || render.scroll_x < 0.0
        || render.scroll_y < 0.0
        || render.scroll_x > f64::from(render.document_width)
        || render.scroll_y > f64::from(render.document_height)
    {
        return Err(EvidenceFailure::Incomplete(
            "значения прокрутки документа должны быть конечными и неотрицательными".into(),
        ));
    }
    if render.graphs.is_empty() {
        return Err(EvidenceFailure::Incomplete(
            "свидетельства захвата не содержат прямоугольников графиков".into(),
        ));
    }

    let geometry_tolerance = PITCH_ACCENT_CAPTURE_GEOMETRY_TOLERANCE_CSS_PX;
    let mut left = f64::INFINITY;
    let mut top = f64::INFINITY;
    let mut right = f64::NEG_INFINITY;
    let mut bottom = f64::NEG_INFINITY;
    for graph in &render.graphs {
        validate_positive_rect(graph.viewport_rect, "график в области просмотра")?;
        validate_positive_rect(graph.document_rect, "график в координатах документа")?;
        if graph.viewport_rect.x < -geometry_tolerance
            || graph.viewport_rect.y < -geometry_tolerance
            || graph.viewport_rect.x + graph.viewport_rect.width
                > f64::from(render.viewport_width) + geometry_tolerance
            || graph.viewport_rect.y + graph.viewport_rect.height
                > f64::from(render.viewport_height) + geometry_tolerance
        {
            return Err(EvidenceFailure::Contradiction(
                "прямоугольник графика выходит за границы области просмотра".into(),
            ));
        }
        if (graph.document_rect.x - (graph.viewport_rect.x + render.scroll_x)).abs()
            > geometry_tolerance
            || (graph.document_rect.y - (graph.viewport_rect.y + render.scroll_y)).abs()
                > geometry_tolerance
            || (graph.document_rect.width - graph.viewport_rect.width).abs() > geometry_tolerance
            || (graph.document_rect.height - graph.viewport_rect.height).abs() > geometry_tolerance
            || graph.document_rect.x < -geometry_tolerance
            || graph.document_rect.y < -geometry_tolerance
            || graph.document_rect.x + graph.document_rect.width
                > f64::from(render.document_width) + geometry_tolerance
            || graph.document_rect.y + graph.document_rect.height
                > f64::from(render.document_height) + geometry_tolerance
        {
            return Err(EvidenceFailure::Contradiction(
                "прямоугольник графика в документе не соответствует области просмотра и прокрутке"
                    .into(),
            ));
        }
        left = left.min(graph.document_rect.x);
        top = top.min(graph.document_rect.y);
        right = right.max(graph.document_rect.x + graph.document_rect.width);
        bottom = bottom.max(graph.document_rect.y + graph.document_rect.height);
    }

    let expected_union = PitchAccentCaptureRect {
        x: left,
        y: top,
        width: right - left,
        height: bottom - top,
    };
    validate_positive_rect(render.graph_union_rect, "объединение графиков")?;
    if !rects_match(render.graph_union_rect, expected_union, geometry_tolerance) {
        return Err(EvidenceFailure::Contradiction(
            "объединение не соответствует прямоугольникам графиков в координатах документа".into(),
        ));
    }

    let rect = render.capture_rect;
    validate_positive_rect(rect, "снимок")?;
    if rect.x < 0.0
        || rect.y < 0.0
        || rect.x + rect.width > f64::from(render.document_width)
        || rect.y + rect.height > f64::from(render.document_height)
    {
        return Err(EvidenceFailure::Contradiction(
            "область снимка выходит за пределы страницы документа".into(),
        ));
    }

    if !rects_match(rect, expected_union, geometry_tolerance) {
        return Err(EvidenceFailure::Contradiction(
            "область снимка должна точно совпадать с объединением прямоугольников графиков".into(),
        ));
    }

    if !capture_pixel_dimensions_match(
        render.pixel_width,
        render.pixel_height,
        rect.width,
        rect.height,
        render.device_scale_factor,
    ) {
        return Err(EvidenceFailure::Contradiction(
            "размеры PNG не соответствуют области снимка в CSS-пикселях и масштабу устройства"
                .into(),
        ));
    }
    Ok(())
}

fn validate_positive_rect(
    rect: PitchAccentCaptureRect,
    label: &str,
) -> Result<(), EvidenceFailure> {
    if ![rect.x, rect.y, rect.width, rect.height]
        .iter()
        .all(|value| value.is_finite())
        || rect.width <= 0.0
        || rect.height <= 0.0
    {
        return Err(EvidenceFailure::Incomplete(format!(
            "{label}: прямоугольник должен иметь конечные положительные размеры"
        )));
    }
    Ok(())
}

fn rects_match(
    actual: PitchAccentCaptureRect,
    expected: PitchAccentCaptureRect,
    tolerance: f64,
) -> bool {
    (actual.x - expected.x).abs() <= tolerance
        && (actual.y - expected.y).abs() <= tolerance
        && (actual.width - expected.width).abs() <= tolerance
        && (actual.height - expected.height).abs() <= tolerance
}

pub(crate) fn validate_capture_background(
    proof: &PitchAccentDarkThemeProof,
    image: &image::DynamicImage,
) -> Result<(), EvidenceFailure> {
    let (width, height) = image.dimensions();
    if width == 0 || height == 0 {
        return Err(EvidenceFailure::Incomplete(
            "в PNG нет пикселей для проверки сплошного фона".into(),
        ));
    }
    let mut matching_background_pixels = 0usize;
    let mut contrasting_graph_pixels = 0usize;
    // Проверяется сам снимок: искусственная рамка вокруг графика не требуется.
    // В области графика JPDB могут присутствовать и тёмный фон, и линии графика.
    for pixel in image.pixels().map(|(_, _, pixel)| pixel.0) {
        if pixel[3] != 255 {
            return Err(EvidenceFailure::Contradiction(
                "в области графиков найден пиксель, который не является полностью непрозрачным"
                    .into(),
            ));
        }
        let matches_background =
            pixel[..3]
                .iter()
                .zip(proof.background_rgb)
                .all(|(actual, expected)| {
                    actual.abs_diff(expected) <= PITCH_ACCENT_BACKGROUND_RGB_TOLERANCE
                });
        if matches_background {
            matching_background_pixels += 1;
        }
        if pixel[..3]
            .iter()
            .zip(proof.background_rgb)
            .any(|(actual, expected)| {
                actual.abs_diff(expected) > PITCH_ACCENT_GRAPH_PIXEL_MIN_RGB_DIFFERENCE
            })
        {
            contrasting_graph_pixels += 1;
        }
    }
    if matching_background_pixels == 0
        || contrasting_graph_pixels < PITCH_ACCENT_MIN_CONTRASTING_GRAPH_PIXELS
    {
        return Err(EvidenceFailure::Contradiction(format!(
            "PNG должен содержать наблюдённый фон и не менее {PITCH_ACCENT_MIN_CONTRASTING_GRAPH_PIXELS} контрастных пикселей графика"
        )));
    }
    Ok(())
}

pub(crate) fn relative_luminance(rgb: [u8; 3]) -> f64 {
    let linear = rgb.map(|channel| {
        let value = f64::from(channel) / 255.0;
        if value <= 0.04045 {
            value / 12.92
        } else {
            ((value + 0.055) / 1.055).powf(2.4)
        }
    });
    0.2126 * linear[0] + 0.7152 * linear[1] + 0.0722 * linear[2]
}

/// Разобранный и проверенный маршрут словарной записи `jpdb.io`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct JpdbVocabularyRoute {
    pub(crate) vocabulary_id: u64,
    pub(crate) surface: String,
    pub(crate) reading: Option<String>,
}

/// Ошибка разбора точного маршрута словарной записи JPDB.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum JpdbVocabularyRouteError {
    Url,
    Origin,
    Path,
    VocabularyId,
    Encoding,
}

/// Разбирает только точный путь словарной записи JPDB, игнорируя строку запроса и фрагмент.
pub(crate) fn parse_jpdb_vocabulary_route(
    value: &str,
) -> Result<JpdbVocabularyRoute, JpdbVocabularyRouteError> {
    let url = Url::parse(value).map_err(|_| JpdbVocabularyRouteError::Url)?;
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url
            .host_str()
            .is_none_or(|host| !host.eq_ignore_ascii_case("jpdb.io"))
        || url.port_or_known_default() != Some(443)
    {
        return Err(JpdbVocabularyRouteError::Origin);
    }
    let encoded_segments: Vec<_> = url
        .path_segments()
        .ok_or(JpdbVocabularyRouteError::Path)?
        .collect();
    if !(encoded_segments.len() == 3 || encoded_segments.len() == 4)
        || encoded_segments.iter().any(|segment| segment.is_empty())
        || encoded_segments[0] != "vocabulary"
    {
        return Err(JpdbVocabularyRouteError::Path);
    }
    if encoded_segments[1]
        .bytes()
        .any(|byte| !byte.is_ascii_digit())
    {
        return Err(JpdbVocabularyRouteError::VocabularyId);
    }
    let vocabulary_id = encoded_segments[1]
        .parse::<u64>()
        .ok()
        .filter(|id| *id > 0 && id.to_string() == encoded_segments[1])
        .ok_or(JpdbVocabularyRouteError::VocabularyId)?;
    let surface = decode_path_segment(encoded_segments[2])?;
    let reading = encoded_segments
        .get(3)
        .map(|segment| decode_path_segment(segment))
        .transpose()?;
    if surface.trim().is_empty()
        || reading
            .as_ref()
            .is_some_and(|value| value.trim().is_empty())
        || surface.eq_ignore_ascii_case("used-in")
        || reading
            .as_ref()
            .is_some_and(|value| value.eq_ignore_ascii_case("used-in"))
    {
        return Err(JpdbVocabularyRouteError::Path);
    }
    Ok(JpdbVocabularyRoute {
        vocabulary_id,
        surface,
        reading,
    })
}

fn decode_path_segment(segment: &str) -> Result<String, JpdbVocabularyRouteError> {
    let input = segment.as_bytes();
    let mut decoded = Vec::with_capacity(input.len());
    let mut index = 0;
    while index < input.len() {
        if input[index] == b'%' {
            if index + 2 >= input.len() {
                return Err(JpdbVocabularyRouteError::Encoding);
            }
            let high = (input[index + 1] as char)
                .to_digit(16)
                .ok_or(JpdbVocabularyRouteError::Encoding)?;
            let low = (input[index + 2] as char)
                .to_digit(16)
                .ok_or(JpdbVocabularyRouteError::Encoding)?;
            decoded.push(((high << 4) | low) as u8);
            index += 3;
        } else {
            decoded.push(input[index]);
            index += 1;
        }
    }
    let value = String::from_utf8(decoded).map_err(|_| JpdbVocabularyRouteError::Encoding)?;
    if value.chars().any(char::is_control) {
        return Err(JpdbVocabularyRouteError::Encoding);
    }
    Ok(value)
}

fn validate_hash(hash: &str) -> Result<(), AssetError> {
    if hash.len() != 64
        || !hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(AssetError::new(
            ErrorCode::ManifestCorrupt,
            "SHA-256 должен состоять из 64 строчных hex-символов",
        ));
    }
    Ok(())
}

fn decode_png(bytes: &[u8]) -> Result<image::DynamicImage, String> {
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(PITCH_ACCENT_MAX_IMAGE_DIMENSION);
    limits.max_image_height = Some(PITCH_ACCENT_MAX_IMAGE_DIMENSION);
    limits.max_alloc = Some(PITCH_ACCENT_MAX_DECODE_ALLOCATION_BYTES);
    let decoder = image::codecs::png::PngDecoder::with_limits(Cursor::new(bytes), limits)
        .map_err(|error| error.to_string())?;
    let (width, height) = decoder.dimensions();
    let bytes_per_pixel = u64::from(decoder.color_type().bytes_per_pixel());
    let output_allocation = u64::from(width)
        .checked_mul(u64::from(height))
        .and_then(|pixels| pixels.checked_mul(bytes_per_pixel))
        .ok_or_else(|| "объём декодируемого PNG выходит за числовой предел".to_owned())?;
    if output_allocation > PITCH_ACCENT_MAX_DECODE_ALLOCATION_BYTES {
        return Err(format!(
            "декодированное изображение потребует {output_allocation} байт при пределе {PITCH_ACCENT_MAX_DECODE_ALLOCATION_BYTES} байт"
        ));
    }
    image::DynamicImage::from_decoder(decoder).map_err(|error| error.to_string())
}

fn incomplete_evidence(message: impl Into<String>) -> SemanticDecision {
    decision(
        SemanticStatus::Uncertain,
        "pitch_accent_evidence_incomplete",
        "PNG не подтверждён полными данными источника и рендеринга",
        json!({"message": message.into()}),
    )
}

fn decision(
    status: SemanticStatus,
    kind: &str,
    summary: &str,
    details: serde_json::Value,
) -> SemanticDecision {
    SemanticDecision::new(
        status,
        vec![ValidationEvidence {
            kind: kind.to_owned(),
            summary: summary.to_owned(),
            details: Some(details),
        }],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_metadata(surface: &str) -> PitchAccentDomainMetadata {
        PitchAccentDomainMetadata {
            surface: surface.to_owned(),
            reading: "ゆうれい".into(),
            jpdb_vocabulary_id: 123,
            evidence: PitchAccentEvidence {
                provider: PitchAccentProvider::Jpdb,
                source_url: "https://jpdb.io/vocabulary/123/幽霊/ゆうれい#a".into(),
                resolved_forms: vec![PitchAccentResolvedForm {
                    surface: "幽霊".into(),
                    reading: "ゆうれい".into(),
                }],
                graph_count: 1,
                render: PitchAccentRenderEvidence {
                    kind: PitchAccentRenderKind::BrowserRegionScreenshot,
                    selector: ".pitch-accent-graph".into(),
                    graphs: vec![PitchAccentGraphEvidence {
                        index: 0,
                        selector: ".pitch-accent-graph".into(),
                        viewport_rect: PitchAccentCaptureRect {
                            x: 100.0,
                            y: 100.0,
                            width: 20.0,
                            height: 10.0,
                        },
                        document_rect: PitchAccentCaptureRect {
                            x: 100.0,
                            y: 200.0,
                            width: 20.0,
                            height: 10.0,
                        },
                    }],
                    coordinate_space: PitchAccentCoordinateSpace::Document,
                    viewport_width: 1280,
                    viewport_height: 900,
                    document_width: 1280,
                    document_height: 1800,
                    scroll_x: 0.0,
                    scroll_y: 100.0,
                    pixel_width: 60,
                    pixel_height: 30,
                    device_scale_factor: 3.0,
                    page_scale_factor: 1.0,
                    dark_theme: PitchAccentDarkThemeProof {
                        document_element_classes: vec!["dark-mode".into()],
                        prefers_color_scheme: "dark".into(),
                        computed_color_scheme: "dark".into(),
                        background_selector: ".subsection-pitch-accent".into(),
                        background_rgb: [24, 36, 48],
                    },
                    graph_union_rect: PitchAccentCaptureRect {
                        x: 100.0,
                        y: 200.0,
                        width: 20.0,
                        height: 10.0,
                    },
                    capture_rect: PitchAccentCaptureRect {
                        x: 100.0,
                        y: 200.0,
                        width: 20.0,
                        height: 10.0,
                    },
                },
                browser: BrowserRuntimeProvenance {
                    product: "Chrome/140".into(),
                    protocol_version: "1.3".into(),
                    revision: "1234567".into(),
                    user_agent: "Mozilla/5.0 Chrome/140".into(),
                    js_version: "V8 14.0".into(),
                    executable_source: crate::browser_runtime::BrowserExecutableSource::PathLookup,
                },
            },
        }
    }

    fn valid_png() -> Vec<u8> {
        let mut image = image::RgbaImage::from_pixel(60, 30, image::Rgba([24, 36, 48, 255]));
        for x in 10..50 {
            image.put_pixel(x, 15, image::Rgba([235, 235, 235, 255]));
        }
        let mut output = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(image)
            .write_to(&mut output, image::ImageFormat::Png)
            .unwrap();
        output.into_inner()
    }

    fn rgb_png(width: u32, height: u32) -> Vec<u8> {
        let mut image = image::RgbImage::from_pixel(width, height, image::Rgb([24, 36, 48]));
        if width > 0 && height > 0 {
            for x in (width / 4)..((width * 3 / 4).max(1)) {
                image.put_pixel(x, height / 2, image::Rgb([235, 235, 235]));
            }
        }
        let mut output = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(image)
            .write_to(&mut output, image::ImageFormat::Png)
            .unwrap();
        output.into_inner()
    }

    fn rgba_png(width: u32, height: u32) -> Vec<u8> {
        let image = image::RgbaImage::from_pixel(width, height, image::Rgba([24, 36, 48, 128]));
        let mut output = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(image)
            .write_to(&mut output, image::ImageFormat::Png)
            .unwrap();
        output.into_inner()
    }

    fn white_png(width: u32, height: u32) -> Vec<u8> {
        let image = image::RgbImage::from_pixel(width, height, image::Rgb([255, 255, 255]));
        let mut output = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(image)
            .write_to(&mut output, image::ImageFormat::Png)
            .unwrap();
        output.into_inner()
    }

    #[test]
    fn pitch_policy_keeps_surface_filename_and_isolates_unsupported_runtime_formats() {
        let policy = PitchAccentDomainPolicy;
        let identity = crate::model::AssetIdentity::new("pitch_accent", "飴").unwrap();
        let hash = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

        let png = policy
            .canonical_location(&identity, hash, DetectedFormat::Png)
            .unwrap();
        assert_eq!(png.storage_path, "assets/png/飴.png");
        assert_eq!(png.consumer_filename, "飴.pitch.png");

        let kanji = crate::domain::KanjiDomainPolicy
            .canonical_location(
                &crate::model::AssetIdentity::new("kanji", "飴").unwrap(),
                hash,
                DetectedFormat::Png,
            )
            .unwrap();
        assert_eq!(kanji.storage_path, "assets/png/飴.png");
        assert_eq!(kanji.consumer_filename, "飴.png");

        let gif = policy
            .canonical_location(&identity, hash, DetectedFormat::Gif)
            .unwrap();
        let identity_hash = sha256_hex("飴".as_bytes());
        assert_eq!(
            gif.storage_path,
            format!("assets/unsupported/{identity_hash}-{hash}.gif")
        );
        assert_eq!(gif.consumer_filename, "飴.gif");
        assert_eq!(policy.max_asset_bytes(), Some(PITCH_ACCENT_MAX_ASSET_BYTES));
        assert!(!policy.is_publishable_format(DetectedFormat::Gif));
        assert!(!policy.allows_verified_format(DetectedFormat::Gif));
        assert!(policy.allows_verified_format(DetectedFormat::Png));
        assert!(
            policy
                .legacy_location(&identity, hash, DetectedFormat::Png)
                .is_none()
        );
    }

    fn record(metadata: Option<PitchAccentDomainMetadata>) -> AssetRecord {
        AssetRecord {
            identity: crate::model::AssetIdentity::new("pitch_accent", "幽霊").unwrap(),
            storage_path: "assets/png/幽霊.png".into(),
            consumer_filename: "幽霊.pitch.png".into(),
            sha256: "0".repeat(64),
            byte_length: 0,
            format: DetectedFormat::Png,
            provenance: crate::model::Provenance {
                source_kind: "jpdb-browser".into(),
                source_name: "幽霊.png".into(),
            },
            lifecycle: crate::model::LifecycleState::Pending,
            validation: None,
            human_attestation: None,
            domain_metadata: metadata.and_then(|value| serde_json::to_value(value).ok()),
        }
    }

    fn validate(asset: &AssetRecord, png: &[u8]) -> Result<SemanticDecision, ValidatorFailure> {
        PitchAccentImageValidator.validate(asset, &mut Cursor::new(png))
    }

    #[test]
    fn decodable_png_with_structured_jpdb_evidence_is_verified() {
        let metadata = valid_metadata("幽霊");
        let asset = record(Some(metadata));
        let decision = validate(&asset, &valid_png()).unwrap();

        assert_eq!(PitchAccentImageValidator::validator_identity().version, "5");
        assert_eq!(decision.status, SemanticStatus::Verified);
        assert_eq!(
            decision.evidence[0].kind,
            "pitch_accent_source_render_verified"
        );
    }

    #[test]
    fn valid_png_without_evidence_is_uncertain() {
        let asset = record(None);
        let decision = validate(&asset, &valid_png()).unwrap();

        assert_eq!(decision.status, SemanticStatus::Uncertain);
        assert_ne!(decision.status, SemanticStatus::Verified);
    }

    #[test]
    fn mismatched_surface_or_capture_settings_cannot_be_verified() {
        let asset = record(Some(valid_metadata("ゆうれい")));
        assert_eq!(
            validate(&asset, &valid_png()).unwrap().status,
            SemanticStatus::Rejected
        );

        let mut metadata = valid_metadata("幽霊");
        metadata.evidence.render.pixel_width = 3;
        let asset = record(Some(metadata));
        assert_eq!(
            validate(&asset, &valid_png()).unwrap().status,
            SemanticStatus::Rejected
        );

        let mut metadata = valid_metadata("幽霊");
        metadata.evidence.render.device_scale_factor = 2.0;
        let asset = record(Some(metadata));
        assert_eq!(
            validate(&asset, &valid_png()).unwrap().status,
            SemanticStatus::Uncertain
        );

        let mut metadata = valid_metadata("幽霊");
        metadata.evidence.resolved_forms = vec![PitchAccentResolvedForm {
            surface: "幽霊".into(),
            reading: "げんき".into(),
        }];
        let asset = record(Some(metadata));
        assert_eq!(
            validate(&asset, &valid_png()).unwrap().status,
            SemanticStatus::Rejected
        );
    }

    #[test]
    fn source_url_and_typed_route_parser_share_the_exact_jpdb_contract() {
        for source_url in [
            "https://jpdb.io:443/vocabulary/123/幽霊/ゆうれい?tab=1#pitch",
            "https://jpdb.io/vocabulary/123/幽霊?tab=1#pitch",
        ] {
            assert!(parse_jpdb_vocabulary_route(source_url).is_ok());
            let mut metadata = valid_metadata("幽霊");
            metadata.evidence.source_url = source_url.into();
            let asset = record(Some(metadata));
            assert_eq!(
                validate(&asset, &valid_png()).unwrap().status,
                SemanticStatus::Verified,
                "валидатор должен принимать маршрут, допустимый общим разборщиком: {source_url}"
            );
        }

        for source_url in [
            "https://subdomain.jpdb.io/vocabulary/123/幽霊/ゆうれい",
            "https://jpdb.io/prefix/vocabulary/123/幽霊/ゆうれい",
            "https://jpdb.io/vocabulary/123/幽霊/used-in",
            "http://jpdb.io/vocabulary/123/幽霊/ゆうれい",
            "https://user:pass@jpdb.io/vocabulary/123/幽霊/ゆうれい",
            "https://jpdb.io:444/vocabulary/123/幽霊/ゆうれい",
            "https://jpdb.io/vocabulary/123/幽霊/ゆうれい/",
            "https://jpdb.io/vocabulary/123//ゆうれい",
            "https://jpdb.io/vocabulary/123/幽霊/ゆうれい/extra",
            "https://jpdb.io.evil.example/vocabulary/123/幽霊/ゆうれい",
            "https://jpdb.io/vocabulary/0123/幽霊/ゆうれい",
        ] {
            assert!(
                parse_jpdb_vocabulary_route(source_url).is_err(),
                "разборщик должен отвергать маршрут: {source_url}"
            );
            let mut metadata = valid_metadata("幽霊");
            metadata.evidence.source_url = source_url.into();
            let asset = record(Some(metadata));
            assert_eq!(
                validate(&asset, &valid_png()).unwrap().status,
                SemanticStatus::Rejected,
                "валидатор должен отвергать неверный URL источника: {source_url}"
            );
        }

        let mismatched_id = "https://jpdb.io/vocabulary/124/幽霊/ゆうれい";
        assert!(parse_jpdb_vocabulary_route(mismatched_id).is_ok());
        let mut metadata = valid_metadata("幽霊");
        metadata.evidence.source_url = mismatched_id.into();
        let asset = record(Some(metadata));
        assert_eq!(
            validate(&asset, &valid_png()).unwrap().status,
            SemanticStatus::Rejected,
            "структурно допустимый маршрут с неверным ID в метаданных отвергается"
        );
    }

    #[test]
    fn metadata_surface_and_reading_must_be_an_observed_pair() {
        let mut metadata = valid_metadata("幽霊");
        metadata.reading = "げんき".into();
        metadata
            .evidence
            .resolved_forms
            .push(PitchAccentResolvedForm {
                surface: "元気".into(),
                reading: "げんき".into(),
            });
        let asset = record(Some(metadata));

        assert_eq!(
            validate(&asset, &valid_png()).unwrap().status,
            SemanticStatus::Rejected
        );
    }

    #[test]
    fn dark_theme_graph_set_and_native_capture_geometry_are_required() {
        let mut metadata = valid_metadata("幽霊");
        metadata.evidence.render.kind = PitchAccentRenderKind::ElementScreenshot;
        let asset = record(Some(metadata));
        assert_eq!(
            validate(&asset, &valid_png()).unwrap().status,
            SemanticStatus::Uncertain
        );

        let mut metadata = valid_metadata("幽霊");
        metadata
            .evidence
            .render
            .dark_theme
            .document_element_classes
            .clear();
        let asset = record(Some(metadata));
        assert_eq!(
            validate(&asset, &valid_png()).unwrap().status,
            SemanticStatus::Uncertain
        );

        let mut metadata = valid_metadata("幽霊");
        metadata.evidence.render.dark_theme.prefers_color_scheme = "light".into();
        let asset = record(Some(metadata));
        assert_eq!(
            validate(&asset, &valid_png()).unwrap().status,
            SemanticStatus::Uncertain
        );

        let mut metadata = valid_metadata("幽霊");
        metadata.evidence.render.page_scale_factor = 1.25;
        let asset = record(Some(metadata));
        assert_eq!(
            validate(&asset, &valid_png()).unwrap().status,
            SemanticStatus::Uncertain
        );

        let mut metadata = valid_metadata("幽霊");
        metadata.evidence.render.capture_rect.width = 2.0;
        let asset = record(Some(metadata));
        assert_eq!(
            validate(&asset, &valid_png()).unwrap().status,
            SemanticStatus::Rejected
        );

        let mut metadata = valid_metadata("幽霊");
        metadata.evidence.graph_count = 2;
        let asset = record(Some(metadata));
        assert_eq!(
            validate(&asset, &valid_png()).unwrap().status,
            SemanticStatus::Rejected
        );
    }

    #[test]
    fn native_capture_clip_rounding_allows_at_most_three_output_pixels() {
        assert!(capture_pixel_dimensions_match(111, 81, 36.0, 26.0, 3.0));
        assert!(capture_pixel_dimensions_match(
            318, 279, 106.047, 93.781, 3.0
        ));
        assert!(!capture_pixel_dimensions_match(112, 78, 36.0, 26.0, 3.0));
        assert!(!capture_pixel_dimensions_match(108, 82, 36.0, 26.0, 3.0));
    }

    #[test]
    fn jpdb_reading_comparison_only_equates_hiragana_and_katakana() {
        assert!(jpdb_readings_equivalent("ネコ", "ねこ"));
        assert!(jpdb_readings_equivalent("スーパー", "すーぱー"));
        assert!(!jpdb_readings_equivalent("ネコ", "しょうじょ"));
        assert!(!jpdb_readings_equivalent("NEKO", "ねこ"));
    }

    #[test]
    fn validator_requires_current_render_metadata_for_verified_status() {
        let mut legacy = serde_json::to_value(valid_metadata("幽霊")).unwrap();
        let evidence = legacy["evidence"].as_object_mut().unwrap();
        evidence.remove("resolved_forms");
        let render = evidence["render"].as_object_mut().unwrap();
        render.remove("graphs");
        render.remove("page_scale_factor");
        render.remove("dark_theme");
        render.remove("capture_rect");

        let mut asset = record(None);
        asset.domain_metadata = Some(legacy);
        let decision = validate(&asset, &valid_png()).unwrap();

        assert_eq!(decision.status, SemanticStatus::Uncertain);
        assert_eq!(
            decision.evidence[0].kind,
            "pitch_accent_evidence_incomplete"
        );
    }

    #[test]
    fn page_coordinate_geometry_offsets_graphs_by_document_scroll() {
        let mut metadata = valid_metadata("幽霊");
        metadata.evidence.render.scroll_y = 120.0;
        metadata.evidence.render.graphs[0].document_rect.y = 220.0;
        metadata.evidence.render.graph_union_rect.y = 220.0;
        metadata.evidence.render.capture_rect.y = 220.0;
        let asset = record(Some(metadata));

        assert_eq!(
            validate(&asset, &valid_png()).unwrap().status,
            SemanticStatus::Verified
        );

        let mut metadata = valid_metadata("幽霊");
        metadata.evidence.render.graphs[0].document_rect.y += 1.0;
        let asset = record(Some(metadata));
        assert_eq!(
            validate(&asset, &valid_png()).unwrap().status,
            SemanticStatus::Rejected
        );

        let mut metadata = valid_metadata("幽霊");
        metadata.evidence.render.graphs[0].document_rect.width += 1.0;
        let asset = record(Some(metadata));
        assert_eq!(
            validate(&asset, &valid_png()).unwrap().status,
            SemanticStatus::Rejected,
            "ширина графика в координатах документа должна совпадать с областью просмотра"
        );
    }

    #[test]
    fn capture_must_equal_the_graph_union_and_document_page_bounds_are_verified() {
        let mut metadata = valid_metadata("幽霊");
        metadata.evidence.render.graphs[0].viewport_rect.y = 2.0;
        metadata.evidence.render.graphs[0].document_rect.y = 2.0;
        metadata.evidence.render.scroll_y = 0.0;
        metadata.evidence.render.graph_union_rect.y = 2.0;
        metadata.evidence.render.capture_rect.y = 2.0;
        metadata.evidence.render.capture_rect.height = 10.0;
        let asset = record(Some(metadata));
        assert_eq!(
            validate(&asset, &rgb_png(60, 30)).unwrap().status,
            SemanticStatus::Verified,
            "capture повторяет график, касающийся верхней границы страницы"
        );

        let mut metadata = valid_metadata("幽霊");
        metadata.evidence.render.graph_union_rect.width += 1.0;
        let asset = record(Some(metadata));
        assert_eq!(
            validate(&asset, &valid_png()).unwrap().status,
            SemanticStatus::Rejected
        );

        let mut metadata = valid_metadata("幽霊");
        metadata.evidence.render.capture_rect.x = -1.0;
        let asset = record(Some(metadata));
        assert_eq!(
            validate(&asset, &valid_png()).unwrap().status,
            SemanticStatus::Rejected
        );
    }

    #[test]
    fn dark_capture_must_contain_observed_background_and_graph_pixels() {
        let asset = record(Some(valid_metadata("幽霊")));
        assert_eq!(
            validate(&asset, &valid_png()).unwrap().status,
            SemanticStatus::Verified
        );

        let mut metadata = valid_metadata("幽霊");
        metadata.evidence.render.dark_theme.background_rgb = [255, 255, 255];
        let asset = record(Some(metadata));
        assert_eq!(
            validate(&asset, &white_png(60, 30)).unwrap().status,
            SemanticStatus::Rejected,
            "флаги тёмной темы не могут подтвердить светлый вычисленный фон"
        );

        let asset = record(Some(valid_metadata("幽霊")));
        assert_eq!(
            validate(&asset, &white_png(60, 30)).unwrap().status,
            SemanticStatus::Rejected,
            "содержимое снимка должно включать наблюдённый тёмный фон и пиксели графика"
        );

        let mut image = image::RgbaImage::from_pixel(60, 30, image::Rgba([24, 36, 48, 255]));
        image.put_pixel(30, 15, image::Rgba([235, 235, 235, 255]));
        let mut output = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(image)
            .write_to(&mut output, image::ImageFormat::Png)
            .unwrap();
        let asset = record(Some(valid_metadata("幽霊")));
        assert_eq!(
            validate(&asset, &output.into_inner()).unwrap().status,
            SemanticStatus::Rejected,
            "один случайный контрастный пиксель не подтверждает содержимое графика"
        );
    }

    #[test]
    fn transparent_tail_after_background_and_graph_pixels_is_rejected() {
        let mut image = image::RgbaImage::from_pixel(60, 30, image::Rgba([24, 36, 48, 255]));
        for x in 1..=8 {
            image.put_pixel(x, 0, image::Rgba([235, 235, 235, 255]));
        }
        image.put_pixel(59, 29, image::Rgba([24, 36, 48, 0]));
        let mut output = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(image)
            .write_to(&mut output, image::ImageFormat::Png)
            .unwrap();

        let asset = record(Some(valid_metadata("幽霊")));
        assert_eq!(
            validate(&asset, &output.into_inner()).unwrap().status,
            SemanticStatus::Rejected
        );
    }

    #[test]
    fn alpha_254_is_not_fully_opaque() {
        let mut image = image::RgbaImage::from_pixel(60, 30, image::Rgba([24, 36, 48, 254]));
        for x in 10..50 {
            image.put_pixel(x, 15, image::Rgba([235, 235, 235, 254]));
        }
        let mut output = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(image)
            .write_to(&mut output, image::ImageFormat::Png)
            .unwrap();

        let asset = record(Some(valid_metadata("幽霊")));
        assert_eq!(
            validate(&asset, &output.into_inner()).unwrap().status,
            SemanticStatus::Rejected
        );
    }

    #[test]
    fn one_background_pixel_and_eight_contrasting_pixels_satisfy_capture_evidence() {
        let proof = valid_metadata("幽霊").evidence.render.dark_theme;
        let mut image = image::RgbaImage::from_pixel(60, 30, image::Rgba([100, 100, 100, 255]));
        image.put_pixel(0, 0, image::Rgba([24, 36, 48, 255]));

        assert!(
            validate_capture_background(&proof, &image::DynamicImage::ImageRgba8(image)).is_ok(),
            "исходный контракт требует один совпавший пиксель фона и не менее восьми контрастных"
        );
    }

    #[test]
    fn invalid_png_bytes_are_corrupt_even_with_valid_evidence() {
        let asset = record(Some(valid_metadata("幽霊")));
        let decision = validate(&asset, b"\x89PNG\r\n\x1a\nnot a complete png").unwrap();

        assert_eq!(decision.status, SemanticStatus::Corrupt);
    }

    #[test]
    fn oversized_input_is_rejected_after_reading_only_limit_plus_one_byte() {
        struct CountingReader {
            remaining: usize,
            bytes_read: usize,
        }

        impl Read for CountingReader {
            fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
                let count = output.len().min(self.remaining);
                output[..count].fill(0);
                self.remaining -= count;
                self.bytes_read += count;
                Ok(count)
            }
        }

        let asset = record(Some(valid_metadata("幽霊")));
        let mut reader = CountingReader {
            remaining: PITCH_ACCENT_MAX_ASSET_BYTES as usize + 128,
            bytes_read: 0,
        };

        let decision = PitchAccentImageValidator
            .validate(&asset, &mut reader)
            .unwrap();

        assert_eq!(decision.status, SemanticStatus::Corrupt);
        assert_eq!(decision.evidence[0].kind, "pitch_accent_png_too_large");
        assert_eq!(reader.bytes_read, PITCH_ACCENT_MAX_ASSET_BYTES as usize + 1);
    }

    #[test]
    fn png_dimensions_above_limit_are_corrupt() {
        let asset = record(Some(valid_metadata("幽霊")));
        let oversized = rgb_png(PITCH_ACCENT_MAX_IMAGE_DIMENSION + 1, 1);

        let decision = validate(&asset, &oversized).unwrap();

        assert_eq!(decision.status, SemanticStatus::Corrupt);
        assert_eq!(decision.evidence[0].kind, "pitch_accent_png_decode_failed");
    }

    #[test]
    fn png_decode_allocation_above_limit_is_corrupt() {
        let asset = record(Some(valid_metadata("幽霊")));
        let oversized = rgba_png(
            PITCH_ACCENT_MAX_IMAGE_DIMENSION,
            PITCH_ACCENT_MAX_IMAGE_DIMENSION,
        );

        let decision = validate(&asset, &oversized).unwrap();

        assert_eq!(decision.status, SemanticStatus::Corrupt);
        assert_eq!(decision.evidence[0].kind, "pitch_accent_png_decode_failed");
        let message = decision.evidence[0].details.as_ref().unwrap()["message"]
            .as_str()
            .unwrap();
        assert!(message.contains("декодированное изображение потребует"));
    }
}
