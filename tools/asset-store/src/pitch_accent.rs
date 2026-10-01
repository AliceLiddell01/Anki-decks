//! Предметная policy и fail-closed semantic validator для pitch-accent PNG.

use std::io::{Cursor, Read};

use serde::{Deserialize, Serialize};
use serde_json::json;
use url::Url;

use crate::browser_runtime::BrowserRuntimeProvenance;
use crate::domain::{
    AssetDomainPolicy, CanonicalAssetLocation, extension_for_format,
    validate_safe_consumer_filename,
};
use crate::error::{AssetError, ErrorCode};
use crate::hashing::sha256_hex;
use crate::model::{
    AssetRecord, DetectedFormat, SemanticDecision, SemanticStatus, ValidationEvidence,
    ValidatorIdentity,
};
use crate::validation::{SemanticValidator, ValidatorFailure};

/// Независимая publishable policy для canonical pitch-accent PNG.
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
                "pitch-accent domain принимает только namespace `pitch_accent`",
            ));
        }
        let filename = format!("{}.png", identity.key);
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
        let consumer_filename = format!("{}.{extension}", identity.key);
        validate_safe_consumer_filename(&consumer_filename, format)?;
        Ok(CanonicalAssetLocation {
            storage_path: format!("assets/{category}/{storage_filename}"),
            consumer_filename,
        })
    }

    fn legacy_location(
        &self,
        _identity: &crate::model::AssetIdentity,
        _sha256: &str,
        _format: DetectedFormat,
    ) -> Option<CanonicalAssetLocation> {
        // Pitch accent не существовал в schema 3/4.
        None
    }

    fn is_publishable_format(&self, format: DetectedFormat) -> bool {
        format == DetectedFormat::Png
    }

    fn allows_verified_format(&self, format: DetectedFormat) -> bool {
        format == DetectedFormat::Png
    }

    fn max_asset_bytes(&self) -> Option<u64> {
        None
    }

    fn content_addressed_storage(&self) -> bool {
        false
    }
}

/// JPDB vocabulary metadata, не участвующая в имени canonical файла.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PitchAccentDomainMetadata {
    /// Точная surface form; она должна совпасть с `AssetIdentity.key`.
    pub surface: String,
    /// Reading для карточки и provenance, но не для filename.
    pub reading: String,
    /// Положительный JPDB vocabulary ID.
    pub jpdb_vocabulary_id: u64,
    /// Структурные сведения о source, capture и browser runtime.
    pub evidence: PitchAccentEvidence,
}

/// Минимальное структурное evidence, достаточное для автоматической проверки.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PitchAccentEvidence {
    pub provider: PitchAccentProvider,
    pub source_url: String,
    /// Число отдельных pitch graphs, подтверждённых на source странице.
    pub graph_count: u32,
    pub render: PitchAccentRenderEvidence,
    pub browser: BrowserRuntimeProvenance,
}

/// Источник, для которого предназначен этот domain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PitchAccentProvider {
    Jpdb,
}

/// Вид получения canonical PNG.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PitchAccentRenderKind {
    ElementScreenshot,
    SvgElementScreenshot,
    CanvasElementScreenshot,
}

/// Фактические параметры rendered элемента.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PitchAccentRenderEvidence {
    pub kind: PitchAccentRenderKind,
    pub selector: String,
    pub viewport_width: u32,
    pub viewport_height: u32,
    pub pixel_width: u32,
    pub pixel_height: u32,
    pub device_scale_factor: f64,
}

/// Production semantic validator pitch-accent PNG.
///
/// PNG декодируется полностью, а `VERIFIED` возвращается только при наличии
/// структурного JPDB source/render evidence. Одной сигнатуры или успешного
/// декодирования изображения недостаточно.
#[derive(Debug, Clone, Copy, Default)]
pub struct PitchAccentImageValidator;

impl PitchAccentImageValidator {
    pub const VALIDATOR_ID: &'static str = "jpdb-pitch-accent-render";
    pub const VALIDATOR_VERSION: &'static str = "1";
    pub const REQUIRED_DEVICE_SCALE_FACTOR: f64 = 3.0;

