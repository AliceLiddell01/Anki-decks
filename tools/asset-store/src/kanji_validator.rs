//! Pixel-based kanji validation against a pinned, Unicode-addressed reference
//! catalog. The GIF path uses fully composited animation frames and selects the
//! frame with the largest foreground coverage.

use std::collections::BTreeMap;
use std::io::{Cursor, Read};
use std::sync::OnceLock;

use flate2::read::ZlibDecoder;
use image::codecs::gif::GifDecoder;
use image::{AnimationDecoder, DynamicImage, GenericImageView, ImageDecoder, ImageReader, Limits};
use serde_json::json;

use crate::model::{
    AssetRecord, DetectedFormat, SemanticDecision, SemanticStatus, ValidationEvidence,
    ValidatorIdentity,
};
use crate::validation::{SemanticValidator, ValidatorFailure};

const VALIDATOR_ID: &str = "kanjivg-pixel-chamfer";
const VALIDATOR_VERSION: &str = "kanjivg-r20250816-noto-serif-jp-24fc2d26-mask64-v5";
const REFERENCE_DB: &[u8] = include_bytes!("data/kanjivg-r20250816.maskdb.zlib");
const YARXI_FONT_REFERENCE_DB: &[u8] = include_bytes!("data/yarxi-noto-serif-jp-r1.maskdb.zlib");
const YARXI_FONT_REFERENCE_VERSION: &str = "Noto Serif JP /fonts/noto-serif-jp.24fc2d26.ttf sha256=e6cffcd5cae6a298ddfd17173b42d64888913976b2d7053024a9ecf0cf5d3fe8";
const MASK_SIDE: usize = 64;
const MASK_BYTES: usize = MASK_SIDE * MASK_SIDE / 8;
const MAX_MEDIA_BYTES: usize = 8 * 1024 * 1024;
const MAX_DIMENSION: u32 = 2048;
const MAX_GIF_FRAMES: usize = 512;
const MIN_FOREGROUND_CONTRAST: f32 = 30.0;

// Frozen before the 50-item acceptance run. Confident pilot positives topped out
// at distance 0.0178 with margin 0.0057. `元` measured 0.0039516 and stays
// uncertain at this threshold; do not lower it to admit that candidate. A mismatch
// is rejected only when expected identity is materially farther; see README.
const VERIFIED_MAX_DISTANCE: f32 = 0.020;
const VERIFIED_MIN_MARGIN: f32 = 0.004;
const REJECT_MIN_GAP: f32 = 0.015;

type Mask = [u8; MASK_BYTES];
type ReferenceMap = BTreeMap<char, ReferenceTemplate>;
type FontReferenceMap = BTreeMap<char, ReferenceTemplate>;

#[derive(Debug, Clone)]
struct ReferenceTemplate {
    mask: Mask,
    // One unit is 1/16 source pixel; quantization keeps the immutable KanjiVG
    // catalog near 52 MiB instead of retaining 100+ MiB of f32 distance fields.
    distances_sixteenths: Box<[u16]>,
}

static REFERENCE_CATALOG: OnceLock<Result<ReferenceMap, String>> = OnceLock::new();
static YARXI_FONT_REFERENCE_CATALOG: OnceLock<Result<FontReferenceMap, String>> = OnceLock::new();

