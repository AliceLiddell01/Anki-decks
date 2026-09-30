use std::collections::BTreeMap;
use std::error::Error;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use flate2::Compression;
use flate2::write::ZlibEncoder;
use image::{ImageBuffer, Rgba, RgbaImage};
use asset_store::hashing::sha256_hex;
use asset_store::kanji_domain::is_supported_han;
use asset_store::kanji_mask::{MASK_BYTES, Mask, normalize_binary_mask};

fn main() -> Result<(), Box<dyn Error>> {
    let (input_dir, output) = arguments()?;
    let mut templates = BTreeMap::<u32, Mask>::new();
    let mut paths: Vec<_> = fs::read_dir(input_dir)?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<Result<_, _>>()?;
    paths.sort();
    for path in paths {
        if path.extension().is_none_or(|extension| extension != "svg") {
            continue;
        }
        let Some(codepoint) = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .and_then(|stem| u32::from_str_radix(stem, 16).ok())
        else {
            continue;
        };
        let Some(character) = char::from_u32(codepoint) else {
            continue;
        };
        if !is_supported_han(character) {
            continue;
        }
        let image = render_svg(&path)?;
        let mask = normalize(&image)?;
        if templates.insert(codepoint, mask).is_some() {
            return Err(format!("повторная ссылка Unicode U+{codepoint:X}").into());
        }
    }
    if templates.len() < 1000 {
        return Err(format!("найдено только {} эталонов CJK", templates.len()).into());
    }

    let mut raw = Vec::with_capacity(8 + templates.len() * (4 + MASK_BYTES));
    raw.extend_from_slice(b"KJ64");
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
        "templates={} raw_bytes={} compressed_bytes={} sha256={}",
        raw.len().saturating_sub(8) / (4 + MASK_BYTES),
        raw.len(),
        compressed.len(),
        sha256_hex(&compressed),
    );
    Ok(())
}

fn arguments() -> Result<(PathBuf, PathBuf), Box<dyn Error>> {
    let mut args = std::env::args_os().skip(1);
    let mut input = None;
    let mut output = None;
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--source-dir") => input = args.next().map(PathBuf::from),
            Some("--output") => output = args.next().map(PathBuf::from),
            _ => return Err(format!("неизвестный или неполный аргумент: {arg:?}").into()),
        }
    }
    Ok((
        input.ok_or("обязательный аргумент: --source-dir <KanjiVG>/kanji")?,
        output.ok_or("обязательный аргумент: --output <maskdb.zlib>")?,
    ))
}

fn render_svg(path: &Path) -> Result<RgbaImage, Box<dyn Error>> {
    let source = String::from_utf8(fs::read(path)?)?
        .replace("<svg ", "<svg xmlns:kvg=\"https://kanjivg.tagaini.net/\" ");
    let tree = resvg::usvg::Tree::from_data(source.as_bytes(), &resvg::usvg::Options::default())?;
    let size = tree.size().to_int_size();
    if size.width() == 0 || size.height() == 0 || size.width() > 512 || size.height() > 512 {
        return Err(format!("недопустимые размеры SVG в {}", path.display()).into());
    }
    let mut pixmap = resvg::tiny_skia::Pixmap::new(size.width(), size.height())
        .ok_or("не удалось выделить память для растра KanjiVG")?;
    resvg::render(
        &tree,
        resvg::tiny_skia::Transform::default(),
        &mut pixmap.as_mut(),
    );
    ImageBuffer::<Rgba<u8>, Vec<u8>>::from_raw(size.width(), size.height(), pixmap.data().to_vec())
        .ok_or_else(|| "у отрисованного буфера SVG недопустимые размеры".into())
}

fn normalize(image: &RgbaImage) -> Result<Mask, Box<dyn Error>> {
    let (width, height) = image.dimensions();
    let foreground = image
        .pixels()
        .map(|pixel| u8::from(is_ink(pixel.0)))
        .collect::<Vec<_>>();
    normalize_binary_mask(&foreground, width as usize, height as usize)
        .ok_or_else(|| "шаблон KanjiVG пуст".into())
}

fn is_ink(pixel: [u8; 4]) -> bool {
    let alpha = f32::from(pixel[3]) / 255.0;
    let luma =
        0.2126 * f32::from(pixel[0]) + 0.7152 * f32::from(pixel[1]) + 0.0722 * f32::from(pixel[2]);
    alpha.mul_add(luma, (1.0 - alpha) * 255.0) < 225.0
}