    /// Устойчивая identity validator для manifest и consumer checks.
    pub fn validator_identity() -> ValidatorIdentity {
        Self.identity()
    }
}

impl SemanticValidator for PitchAccentImageValidator {
    fn identity(&self) -> ValidatorIdentity {
        ValidatorIdentity::new(Self::VALIDATOR_ID, Self::VALIDATOR_VERSION)
            .expect("static pitch-accent validator identity is valid")
    }

    fn validate(
        &self,
        asset: &AssetRecord,
        bytes: &mut dyn Read,
    ) -> Result<SemanticDecision, ValidatorFailure> {
        let mut contents = Vec::new();
        bytes
            .read_to_end(&mut contents)
            .map_err(|error| ValidatorFailure::new("read_failed", error.to_string()))?;

        if asset.format != DetectedFormat::Png
            || DetectedFormat::from_signature(&contents) != DetectedFormat::Png
        {
            return Ok(decision(
                SemanticStatus::Corrupt,
                "pitch_accent_png_required",
                "pitch-accent canonical asset должен быть PNG",
                json!({"format": format!("{:?}", asset.format)}),
            ));
        }
        let (pixel_width, pixel_height) = match decode_png(&contents) {
            Ok(dimensions) => dimensions,
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
                "identity не принадлежит pitch-accent domain",
                json!({"message": error.message}),
            ));
        }

        let Some(value) = asset.domain_metadata.clone() else {
            return Ok(incomplete_evidence("domain_metadata отсутствует"));
        };
        let metadata: PitchAccentDomainMetadata = match serde_json::from_value(value) {
            Ok(metadata) => metadata,
            Err(error) => return Ok(incomplete_evidence(error.to_string())),
        };
        match validate_evidence(&asset.identity.key, &metadata, pixel_width, pixel_height) {
            Ok(()) => Ok(decision(
                SemanticStatus::Verified,
                "pitch_accent_source_render_verified",
                "PNG полностью декодирован и согласован со структурным JPDB render evidence",
                serde_json::to_value(metadata).unwrap_or_else(|_| json!({})),
            )),
            Err(EvidenceFailure::Contradiction(message)) => Ok(decision(
                SemanticStatus::Rejected,
                "pitch_accent_evidence_mismatch",
                "source/render evidence противоречит identity или JPDB source",
                json!({"message": message}),
            )),
            Err(EvidenceFailure::Incomplete(message)) => Ok(incomplete_evidence(message)),
        }
    }
}

enum EvidenceFailure {
    Incomplete(String),
    Contradiction(String),
}

fn validate_evidence(
    surface: &str,
    metadata: &PitchAccentDomainMetadata,
    actual_pixel_width: u32,
    actual_pixel_height: u32,
) -> Result<(), EvidenceFailure> {
    if metadata.surface != surface {
        return Err(EvidenceFailure::Contradiction(
            "metadata surface не совпадает с identity key".into(),
        ));
    }
    if metadata.jpdb_vocabulary_id == 0 {
        return Err(EvidenceFailure::Incomplete(
            "JPDB vocabulary ID должен быть положительным".into(),
        ));
    }
    if metadata.reading.trim().is_empty() {
        return Err(EvidenceFailure::Incomplete("reading отсутствует".into()));
    }

    let evidence = &metadata.evidence;
    if evidence.provider != PitchAccentProvider::Jpdb {
        return Err(EvidenceFailure::Contradiction(
            "provider evidence не равен JPDB".into(),
        ));
    }
    if !is_jpdb_source_url(&evidence.source_url, metadata.jpdb_vocabulary_id) {
        return Err(EvidenceFailure::Contradiction(
            "source URL должен быть абсолютным HTTPS URL домена jpdb.io".into(),
        ));
    }
    if evidence.graph_count == 0 {
        return Err(EvidenceFailure::Incomplete(
            "source evidence не содержит pitch graphs".into(),
        ));
    }

    let render = &evidence.render;
    if render.selector.trim().is_empty() || render.selector.chars().any(char::is_control) {
        return Err(EvidenceFailure::Incomplete(
            "render selector отсутствует или содержит управляющие символы".into(),
        ));
    }
    if render.viewport_width == 0
        || render.viewport_height == 0
        || render.pixel_width == 0
        || render.pixel_height == 0
    {
        return Err(EvidenceFailure::Incomplete(
            "viewport и pixel dimensions должны быть положительными".into(),
        ));
    }
    if render.pixel_width != actual_pixel_width || render.pixel_height != actual_pixel_height {
        return Err(EvidenceFailure::Contradiction(
            "render pixel dimensions не совпадают с декодированным PNG".into(),
        ));
    }
    if render.device_scale_factor != PitchAccentImageValidator::REQUIRED_DEVICE_SCALE_FACTOR {
        return Err(EvidenceFailure::Incomplete(
            "device scale factor должен быть ровно 3.0".into(),
        ));
    }

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
            "browser runtime provenance неполон".into(),
        ));
    }
    Ok(())
}