#[derive(Debug, Clone, PartialEq)]
struct RasterEvidence {
    mask: Mask,
    width: u32,
    height: u32,
    format: DetectedFormat,
    frame_count: usize,
    representative_frame: usize,
    foreground_pixels: usize,
    background_luma: f32,
    foreground_polarity: ForegroundPolarity,
    excluded_border_components: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ForegroundPolarity {
    DarkOnLight,
    LightOnDark,
}

impl ForegroundPolarity {
    const fn as_str(self) -> &'static str {
        match self {
            Self::DarkOnLight => "dark_on_light",
            Self::LightOnDark => "light_on_dark",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
struct NormalizedMask {
    mask: Mask,
    foreground_pixels: usize,
    background_luma: f32,
    foreground_polarity: ForegroundPolarity,
    excluded_border_components: usize,
}

/// Production validator for PNG/GIF kanji drawings.
#[derive(Debug, Default, Clone, Copy)]
pub struct KanjiImageValidator;

impl KanjiImageValidator {
    pub const fn new() -> Self {
        Self
    }

    /// Returns the current stable identity used in manifest decisions.
    pub fn validator_identity() -> ValidatorIdentity {
        ValidatorIdentity {
            id: VALIDATOR_ID.to_owned(),
            version: VALIDATOR_VERSION.to_owned(),
        }
    }
}

impl SemanticValidator for KanjiImageValidator {
    fn identity(&self) -> ValidatorIdentity {
        Self::validator_identity()
    }

    fn validate(
        &self,
        asset: &AssetRecord,
        bytes: &mut dyn Read,
    ) -> Result<SemanticDecision, ValidatorFailure> {
        let mut contents = Vec::new();
        bytes
            .take((MAX_MEDIA_BYTES + 1) as u64)
            .read_to_end(&mut contents)
            .map_err(|error| ValidatorFailure::new("media_read_failed", error.to_string()))?;
        if contents.len() > MAX_MEDIA_BYTES {
            return Ok(decision(
                SemanticStatus::Corrupt,
                "media_too_large",
                "image exceeds the 8 MiB decode limit",
                json!({ "max_bytes": MAX_MEDIA_BYTES }),
            ));
        }
        let raster = match decode_media(&contents) {
            Ok(raster) => raster,
            Err(reason) => {
                return Ok(decision(
                    SemanticStatus::Corrupt,
                    "image_decode",
                    "actual bytes are not a supported, non-empty PNG/GIF image",
                    json!({ "reason": reason }),
                ));
            }
        };
        if raster.format != asset.format {
            return Ok(decision(
                SemanticStatus::Corrupt,
                "format_mismatch",
                "manifest format does not match the media signature",
                json!({
                    "manifest_format": asset.format,
                    "actual_format": raster.format,
                }),
            ));
        }
        let expected = match one_character(&asset.identity.key) {
            Some(character) => character,
            None => {
                return Ok(decision(
                    SemanticStatus::Uncertain,
                    "identity_not_single_character",
                    "semantic validator requires one Unicode scalar as identity",
                    json!({ "identity": asset.identity.key }),
                ));
            }
        };
        let catalog = match REFERENCE_CATALOG.get_or_init(load_reference_catalog) {
            Ok(catalog) => catalog,
            Err(reason) => {
                return Ok(decision(
                    SemanticStatus::Uncertain,
                    "reference_unavailable",
                    "pinned KanjiVG template catalog is unavailable or invalid",
                    json!({ "reason": reason }),
                ));
            }
        };
        let Some(expected_template) = catalog.get(&expected) else {
            return Ok(decision(
                SemanticStatus::Uncertain,
                "reference_character_missing",
                "the requested character is not present in the pinned reference catalog",
                json!({ "expected": expected.to_string() }),
            ));
        };

        let font_catalog = if raster.foreground_polarity == ForegroundPolarity::LightOnDark {
            match YARXI_FONT_REFERENCE_CATALOG.get_or_init(load_yarxi_font_reference_catalog) {
                Ok(catalog) => Some(catalog),
                Err(reason) => {
                    return Ok(decision(
                        SemanticStatus::Uncertain,
                        "font_reference_unavailable",
                        "pinned dark-tile font reference catalog is unavailable or invalid",
                        json!({ "reason": reason }),
                    ));
                }
            }
        } else {
            None
        };
        let (
            expected_distance,
            kanjivg_expected_distance,
            font_expected_distance,
            nearest_other,
            other_distance,
            margin,
        ) = compare_against_catalog(
            &raster.mask,
            expected,
            expected_template,
            font_catalog,
            catalog,
        );
        let status = classify_distance(expected_distance, other_distance, margin);
        Ok(decision(
            status,
            "pixel_reference_comparison",
            match status {
                SemanticStatus::Verified => {
                    "expected Unicode template is a confident nearest pixel match"
                }
                SemanticStatus::Rejected => {
                    "another Unicode template is materially closer to the image"
                }
                SemanticStatus::Uncertain => {
                    "pixel evidence does not meet the registered confidence thresholds"
                }
                SemanticStatus::Corrupt => unreachable!(),
            },
            json!({
                "expected": expected.to_string(),
                "expected_distance": expected_distance,
                "kanjivg_expected_distance": kanjivg_expected_distance,
                "noto_font_expected_distance": font_expected_distance,
                "nearest_other": nearest_other.to_string(),
                "nearest_other_distance": other_distance,
                "nearest_margin": margin,
                "max_distance": VERIFIED_MAX_DISTANCE,
                "minimum_margin": VERIFIED_MIN_MARGIN,
                "rejection_gap": REJECT_MIN_GAP,
                "dimensions": [raster.width, raster.height],
                "actual_format": raster.format,
                "frame_count": raster.frame_count,
                "representative_frame": raster.representative_frame,
                "foreground_pixels": raster.foreground_pixels,
                "background_luma": raster.background_luma,
                "foreground_polarity": raster.foreground_polarity.as_str(),
                "excluded_border_components": raster.excluded_border_components,
                "reference_version": "KanjiVG r20250816 @ bd13ffbcc9d85cb86ae98bbbf001d9069220b901",
                "font_reference_version": font_catalog.map(|_| YARXI_FONT_REFERENCE_VERSION),
                "comparison": "symmetric chamfer distance on centered 64x64 binary foreground masks; candidate downsampling samples pixel centers; expected and nearest-other distances search every applicable pinned pixel-reference catalog",
                "segmentation": "corner-median background estimate, adaptive polarity, edge/frame component suppression",
            }),
        ))
    }
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

fn one_character(value: &str) -> Option<char> {
    let mut chars = value.chars();
    let first = chars.next()?;
    chars.next().is_none().then_some(first)
}

fn classify_distance(expected_distance: f32, other_distance: f32, margin: f32) -> SemanticStatus {
    if expected_distance <= VERIFIED_MAX_DISTANCE && margin >= VERIFIED_MIN_MARGIN {
        SemanticStatus::Verified
    } else if expected_distance - other_distance >= REJECT_MIN_GAP {
        SemanticStatus::Rejected
    } else {
        SemanticStatus::Uncertain
    }
}

fn decode_media(bytes: &[u8]) -> Result<RasterEvidence, String> {
    if bytes.is_empty() || bytes.len() > MAX_MEDIA_BYTES {
        return Err(format!(
            "byte length {} is outside accepted range",
            bytes.len()
        ));
    }
    match DetectedFormat::from_signature(bytes) {
        DetectedFormat::Png => decode_png(bytes),
        DetectedFormat::Gif => decode_gif(bytes),
        actual => Err(format!("unsupported magic-byte format: {actual:?}")),
    }
}

fn decode_png(bytes: &[u8]) -> Result<RasterEvidence, String> {
    let mut reader = ImageReader::with_format(Cursor::new(bytes), image::ImageFormat::Png);
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_DIMENSION);
    limits.max_image_height = Some(MAX_DIMENSION);
    limits.max_alloc = Some(32 * 1024 * 1024);
    reader.limits(limits);
    let image = reader
        .decode()
        .map_err(|error| format!("PNG decode: {error}"))?;
    let (width, height) = image.dimensions();
    let normalized = image_to_mask(&image)?;
    let foreground_pixels = normalized.foreground_pixels;
    if foreground_pixels < 24 {
        return Err("decoded PNG contains too little foreground".into());
    }
    Ok(RasterEvidence {
        mask: normalized.mask,
        width,
        height,
        format: DetectedFormat::Png,
        frame_count: 1,
        representative_frame: 0,
        foreground_pixels,
        background_luma: normalized.background_luma,
        foreground_polarity: normalized.foreground_polarity,
        excluded_border_components: normalized.excluded_border_components,
    })
}

fn decode_gif(bytes: &[u8]) -> Result<RasterEvidence, String> {
    let mut decoder =
        GifDecoder::new(Cursor::new(bytes)).map_err(|error| format!("GIF header: {error}"))?;
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_DIMENSION);
    limits.max_image_height = Some(MAX_DIMENSION);
    limits.max_alloc = Some(32 * 1024 * 1024);
    decoder
        .set_limits(limits)
        .map_err(|error| format!("GIF dimensions: {error}"))?;
    let (width, height) = decoder.dimensions();
    if width == 0 || height == 0 {
        return Err("GIF has zero dimensions".into());
    }
    let mut count = 0usize;
    let mut best: Option<(usize, NormalizedMask)> = None;
    for frame in decoder.into_frames() {
        let frame = frame.map_err(|error| format!("GIF frame decode: {error}"))?;
        count += 1;
        if count > MAX_GIF_FRAMES {
            return Err(format!("GIF frame count exceeds {MAX_GIF_FRAMES}"));
        }
        let buffer = frame.into_buffer();
        let normalized = rgba_to_mask(&buffer)?;
        let foreground_pixels = normalized.foreground_pixels;
        if best
            .as_ref()
            .is_none_or(|(_, current)| foreground_pixels > current.foreground_pixels)
        {
            best = Some((count - 1, normalized));
        }
    }
    let Some((representative_frame, normalized)) = best else {
        return Err("GIF contains no decodable frames".into());
    };
    let foreground_pixels = normalized.foreground_pixels;
    if foreground_pixels < 24 {
        return Err("decoded GIF contains no substantive drawing".into());
    }
    Ok(RasterEvidence {
        mask: normalized.mask,
        width,
        height,
        format: DetectedFormat::Gif,
        frame_count: count,
        representative_frame,
        foreground_pixels,
        background_luma: normalized.background_luma,
        foreground_polarity: normalized.foreground_polarity,
        excluded_border_components: normalized.excluded_border_components,
    })
}

fn image_to_mask(image: &DynamicImage) -> Result<NormalizedMask, String> {
    rgba_to_mask(&image.to_rgba8())
}

fn rgba_to_mask(image: &image::RgbaImage) -> Result<NormalizedMask, String> {
    let (width, height) = image.dimensions();
    if width == 0 || height == 0 || width > MAX_DIMENSION || height > MAX_DIMENSION {
        return Err(format!("invalid dimensions {width}x{height}"));
    }

    let background_luma = estimate_background_luma(image);
    let foreground_polarity = if background_luma >= 128.0 {
        ForegroundPolarity::DarkOnLight
    } else {
        ForegroundPolarity::LightOnDark
    };
    let mut foreground = Vec::with_capacity((width * height) as usize);
    for pixel in image.pixels() {
        let pixel_luma = composited_luma(pixel.0);
        let contrast = match foreground_polarity {
            ForegroundPolarity::DarkOnLight => background_luma - pixel_luma,
            ForegroundPolarity::LightOnDark => pixel_luma - background_luma,
        };
        foreground.push(u8::from(contrast >= MIN_FOREGROUND_CONTRAST));
    }
    let excluded_border_components = suppress_border_components(&mut foreground, width, height);

    let mut bounds: Option<(u32, u32, u32, u32)> = None;
    for (index, &is_foreground) in foreground.iter().enumerate() {
        if is_foreground == 3 {
            let x = (index as u32) % width;
            let y = (index as u32) / width;
            bounds = Some(match bounds {
                Some((left, top, right, bottom)) => {
                    (left.min(x), top.min(y), right.max(x), bottom.max(y))
                }
                None => (x, y, x, y),
            });
        }
    }
    let Some((left, top, right, bottom)) = bounds else {
        return Err("image contains no foreground after background and border suppression".into());
    };
    let source_width = right - left + 1;
    let source_height = bottom - top + 1;
    let interior = (MASK_SIDE - 8) as u32;
    let scale = (interior as f32 / source_width as f32).min(interior as f32 / source_height as f32);
    let scaled_width = ((source_width as f32 * scale).round() as usize).clamp(1, MASK_SIDE - 8);
    let scaled_height = ((source_height as f32 * scale).round() as usize).clamp(1, MASK_SIDE - 8);
    let offset_x = (MASK_SIDE - scaled_width) / 2;
    let offset_y = (MASK_SIDE - scaled_height) / 2;
    let mut mask = [0u8; MASK_BYTES];
    for y in 0..scaled_height {
        for x in 0..scaled_width {
            let source_x = left + center_sample_index(x, source_width, scaled_width);
            let source_y = top + center_sample_index(y, source_height, scaled_height);
            let source_index = (source_y * width + source_x) as usize;
            if foreground[source_index] == 3 {
                set_ink(&mut mask, offset_x + x, offset_y + y);
            }
        }
    }
    let foreground_pixels = count_ink(&mask);
    if foreground_pixels == 0 {
        return Err("image is blank after normalization".into());
    }
    Ok(NormalizedMask {
        mask,
        foreground_pixels,
        background_luma,
        foreground_polarity,
        excluded_border_components,
    })
}

fn composited_luma(pixel: [u8; 4]) -> f32 {
    let alpha = f32::from(pixel[3]) / 255.0;
    let luma =
        0.2126 * f32::from(pixel[0]) + 0.7152 * f32::from(pixel[1]) + 0.0722 * f32::from(pixel[2]);
    alpha.mul_add(luma, (1.0 - alpha) * 255.0)
}

fn estimate_background_luma(image: &image::RgbaImage) -> f32 {
    let (width, height) = image.dimensions();
    let mut samples = Vec::new();
    for (x, y) in corner_patch_coordinates(width) {
        for (corner_x, corner_y) in [
            (x, y),
            (width - 1 - x, y),
            (x, height - 1 - y),
            (width - 1 - x, height - 1 - y),
        ] {
            samples.push(composited_luma(image.get_pixel(corner_x, corner_y).0));
        }
    }
    samples.sort_by(f32::total_cmp);
    samples[samples.len() / 2]
}

fn corner_patch_coordinates(length: u32) -> Vec<(u32, u32)> {
    let inset = (length / 16).min(length.saturating_sub(1) / 2);
    let patch = (length / 24)
        .clamp(1, 5)
        .min(length.saturating_sub(2 * inset).max(1));
    let mut coordinates = Vec::with_capacity((patch * patch) as usize);
    for y in 0..patch {
        for x in 0..patch {
            coordinates.push((inset + x, inset + y));
        }
    }
    coordinates
}

/// Discards connected foreground components that touch the raster edge or span
/// nearly the complete raster in both dimensions. This removes CSS borders while
/// preserving the disconnected strokes inside the Yarxi tile.
fn suppress_border_components(foreground: &mut [u8], width: u32, height: u32) -> usize {
    let mut queue = Vec::<u32>::new();
    let mut excluded_components = 0;
    let near_x = (width / 20).max(1);
    let near_y = (height / 20).max(1);
    for start in 0..foreground.len() {
        if foreground[start] != 1 {
            continue;
        }
        foreground[start] = 2;
        queue.clear();
        queue.push(start as u32);
        let (mut left, mut top, mut right, mut bottom) = (width, height, 0, 0);
        let mut touches_edge = false;
        let mut cursor = 0;
        while cursor < queue.len() {
            let index = queue[cursor] as usize;
            cursor += 1;
            let x = (index as u32) % width;
            let y = (index as u32) / width;
            left = left.min(x);
            top = top.min(y);
            right = right.max(x);
            bottom = bottom.max(y);
            touches_edge |= x == 0 || y == 0 || x + 1 == width || y + 1 == height;
            for neighbor_y in y.saturating_sub(1)..=(y + 1).min(height - 1) {
                for neighbor_x in x.saturating_sub(1)..=(x + 1).min(width - 1) {
                    let neighbor = (neighbor_y * width + neighbor_x) as usize;
                    if foreground[neighbor] == 1 {
                        foreground[neighbor] = 2;
                        queue.push(neighbor as u32);
                    }
                }
            }
        }
        let frame_component = left <= near_x
            && top <= near_y
            && right + near_x + 1 >= width
            && bottom + near_y + 1 >= height;
        if touches_edge || frame_component {
            excluded_components += 1;
        } else {
            for &index in &queue {
                foreground[index as usize] = 3;
            }
        }
    }
    excluded_components
}

fn set_ink(mask: &mut Mask, x: usize, y: usize) {
    let index = y * MASK_SIDE + x;
    mask[index / 8] |= 1 << (index % 8);
}

fn get_ink(mask: &Mask, x: usize, y: usize) -> bool {
    let index = y * MASK_SIDE + x;
    mask[index / 8] & (1 << (index % 8)) != 0
}

fn count_ink(mask: &Mask) -> usize {
    mask.iter().map(|byte| byte.count_ones() as usize).sum()
}

fn center_sample_index(output_index: usize, source_size: u32, output_size: usize) -> u32 {
    let numerator = (2 * output_index as u64 + 1) * u64::from(source_size);
    let denominator = 2 * output_size as u64;
    (numerator / denominator).min(u64::from(source_size - 1)) as u32
}

fn load_reference_catalog() -> Result<ReferenceMap, String> {
    let mut decoder = ZlibDecoder::new(REFERENCE_DB);
    let mut bytes = Vec::new();
    decoder
        .read_to_end(&mut bytes)
        .map_err(|error| format!("reference zlib decode: {error}"))?;
    if bytes.len() < 8 || &bytes[..4] != b"KJ64" {
        return Err("reference database magic/version mismatch".into());
    }
    let count = u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize;
    let entry_size = 4 + MASK_BYTES;
    if count > 20_000 || bytes.len() != 8 + count * entry_size {
        return Err("reference database length/count mismatch".into());
    }
    let mut map = BTreeMap::new();
    for entry in bytes[8..].chunks_exact(entry_size) {
        let codepoint = u32::from_le_bytes(entry[..4].try_into().unwrap());
        let character = char::from_u32(codepoint)
            .ok_or_else(|| format!("invalid reference Unicode scalar U+{codepoint:X}"))?;
        let mask: Mask = entry[4..]
            .try_into()
            .map_err(|_| "reference mask has an invalid byte length".to_owned())?;
        if count_ink(&mask) == 0 {
            return Err(format!(
                "empty or duplicate reference template for {character}"
            ));
        }
        let distances_sixteenths = distance_transform(&mask)
            .into_iter()
            .map(|distance| (distance * 16.0).round().clamp(0.0, f32::from(u16::MAX)) as u16)
            .collect::<Vec<_>>()
            .into_boxed_slice();
        if map
            .insert(
                character,
                ReferenceTemplate {
                    mask,
                    distances_sixteenths,
                },
            )
            .is_some()
        {
            return Err(format!("duplicate reference template for {character}"));
        }
    }
    Ok(map)
}

fn load_yarxi_font_reference_catalog() -> Result<FontReferenceMap, String> {
    let mut decoder = ZlibDecoder::new(YARXI_FONT_REFERENCE_DB);
    let mut bytes = Vec::new();
    decoder
        .read_to_end(&mut bytes)
        .map_err(|error| format!("Noto Serif JP reference zlib decode: {error}"))?;
    if bytes.len() < 8 || &bytes[..4] != b"YJ64" {
        return Err("Noto Serif JP reference database magic/version mismatch".into());
    }
    let count = u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize;
    let entry_size = 4 + MASK_BYTES;
    if !(1000..=20_000).contains(&count) || bytes.len() != 8 + count * entry_size {
        return Err("Noto Serif JP reference database length/count mismatch".into());
    }
    let mut map = BTreeMap::new();
    for entry in bytes[8..].chunks_exact(entry_size) {
        let codepoint = u32::from_le_bytes(entry[..4].try_into().unwrap());
        let character = char::from_u32(codepoint)
            .ok_or_else(|| format!("invalid Noto Serif JP Unicode scalar U+{codepoint:X}"))?;
        let mask: Mask = entry[4..]
            .try_into()
            .map_err(|_| "Noto Serif JP mask has an invalid byte length".to_owned())?;
        if count_ink(&mask) == 0 {
            return Err(format!("empty Noto Serif JP template for {character}"));
        }
        let distances_sixteenths = distance_transform(&mask)
            .into_iter()
            .map(|distance| (distance * 16.0).round().clamp(0.0, f32::from(u16::MAX)) as u16)
            .collect::<Vec<_>>()
            .into_boxed_slice();
        if map
            .insert(
                character,
                ReferenceTemplate {
                    mask,
                    distances_sixteenths,
                },
            )
            .is_some()
        {
            return Err(format!(
                "empty or duplicate Noto Serif JP template for {character}"
            ));
        }
    }
    Ok(map)
}

fn compare_against_catalog(
    candidate: &Mask,
    expected: char,
    expected_template: &ReferenceTemplate,
    font_catalog: Option<&FontReferenceMap>,
    catalog: &ReferenceMap,
) -> (f32, f32, Option<f32>, char, f32, f32) {
    let candidate_distances = distance_transform(candidate);
    let kanjivg_expected_distance = symmetric_distance_quantized(
        candidate,
        &candidate_distances,
        &expected_template.mask,
        &expected_template.distances_sixteenths,
    );
    let font_template = font_catalog.and_then(|references| references.get(&expected));
    let font_expected_distance = font_template.map(|template| {
        symmetric_distance_quantized(
            candidate,
            &candidate_distances,
            &template.mask,
            &template.distances_sixteenths,
        )
    });
    let expected_distance = font_expected_distance
        .map(|font_distance| font_distance.min(kanjivg_expected_distance))
        .unwrap_or(kanjivg_expected_distance);
    let mut nearest: Option<(char, f32)> = None;
    for (&character, template) in catalog {
        if character == expected {
            continue;
        }
        let distance = symmetric_distance_quantized(
            candidate,
            &candidate_distances,
            &template.mask,
            &template.distances_sixteenths,
        );
        if nearest.is_none_or(|(_, current)| distance < current) {
            nearest = Some((character, distance));
        }
    }
    if let Some(font_catalog) = font_catalog {
        for (&character, template) in font_catalog {
            if character == expected {
                continue;
            }
            let distance = symmetric_distance_quantized(
                candidate,
                &candidate_distances,
                &template.mask,
                &template.distances_sixteenths,
            );
            if nearest.is_none_or(|(_, current)| distance < current) {
                nearest = Some((character, distance));
            }
        }
    }
    let (nearest_other, other_distance) = nearest.unwrap_or(('?', f32::INFINITY));
    (
        expected_distance,
        kanjivg_expected_distance,
        font_expected_distance,
        nearest_other,
        other_distance,
        other_distance - expected_distance,
    )
}

fn symmetric_distance_quantized(
    candidate: &Mask,
    candidate_distances: &[f32],
    reference: &Mask,
    reference_distances_sixteenths: &[u16],
) -> f32 {
    let mut candidate_to_reference = 0.0;
    let mut reference_to_candidate = 0.0;
    let (mut candidate_count, mut reference_count) = (0u32, 0u32);
    for y in 0..MASK_SIDE {
        for x in 0..MASK_SIDE {
            let index = y * MASK_SIDE + x;
            if get_ink(candidate, x, y) {
                candidate_to_reference += f32::from(reference_distances_sixteenths[index]) / 16.0;
                candidate_count += 1;
            }
            if get_ink(reference, x, y) {
                reference_to_candidate += candidate_distances[index];
                reference_count += 1;
            }
        }
    }
    if candidate_count == 0 || reference_count == 0 {
        return f32::INFINITY;
    }
    (candidate_to_reference / candidate_count as f32
        + reference_to_candidate / reference_count as f32)
        / (2.0 * MASK_SIDE as f32)
}

#[cfg(test)]
fn symmetric_distance(
    candidate: &Mask,
    candidate_distances: &[f32],
    reference: &Mask,
    reference_distances: &[f32],
) -> f32 {
    let mut candidate_to_reference = 0.0;
    let mut reference_to_candidate = 0.0;
    let (mut candidate_count, mut reference_count) = (0u32, 0u32);
    for y in 0..MASK_SIDE {
        for x in 0..MASK_SIDE {
            let index = y * MASK_SIDE + x;
            if get_ink(candidate, x, y) {
                candidate_to_reference += reference_distances[index];
                candidate_count += 1;
            }
            if get_ink(reference, x, y) {
                reference_to_candidate += candidate_distances[index];
                reference_count += 1;
            }
        }
    }
    if candidate_count == 0 || reference_count == 0 {
        return f32::INFINITY;
    }
    (candidate_to_reference / candidate_count as f32
        + reference_to_candidate / reference_count as f32)
        / (2.0 * MASK_SIDE as f32)
}

fn distance_transform(mask: &Mask) -> Vec<f32> {
    let mut distances = vec![f32::INFINITY; MASK_SIDE * MASK_SIDE];
    for y in 0..MASK_SIDE {
        for x in 0..MASK_SIDE {
            if get_ink(mask, x, y) {
                distances[y * MASK_SIDE + x] = 0.0;
            }
        }
    }
    let diagonal = std::f32::consts::SQRT_2;
    for y in 0..MASK_SIDE {
        for x in 0..MASK_SIDE {
            relax(&mut distances, x, y, x.wrapping_sub(1), y, 1.0);
            relax(&mut distances, x, y, x, y.wrapping_sub(1), 1.0);
            relax(
                &mut distances,
                x,
                y,
                x.wrapping_sub(1),
                y.wrapping_sub(1),
                diagonal,
            );
            relax(&mut distances, x, y, x + 1, y.wrapping_sub(1), diagonal);
        }
    }
    for y in (0..MASK_SIDE).rev() {
        for x in (0..MASK_SIDE).rev() {
            relax(&mut distances, x, y, x + 1, y, 1.0);
            relax(&mut distances, x, y, x, y + 1, 1.0);
            relax(&mut distances, x, y, x + 1, y + 1, diagonal);
            relax(&mut distances, x, y, x.wrapping_sub(1), y + 1, diagonal);
        }
    }
    distances
}

fn relax(distances: &mut [f32], x: usize, y: usize, other_x: usize, other_y: usize, weight: f32) {
    if other_x >= MASK_SIDE || other_y >= MASK_SIDE {
        return;
    }
    let index = y * MASK_SIDE + x;
    let other = distances[other_y * MASK_SIDE + other_x] + weight;
    distances[index] = distances[index].min(other);
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::codecs::gif::GifEncoder;
    use image::{Delay, Frame, ImageBuffer, ImageFormat};

    fn line_mask(x: usize) -> Mask {
        let mut mask = [0; MASK_BYTES];
        for y in 10..54 {
            for dx in 0..3 {
                set_ink(&mut mask, x + dx, y);
            }
        }
        mask
    }

    fn reference(character: char) -> Mask {
        test_catalog()
            .get(&character)
            .expect("reference character exists")
            .mask
    }

    fn test_catalog() -> &'static ReferenceMap {
        REFERENCE_CATALOG
            .get_or_init(load_reference_catalog)
            .as_ref()
            .expect("pinned reference catalog decodes")
    }

