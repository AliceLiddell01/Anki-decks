use std::collections::BTreeMap;
use std::error::Error;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use flate2::Compression;
use flate2::write::ZlibEncoder;
use image::{ImageBuffer, Rgba, RgbaImage};
use sha2::{Digest, Sha256};

const SIDE: usize = 64;
const BYTES: usize = SIDE * SIDE / 8;

fn main() -> Result<(), Box<dyn Error>> {
    let (input_dir, output) = arguments()?;
    let mut templates = BTreeMap::<u32, [u8; BYTES]>::new();
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
        if !is_cjk(codepoint) || char::from_u32(codepoint).is_none() {
            continue;
        }
        let image = render_svg(&path)?;
        let mask = normalize(&image)?;
        if templates.insert(codepoint, mask).is_some() {
            return Err(format!("duplicate Unicode reference U+{codepoint:X}").into());
        }
    }
    if templates.len() < 1000 {
        return Err(format!("only {} CJK references parsed", templates.len()).into());
    }

    let mut raw = Vec::with_capacity(8 + templates.len() * (4 + BYTES));
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
        "templates={} raw_bytes={} compressed_bytes={} sha256={:x}",
        raw.len().saturating_sub(8) / (4 + BYTES),
        raw.len(),
        compressed.len(),
        Sha256::digest(&compressed),
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
            _ => return Err(format!("unknown or incomplete argument: {arg:?}").into()),
        }
    }
    Ok((
        input.ok_or("required: --source-dir <KanjiVG>/kanji")?,
        output.ok_or("required: --output <maskdb.zlib>")?,
    ))
}

fn is_cjk(codepoint: u32) -> bool {
    matches!(codepoint,
        0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xF900..=0xFAFF |
        0x20000..=0x2FA1F | 0x30000..=0x3134F)
}

fn render_svg(path: &Path) -> Result<RgbaImage, Box<dyn Error>> {
    let source = String::from_utf8(fs::read(path)?)?
        .replace("<svg ", "<svg xmlns:kvg=\"https://kanjivg.tagaini.net/\" ");
    let tree = resvg::usvg::Tree::from_data(source.as_bytes(), &resvg::usvg::Options::default())?;
    let size = tree.size().to_int_size();
    if size.width() == 0 || size.height() == 0 || size.width() > 512 || size.height() > 512 {
        return Err(format!("invalid SVG dimensions in {}", path.display()).into());
    }
    let mut pixmap = resvg::tiny_skia::Pixmap::new(size.width(), size.height())
        .ok_or("could not allocate KanjiVG reference raster")?;
    resvg::render(
        &tree,
        resvg::tiny_skia::Transform::default(),
        &mut pixmap.as_mut(),
    );
    ImageBuffer::<Rgba<u8>, Vec<u8>>::from_raw(size.width(), size.height(), pixmap.data().to_vec())
        .ok_or_else(|| "rendered SVG buffer has invalid dimensions".into())
}

fn normalize(image: &RgbaImage) -> Result<[u8; BYTES], Box<dyn Error>> {
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
        return Err("KanjiVG template is blank".into());
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
    Ok(mask)
}

fn is_ink(pixel: [u8; 4]) -> bool {
    let alpha = f32::from(pixel[3]) / 255.0;
    let luma =
        0.2126 * f32::from(pixel[0]) + 0.7152 * f32::from(pixel[1]) + 0.0722 * f32::from(pixel[2]);
    alpha.mul_add(luma, (1.0 - alpha) * 255.0) < 225.0
}