fn is_jpdb_source_url(value: &str, vocabulary_id: u64) -> bool {
    let Ok(url) = Url::parse(value) else {
        return false;
    };
    let has_vocabulary_id = url.path_segments().is_some_and(|segments| {
        let segments: Vec<_> = segments.collect();
        segments
            .windows(2)
            .any(|pair| pair[0] == "vocabulary" && pair[1] == vocabulary_id.to_string())
    });
    url.scheme() == "https"
        && url.username().is_empty()
        && url.password().is_none()
        && has_vocabulary_id
        && url.host_str().is_some_and(|host| {
            host.eq_ignore_ascii_case("jpdb.io")
                || host
                    .to_ascii_lowercase()
                    .strip_suffix(".jpdb.io")
                    .is_some_and(|prefix| !prefix.is_empty())
        })
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

fn decode_png(bytes: &[u8]) -> Result<(u32, u32), String> {
    let decoder =
        image::codecs::png::PngDecoder::with_limits(Cursor::new(bytes), image::Limits::default())
            .map_err(|error| error.to_string())?;
    image::DynamicImage::from_decoder(decoder)
        .map(|image| (image.width(), image.height()))
        .map_err(|error| error.to_string())
}

fn incomplete_evidence(message: impl Into<String>) -> SemanticDecision {
    decision(
        SemanticStatus::Uncertain,
        "pitch_accent_evidence_incomplete",
        "PNG не подтверждён полным source/render evidence",
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
                source_url: "https://jpdb.io/vocabulary/123".into(),
                graph_count: 1,
                render: PitchAccentRenderEvidence {
                    kind: PitchAccentRenderKind::ElementScreenshot,
                    selector: ".pitch-accent-graph".into(),
                    viewport_width: 1280,
                    viewport_height: 900,
                    pixel_width: 2,
                    pixel_height: 2,
                    device_scale_factor: 3.0,
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
        let image = image::RgbaImage::from_pixel(2, 2, image::Rgba([24, 36, 48, 255]));
        let mut output = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(image)
            .write_to(&mut output, image::ImageFormat::Png)
            .unwrap();
        output.into_inner()
    }

    #[test]
    fn pitch_policy_keeps_surface_filename_and_isolates_unsupported_runtime_formats() {
        let policy = PitchAccentDomainPolicy;
        let identity = crate::model::AssetIdentity::new("pitch_accent", "幽霊").unwrap();
        let hash = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

        let png = policy
            .canonical_location(&identity, hash, DetectedFormat::Png)
            .unwrap();
        assert_eq!(png.storage_path, "assets/png/幽霊.png");
        assert_eq!(png.consumer_filename, "幽霊.png");

        let gif = policy
            .canonical_location(&identity, hash, DetectedFormat::Gif)
            .unwrap();
        let identity_hash = sha256_hex("幽霊".as_bytes());
        assert_eq!(
            gif.storage_path,
            format!("assets/unsupported/{identity_hash}-{hash}.gif")
        );
        assert_eq!(gif.consumer_filename, "幽霊.gif");
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
            consumer_filename: "幽霊.png".into(),
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
    }

    #[test]
    fn invalid_png_bytes_are_corrupt_even_with_valid_evidence() {
        let asset = record(Some(valid_metadata("幽霊")));
        let decision = validate(&asset, b"\x89PNG\r\n\x1a\nnot a complete png").unwrap();

        assert_eq!(decision.status, SemanticStatus::Corrupt);
    }
}