    fn rgba_for(mask: &Mask) -> image::RgbaImage {
        ImageBuffer::from_fn(MASK_SIDE as u32, MASK_SIDE as u32, |x, y| {
            if get_ink(mask, x as usize, y as usize) {
                image::Rgba([0, 0, 0, 255])
            } else {
                image::Rgba([255, 255, 255, 255])
            }
        })
    }

    fn dark_tile_for(mask: &Mask, with_border: bool) -> image::RgbaImage {
        const SIDE: u32 = 128;
        const TILE_OFFSET: u32 = 16;
        const TILE_CONTENT_SIDE: u32 = 96;
        const DARK_BACKGROUND: image::Rgba<u8> = image::Rgba([36, 39, 41, 255]);
        const LIGHT_GLYPH: image::Rgba<u8> = image::Rgba([245, 246, 247, 255]);
        const LIGHT_BORDER: image::Rgba<u8> = image::Rgba([126, 137, 141, 255]);
        let mut image = ImageBuffer::from_pixel(SIDE, SIDE, DARK_BACKGROUND);
        for y in 0..TILE_CONTENT_SIDE {
            for x in 0..TILE_CONTENT_SIDE {
                let source_x =
                    center_sample_index(x as usize, MASK_SIDE as u32, TILE_CONTENT_SIDE as usize)
                        as usize;
                let source_y =
                    center_sample_index(y as usize, MASK_SIDE as u32, TILE_CONTENT_SIDE as usize)
                        as usize;
                if get_ink(mask, source_x, source_y) {
                    image.put_pixel(x + TILE_OFFSET, y + TILE_OFFSET, LIGHT_GLYPH);
                }
            }
        }
        if with_border {
            for edge in 0..2 {
                for offset in 0..SIDE {
                    image.put_pixel(offset, edge, LIGHT_BORDER);
                    image.put_pixel(offset, SIDE - 1 - edge, LIGHT_BORDER);
                    image.put_pixel(edge, offset, LIGHT_BORDER);
                    image.put_pixel(SIDE - 1 - edge, offset, LIGHT_BORDER);
                }
            }
        }
        image
    }

