use std::collections::BTreeMap;
use std::error::Error;
use std::fs;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::Arc;

use flate2::Compression;
use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use image::{ImageBuffer, Rgba, RgbaImage};
use resvg::usvg::{self, fontdb};
use sha2::{Digest, Sha256};

const SIDE: usize = 64;
const BYTES: usize = SIDE * SIDE / 8;
const KANJIVG_DB: &[u8] = include_bytes!("../data/kanjivg-r20250816.maskdb.zlib");
const PINNED_FONT_SHA256: &str = "e6cffcd5cae6a298ddfd17173b42d64888913976b2d7053024a9ecf0cf5d3fe8";

fn main() -> Result<(), Box<dyn Error>> {
    let (font_path, output) = arguments()?;
    let font_bytes = fs::read(&font_path)?;
    let font_sha256 = format!("{:x}", Sha256::digest(&font_bytes));
    if font_sha256 != PINNED_FONT_SHA256 {
        return Err(format!(
            "Noto Serif JP source font SHA-256 mismatch: expected {PINNED_FONT_SHA256}, got {font_sha256}"
        )
        .into());
    }
    let characters = kanjivg_codepoints()?;
    let mut fontdb = fontdb::Database::new();
    fontdb.load_font_data(font_bytes);
    if fontdb.faces().next().is_none() {
        return Err("source font did not provide a usable face".into());
    }
    let mut options = usvg::Options::default();
    options.fontdb = Arc::new(fontdb);

    let mut templates = BTreeMap::<u32, [u8; BYTES]>::new();
    for codepoint in characters {
        let character = char::from_u32(codepoint)
            .ok_or_else(|| format!("invalid Unicode scalar U+{codepoint:X}"))?;
        let image = render_character(character, &options)?;
        let Some(mask) = normalize(&image)? else {
            continue;
        };
        if templates.insert(codepoint, mask).is_some() {
            return Err(format!("duplicate Unicode reference U+{codepoint:X}").into());
        }
    }
    if templates.len() < 1000 {
        return Err(format!(
            "only {} Noto Serif JP CJK glyphs were rendered",
            templates.len()
        )
        .into());
    }

    let mut raw = Vec::with_capacity(8 + templates.len() * (4 + BYTES));
    raw.extend_from_slice(b"YJ64");
    raw.extend_from_slice(&(templates.len() as u32).to_le_bytes());
    for (codepoint, mask) in templates {
        raw.extend_from_slice(&codepoint.to_le_bytes());
        raw.extend_from_slice(&mask);
    }
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::best());
    encoder.write_all(&raw)?;
    let compressed = encoder.finish()?;
    let temp = output.with_extension("zlib.tmp");
    fs::write(&temp, &compressed)?;
    fs::rename(&temp, &output)?;
    println!(
        "templates={} font_sha256={} raw_bytes={} compressed_bytes={} output_sha256={:x}",
        raw.len().saturating_sub(8) / (4 + BYTES),
        font_sha256,
        raw.len(),
        compressed.len(),
        Sha256::digest(&compressed),
    );
    Ok(())
}

fn arguments() -> Result<(PathBuf, PathBuf), Box<dyn Error>> {
    let mut args = std::env::args_os().skip(1);
    let mut font = None;
    let mut output = None;
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--font-file") => font = args.next().map(PathBuf::from),
            Some("--output") => output = args.next().map(PathBuf::from),
            _ => return Err(format!("unknown or incomplete argument: {arg:?}").into()),
        }
    }
    Ok((
        font.ok_or("required: --font-file <pinned Noto Serif JP TTF>")?,
        output.ok_or("required: --output <maskdb.zlib>")?,
    ))
}

