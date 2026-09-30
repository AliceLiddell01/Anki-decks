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

use asset_store::kanji_mask::{MASK_BYTES, Mask, normalize_binary_mask};

const KANJIVG_DB: &[u8] = include_bytes!("../data/kanjivg-r20250816.maskdb.zlib");
const PINNED_FONT_SHA256: &str = "e6cffcd5cae6a298ddfd17173b42d64888913976b2d7053024a9ecf0cf5d3fe8";

fn main() -> Result<(), Box<dyn Error>> {
    let (font_path, output) = arguments()?;
    let font_bytes = fs::read(&font_path)?;
    let font_sha256 = format!("{:x}", Sha256::digest(&font_bytes));
    if font_sha256 != PINNED_FONT_SHA256 {
        return Err(format!(
            "SHA-256 исходного шрифта Noto Serif JP не совпадает: ожидалось {PINNED_FONT_SHA256}, получено {font_sha256}"
        )
        .into());
    }
    let characters = kanjivg_codepoints()?;
    let mut fontdb = fontdb::Database::new();
    fontdb.load_font_data(font_bytes);
    if fontdb.faces().next().is_none() {
        return Err("в исходном шрифте не найдено пригодное начертание".into());
    }
    let options = usvg::Options {
        fontdb: Arc::new(fontdb),
        ..Default::default()
    };

    let mut templates = BTreeMap::<u32, Mask>::new();
    for codepoint in characters {
        let character = char::from_u32(codepoint)
            .ok_or_else(|| format!("недопустимый скаляр Unicode U+{codepoint:X}"))?;
        let image = render_character(character, &options)?;
        let Some(mask) = normalize(&image)? else {
            continue;
        };
        if templates.insert(codepoint, mask).is_some() {
            return Err(format!("повторная ссылка Unicode U+{codepoint:X}").into());
        }
    }
    if templates.len() < 1000 {
        return Err(format!(
            "отрисовано только {} глифов CJK из Noto Serif JP",
            templates.len()
        )
        .into());
    }

    let mut raw = Vec::with_capacity(8 + templates.len() * (4 + MASK_BYTES));
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
        raw.len().saturating_sub(8) / (4 + MASK_BYTES),
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
            _ => return Err(format!("неизвестный или неполный аргумент: {arg:?}").into()),
        }
    }
    Ok((
        font.ok_or("обязательный аргумент: --font-file <закреплённый шрифт Noto Serif JP TTF>")?,
        output.ok_or("обязательный аргумент: --output <maskdb.zlib>")?,
    ))
}

fn kanjivg_codepoints() -> Result<Vec<u32>, Box<dyn Error>> {
    let mut decoder = ZlibDecoder::new(KANJIVG_DB);
    let mut bytes = Vec::new();
    decoder.read_to_end(&mut bytes)?;
    if bytes.len() < 8 || &bytes[..4] != b"KJ64" {
        return Err("у закреплённой исходной базы KanjiVG неверный заголовок".into());
    }
    let count = u32::from_le_bytes(bytes[4..8].try_into()?) as usize;
    let entry_size = 4 + MASK_BYTES;
    if count > 20_000 || bytes.len() != 8 + count * entry_size {
        return Err("у закреплённой исходной базы KanjiVG неверный размер".into());
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
        return Err(format!("недопустимые размеры SVG глифа {character}").into());
    }
    let mut pixmap = resvg::tiny_skia::Pixmap::new(size.width(), size.height())
        .ok_or("не удалось выделить память для холста эталонного шрифта")?;
    resvg::render(
        &tree,
        resvg::tiny_skia::Transform::default(),
        &mut pixmap.as_mut(),
    );
    ImageBuffer::<Rgba<u8>, Vec<u8>>::from_raw(size.width(), size.height(), pixmap.data().to_vec())
        .ok_or_else(|| "у изображения эталонного шрифта недопустимые размеры".into())
}

fn normalize(image: &RgbaImage) -> Result<Option<Mask>, Box<dyn Error>> {
    let (width, height) = image.dimensions();
    let foreground = image
        .pixels()
        .map(|pixel| u8::from(is_ink(pixel.0)))
        .collect::<Vec<_>>();
    Ok(normalize_binary_mask(
        &foreground,
        width as usize,
        height as usize,
    ))
}

fn is_ink(pixel: [u8; 4]) -> bool {
    let alpha = f32::from(pixel[3]) / 255.0;
    let luma =
        0.2126 * f32::from(pixel[0]) + 0.7152 * f32::from(pixel[1]) + 0.0722 * f32::from(pixel[2]);
    alpha.mul_add(luma, (1.0 - alpha) * 255.0) < 225.0
}