    fn rendered_dark_tile_for(mask: &Mask, with_border: bool) -> image::RgbaImage {
        const SIDE: u32 = 174;
        const TILE_OFFSET: u32 = 16;
        const TILE_CONTENT_SIDE: u32 = 142;
        const DARK_BACKGROUND: image::Rgba<u8> = image::Rgba([34, 34, 34, 255]);
        const LIGHT_GLYPH: image::Rgba<u8> = image::Rgba([253, 253, 253, 255]);
        const LIGHT_BORDER: image::Rgba<u8> = image::Rgba([124, 124, 124, 255]);
        let mut image = ImageBuffer::from_pixel(SIDE, SIDE, DARK_BACKGROUND);
        for y in 0..TILE_CONTENT_SIDE {
            for x in 0..TILE_CONTENT_SIDE {
                let source_x =
                    center_sample_index(x as usize, MASK_SIDE as u32, TILE_CONTENT_SIDE as usize)
                        as usize;
                let source_y =
                    center_sample_index(y as usize, MASK_SIDE as u32, TILE_CONTENT_SIDE as usize)
                        as usize;
                if get_ink(mask, source_x, source_y) {
                    image.put_pixel(x + TILE_OFFSET, y + TILE_OFFSET, LIGHT_GLYPH);
                }
            }
        }
        if with_border {
            for edge in 0..2 {
                for offset in 0..SIDE {
                    image.put_pixel(offset, edge, LIGHT_BORDER);
                    image.put_pixel(offset, SIDE - 1 - edge, LIGHT_BORDER);
                    image.put_pixel(edge, offset, LIGHT_BORDER);
                    image.put_pixel(SIDE - 1 - edge, offset, LIGHT_BORDER);
                }
            }
        }
        image
    }