fn kanjivg_codepoints() -> Result<Vec<u32>, Box<dyn Error>> {
    let mut decoder = ZlibDecoder::new(KANJIVG_DB);
    let mut bytes = Vec::new();
    decoder.read_to_end(&mut bytes)?;
    if bytes.len() < 8 || &bytes[..4] != b"KJ64" {
        return Err("pinned KanjiVG source database has an invalid header".into());
    }
    let count = u32::from_le_bytes(bytes[4..8].try_into()?) as usize;
    let entry_size = 4 + BYTES;
    if count > 20_000 || bytes.len() != 8 + count * entry_size {
        return Err("pinned KanjiVG source database has an invalid size".into());
    }
    Ok(bytes[8..]
        .chunks_exact(entry_size)
        .map(|entry| u32::from_le_bytes(entry[..4].try_into().unwrap()))
        .collect())
}

fn render_character(
    character: char,
    options: &usvg::Options<'_>,
) -> Result<RgbaImage, Box<dyn Error>> {
    let svg = format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="128" height="128" viewBox="0 0 128 128"><rect width="128" height="128" fill="#fff"/><text x="64" y="100" text-anchor="middle" font-family="Noto Serif JP" font-size="100" fill="#000">{character}</text></svg>"##
    );
    let tree = usvg::Tree::from_data(svg.as_bytes(), options)?;
    let size = tree.size().to_int_size();
    if size.width() == 0 || size.height() == 0 || size.width() > 512 || size.height() > 512 {
        return Err(format!("invalid glyph SVG dimensions for {character}").into());
    }
    let mut pixmap = resvg::tiny_skia::Pixmap::new(size.width(), size.height())
        .ok_or("font reference canvas allocation failed")?;
    resvg::render(
        &tree,
        resvg::tiny_skia::Transform::default(),
        &mut pixmap.as_mut(),
    );
    ImageBuffer::<Rgba<u8>, Vec<u8>>::from_raw(size.width(), size.height(), pixmap.data().to_vec())
        .ok_or_else(|| "font reference image dimensions are invalid".into())
}

fn normalize(image: &RgbaImage) -> Result<Option<[u8; BYTES]>, Box<dyn Error>> {
    let mut bounds: Option<(u32, u32, u32, u32)> = None;
    for (x, y, pixel) in image.enumerate_pixels() {
        if is_ink(pixel.0) {
            bounds = Some(match bounds {
                Some((left, top, right, bottom)) => {
                    (left.min(x), top.min(y), right.max(x), bottom.max(y))
                }
                None => (x, y, x, y),
            });
        }
    }
    let Some((left, top, right, bottom)) = bounds else {
        return Ok(None);
    };
    let width = right - left + 1;
    let height = bottom - top + 1;
    let inner = (SIDE - 8) as u32;
    let scale = (inner as f32 / width as f32).min(inner as f32 / height as f32);
    let scaled_width = ((width as f32 * scale).round() as usize).clamp(1, SIDE - 8);
    let scaled_height = ((height as f32 * scale).round() as usize).clamp(1, SIDE - 8);
    let offset_x = (SIDE - scaled_width) / 2;
    let offset_y = (SIDE - scaled_height) / 2;
    let mut mask = [0; BYTES];
    for y in 0..scaled_height {
        for x in 0..scaled_width {
            let source_x = left + (x as u32 * width / scaled_width as u32).min(width - 1);
            let source_y = top + (y as u32 * height / scaled_height as u32).min(height - 1);
            if is_ink(image.get_pixel(source_x, source_y).0) {
                let index = (offset_y + y) * SIDE + offset_x + x;
                mask[index / 8] |= 1 << (index % 8);
            }
        }
    }
    Ok((mask.iter().any(|byte| *byte != 0)).then_some(mask))
}

fn is_ink(pixel: [u8; 4]) -> bool {
    let alpha = f32::from(pixel[3]) / 255.0;
    let luma =
        0.2126 * f32::from(pixel[0]) + 0.7152 * f32::from(pixel[1]) + 0.0722 * f32::from(pixel[2]);
    alpha.mul_add(luma, (1.0 - alpha) * 255.0) < 225.0
}