    fn random_light_on_dark_tile() -> image::RgbaImage {
        let dark = image::Rgba([36, 39, 41, 255]);
        let light = image::Rgba([245, 246, 247, 255]);
        let mut state = 0x7a31_8d2bu32;
        ImageBuffer::from_fn(128, 128, |x, y| {
            if x < 8 || y < 8 || x >= 120 || y >= 120 {
                return dark;
            }
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            if state >> 28 < 6 { light } else { dark }
        })
    }

    fn png_for_image(image: image::RgbaImage) -> Vec<u8> {
        let mut cursor = Cursor::new(Vec::new());
        DynamicImage::ImageRgba8(image)
            .write_to(&mut cursor, ImageFormat::Png)
            .expect("synthetic PNG encodes");
        cursor.into_inner()
    }

    fn png_for(mask: &Mask) -> Vec<u8> {
        png_for_image(rgba_for(mask))
    }

    fn gif_for(frames: Vec<Mask>) -> Vec<u8> {
        let encoded_frames = frames
            .into_iter()
            .map(|mask| Frame::from_parts(rgba_for(&mask), 0, 0, Delay::from_numer_denom_ms(80, 1)))
            .collect::<Vec<_>>();
        let mut cursor = Cursor::new(Vec::new());
        {
            let mut encoder = GifEncoder::new(&mut cursor);
            encoder
                .encode_frames(encoded_frames)
                .expect("synthetic animated GIF encodes");
        }
        cursor.into_inner()
    }

    fn semantic_status_for(candidate: &Mask, expected: char) -> SemanticStatus {
        let catalog = test_catalog();
        let expected_template = catalog.get(&expected).expect("expected template exists");
        let (distance, _, _, _, other, margin) =
            compare_against_catalog(candidate, expected, expected_template, None, catalog);
        classify_distance(distance, other, margin)
    }

    #[test]
    fn identical_and_distant_masks_have_ordered_distances() {
        let expected = line_mask(20);
        let close = line_mask(21);
        let far = line_mask(34);
        let close_distances = distance_transform(&close);
        let far_distances = distance_transform(&far);
        let expected_distances = distance_transform(&expected);
        assert_eq!(
            symmetric_distance(
                &expected,
                &expected_distances,
                &expected,
                &expected_distances
            ),
            0.0
        );
        assert!(
            symmetric_distance(&expected, &expected_distances, &close, &close_distances)
                < symmetric_distance(&expected, &expected_distances, &far, &far_distances)
        );
        assert!(far_distances.iter().all(|distance| distance.is_finite()));
        assert!(close_distances.iter().all(|distance| distance.is_finite()));
    }

    #[test]
    fn pixel_center_sampling_maps_odd_and_even_dimensions_consistently() {
        let map = |source_size, output_size| {
            (0..output_size)
                .map(|index| center_sample_index(index, source_size, output_size))
                .collect::<Vec<_>>()
        };

        assert_eq!(map(5, 2), [1, 3]);
        assert_eq!(map(4, 3), [0, 2, 3]);
        assert_eq!(map(2, 5), [0, 0, 1, 1, 1]);
    }

    #[test]
    fn blank_and_wrong_magic_bytes_are_technical_corruption() {
        assert!(decode_media(b"").is_err());
        assert!(decode_media(b"GIF89a but not a valid frame stream").is_err());
        assert!(
            decode_media(&png_for_image(ImageBuffer::from_pixel(
                128,
                128,
                image::Rgba([36, 39, 41, 255]),
            )))
            .is_err()
        );
    }

    #[test]
    fn light_background_dark_glyph_png_and_animated_gif_use_full_frame() {
        let expected = reference('元');
        let png = decode_media(&png_for(&expected)).expect("PNG decodes");
        assert_eq!(png.format, DetectedFormat::Png);
        assert_eq!(png.foreground_polarity, ForegroundPolarity::DarkOnLight);
        assert_eq!(
            semantic_status_for(&png.mask, '元'),
            SemanticStatus::Verified
        );

        let mut early = expected;
        for y in 0..MASK_SIDE {
            if y > 27 {
                for x in 0..MASK_SIDE {
                    let index = y * MASK_SIDE + x;
                    early[index / 8] &= !(1 << (index % 8));
                }
            }
        }
        let gif = gif_for(vec![early, expected]);
        let animation = decode_media(&gif).expect("animated GIF decodes");
        assert_eq!(animation.frame_count, 2);
        assert_eq!(animation.representative_frame, 1);
        assert_eq!(animation.mask, expected);
        assert_eq!(
            semantic_status_for(&animation.mask, '元'),
            SemanticStatus::Verified
        );
        assert_ne!(semantic_status_for(&early, '元'), SemanticStatus::Verified);
    }

    #[test]
    fn dark_background_light_glyph_is_normalized_and_verified() {
        let expected = reference('元');
        let raster = decode_media(&png_for_image(dark_tile_for(&expected, false)))
            .expect("dark-theme tile decodes");
        assert_eq!(raster.foreground_polarity, ForegroundPolarity::LightOnDark);
        assert!((raster.background_luma - 38.0).abs() < 2.0);
        assert_eq!(
            semantic_status_for(&raster.mask, '元'),
            SemanticStatus::Verified
        );
    }

    #[test]
    fn light_frame_on_dark_tile_is_excluded_from_glyph_mask() {
        let expected = reference('元');
        let plain = rgba_to_mask(&dark_tile_for(&expected, false)).expect("plain dark tile");
        let framed = rgba_to_mask(&dark_tile_for(&expected, true)).expect("framed dark tile");
        assert_eq!(framed.excluded_border_components, 1);
        assert_eq!(framed.mask, plain.mask);
        assert_eq!(
            semantic_status_for(&framed.mask, '元'),
            SemanticStatus::Verified
        );
    }

    #[test]
    fn dark_tile_confusable_identity_remains_fail_closed() {
        let wrong_identity_pixels = reference('末');
        let raster = decode_media(&png_for_image(dark_tile_for(&wrong_identity_pixels, true)))
            .expect("confusable dark tile decodes");
        assert_ne!(
            semantic_status_for(&raster.mask, '未'),
            SemanticStatus::Verified
        );
    }

    #[test]
    fn dark_yarxi_font_reference_verifies_pixels_but_not_a_wrong_identity() {
        let font_catalog = YARXI_FONT_REFERENCE_CATALOG
            .get_or_init(load_yarxi_font_reference_catalog)
            .as_ref()
            .expect("pinned Noto Serif JP reference catalog decodes");
        let font_mask = font_catalog
            .get(&'心')
            .expect("pinned Noto Serif JP has a 心 template");
        let png = png_for_image(dark_tile_for(&font_mask.mask, true));
        let validator = KanjiImageValidator::new();

        let expected_asset = synthetic_png_asset('心', png.len());
        let expected = validator
            .validate(&expected_asset, &mut Cursor::new(&png))
            .expect("dark Yarxi-style PNG validates");
        assert_eq!(expected.status, SemanticStatus::Verified);
        let evidence = expected.evidence[0]
            .details
            .as_ref()
            .expect("pixel evidence is present");
        assert_eq!(evidence["foreground_polarity"], "light_on_dark");
        assert_eq!(
            evidence["font_reference_version"],
            YARXI_FONT_REFERENCE_VERSION
        );
        assert!(evidence["noto_font_expected_distance"].as_f64().unwrap() < 0.020);
        assert!((evidence["max_distance"].as_f64().unwrap() - 0.020).abs() < 1e-7);
        assert!((evidence["minimum_margin"].as_f64().unwrap() - 0.004).abs() < 1e-7);

        let wrong_asset = synthetic_png_asset('疊', png.len());
        let wrong = validator
            .validate(&wrong_asset, &mut Cursor::new(&png))
            .expect("wrong expected identity validates as a decision");
        assert_ne!(wrong.status, SemanticStatus::Verified);
    }

    #[test]
    fn rendered_scale_kanji_confusables_keep_the_registered_margin() {
        let font_catalog = YARXI_FONT_REFERENCE_CATALOG
            .get_or_init(load_yarxi_font_reference_catalog)
            .as_ref()
            .expect("pinned Noto Serif JP reference catalog decodes");
        let validator = KanjiImageValidator::new();

        for (actual, wrong) in [('技', '枝'), ('髪', '髮')] {
            let template = font_catalog
                .get(&actual)
                .unwrap_or_else(|| panic!("Noto Serif JP has a {actual} template"));
            let png = png_for_image(rendered_dark_tile_for(&template.mask, true));
            let correct = validator
                .validate(
                    &synthetic_png_asset(actual, png.len()),
                    &mut Cursor::new(&png),
                )
                .expect("rendered-scale dark tile validates");
            assert_eq!(
                correct.status,
                SemanticStatus::Verified,
                "pixels for {actual}"
            );
            let evidence = correct.evidence[0]
                .details
                .as_ref()
                .expect("pixel evidence is present");
            assert!(evidence["nearest_margin"].as_f64().unwrap() >= 0.004);

            let wrong_identity = validator
                .validate(
                    &synthetic_png_asset(wrong, png.len()),
                    &mut Cursor::new(&png),
                )
                .expect("confusable wrong identity produces a decision");
            assert_ne!(
                wrong_identity.status,
                SemanticStatus::Verified,
                "pixels for {actual} must not verify as {wrong}"
            );
        }
    }

    #[test]
    fn dark_font_reference_confusables_remain_fail_closed() {
        let font_catalog = YARXI_FONT_REFERENCE_CATALOG
            .get_or_init(load_yarxi_font_reference_catalog)
            .as_ref()
            .expect("pinned Noto Serif JP reference catalog decodes");
        let validator = KanjiImageValidator::new();
        for (actual, wrong) in [
            ('未', '末'),
            ('末', '未'),
            ('土', '士'),
            ('士', '土'),
            ('己', '已'),
            ('己', '巳'),
            ('已', '己'),
            ('已', '巳'),
            ('巳', '己'),
            ('巳', '已'),
        ] {
            let template = font_catalog
                .get(&actual)
                .unwrap_or_else(|| panic!("Noto Serif JP has a {actual} template"));
            let png = png_for_image(dark_tile_for(&template.mask, true));
            let asset = synthetic_png_asset(wrong, png.len());
            let decision = validator
                .validate(&asset, &mut Cursor::new(&png))
                .expect("confusable wrong identity produces a semantic decision");
            assert_ne!(
                decision.status,
                SemanticStatus::Verified,
                "Noto Serif JP pixels for {actual} must not verify as {wrong}"
            );
        }
    }

    fn synthetic_png_asset(character: char, byte_length: usize) -> AssetRecord {
        use crate::model::{AssetIdentity, LifecycleState, Provenance};

        AssetRecord {
            identity: AssetIdentity::new("kanji", character.to_string()).unwrap(),
            storage_path: format!("assets/{character}.png"),
            sha256: "synthetic-test-hash".into(),
            byte_length: byte_length as u64,
            format: DetectedFormat::Png,
            provenance: Provenance {
                source_kind: "test".into(),
                source_name: "synthetic-dark-font-sample.png".into(),
            },
            lifecycle: LifecycleState::Pending,
            validation: None,
            domain_metadata: None,
        }
    }

    #[test]
    fn random_high_contrast_dark_tile_does_not_verify_by_polarity_alone() {
        let raster = decode_media(&png_for_image(random_light_on_dark_tile()))
            .expect("random high-contrast image decodes");
        assert_eq!(raster.foreground_polarity, ForegroundPolarity::LightOnDark);
        assert_ne!(
            semantic_status_for(&raster.mask, '元'),
            SemanticStatus::Verified
        );
    }

    #[test]
    fn wrong_identity_and_confusable_pairs_never_receive_false_verified() {
        for (actual, wrong) in [
            ('未', '末'),
            ('末', '未'),
            ('土', '士'),
            ('士', '土'),
            ('己', '已'),
            ('己', '巳'),
            ('已', '己'),
            ('已', '巳'),
            ('巳', '己'),
            ('巳', '已'),
        ] {
            let candidate = reference(actual);
            assert_ne!(
                semantic_status_for(&candidate, wrong),
                SemanticStatus::Verified,
                "{} pixels must not verify as {}",
                actual,
                wrong
            );
        }
    }

    #[test]
    fn thickness_and_partial_stroke_regressions_fail_closed() {
        let mut thickened = reference('士');
        let original = thickened;
        for y in 1..MASK_SIDE - 1 {
            for x in 1..MASK_SIDE - 1 {
                if get_ink(&original, x, y) {
                    for (nx, ny) in [(x - 1, y), (x + 1, y), (x, y - 1), (x, y + 1)] {
                        set_ink(&mut thickened, nx, ny);
                    }
                }
            }
        }
        assert_ne!(
            semantic_status_for(&thickened, '土'),
            SemanticStatus::Verified
        );

        let mut early = reference('未');
        for y in 0..MASK_SIDE {
            if y > 20 {
                for x in 0..MASK_SIDE {
                    let index = y * MASK_SIDE + x;
                    early[index / 8] &= !(1 << (index % 8));
                }
            }
        }
        assert_ne!(semantic_status_for(&early, '未'), SemanticStatus::Verified);
    }

    #[test]
    fn known_image_with_missing_reference_stays_uncertain() {
        assert_eq!(
            classify_distance(f32::INFINITY, f32::INFINITY, f32::NEG_INFINITY),
            SemanticStatus::Uncertain
        );
        assert_eq!(
            classify_distance(0.02, 0.01, -0.01),
            SemanticStatus::Uncertain
        );
    }

    #[test]
    fn registered_threshold_boundaries_remain_fail_closed() {
        assert_eq!(
            classify_distance(0.020, 0.025, 0.005),
            SemanticStatus::Verified
        );
        assert_eq!(
            classify_distance(0.0201, 0.0251, 0.005),
            SemanticStatus::Uncertain,
            "distance above the positive bound is not accepted"
        );
        assert_eq!(
            classify_distance(0.015, 0.0189, 0.0039),
            SemanticStatus::Uncertain,
            "margin below the positive bound is not accepted"
        );
        assert_eq!(
            classify_distance(0.030, 0.010, -0.020),
            SemanticStatus::Rejected
        );
    }
}
