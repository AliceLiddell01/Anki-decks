//! Общие правила crop, масштабирования и resampling масок кандзи 64×64.

pub const MASK_SIDE: usize = 64;
pub const MASK_BYTES: usize = MASK_SIDE * MASK_SIDE / 8;
pub const MASK_INTERIOR_SIDE: usize = MASK_SIDE - 8;

pub type Mask = [u8; MASK_BYTES];

/// Обрезает row-major бинарную foreground-маску и помещает её по центру в
/// 64×64 mask. Каждый выходной пиксель выбирается из центра source-cell.
pub fn normalize_binary_mask(source: &[u8], width: usize, height: usize) -> Option<Mask> {
    let source_len = width.checked_mul(height)?;
    if width == 0 || height == 0 || source.len() != source_len {
        return None;
    }

    let mut bounds: Option<(usize, usize, usize, usize)> = None;
    for (index, &pixel) in source.iter().enumerate() {
        if pixel == 0 {
            continue;
        }
        let x = index % width;
        let y = index / width;
        bounds = Some(match bounds {
            Some((left, top, right, bottom)) => {
                (left.min(x), top.min(y), right.max(x), bottom.max(y))
            }
            None => (x, y, x, y),
        });
    }
    let (left, top, right, bottom) = bounds?;
    let source_width = right - left + 1;
    let source_height = bottom - top + 1;
    let scale = (MASK_INTERIOR_SIDE as f32 / source_width as f32)
        .min(MASK_INTERIOR_SIDE as f32 / source_height as f32);
    let scaled_width =
        ((source_width as f32 * scale).round() as usize).clamp(1, MASK_INTERIOR_SIDE);
    let scaled_height =
        ((source_height as f32 * scale).round() as usize).clamp(1, MASK_INTERIOR_SIDE);
    let offset_x = (MASK_SIDE - scaled_width) / 2;
    let offset_y = (MASK_SIDE - scaled_height) / 2;
    let mut mask = [0; MASK_BYTES];

    for y in 0..scaled_height {
        let source_y = top + center_sample_index(y, source_height, scaled_height);
        for x in 0..scaled_width {
            let source_x = left + center_sample_index(x, source_width, scaled_width);
            if source[source_y * width + source_x] != 0 {
                let index = (offset_y + y) * MASK_SIDE + offset_x + x;
                mask[index / 8] |= 1 << (index % 8);
            }
        }
    }

    mask.iter().any(|byte| *byte != 0).then_some(mask)
}

/// Находит source pixel, содержащий центр выходной ячейки.
pub(crate) fn center_sample_index(
    output_index: usize,
    source_size: usize,
    output_size: usize,
) -> usize {
    if source_size == 0 || output_size == 0 {
        return 0;
    }
    let numerator = (2 * output_index as u64 + 1) * source_size as u64;
    let denominator = 2 * output_size as u64;
    (numerator / denominator).min(source_size.saturating_sub(1) as u64) as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    fn get_ink(mask: &Mask, x: usize, y: usize) -> bool {
        let index = y * MASK_SIDE + x;
        mask[index / 8] & (1 << (index % 8)) != 0
    }

    #[test]
    fn center_sampling_covers_odd_even_and_upsampled_cells() {
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
    fn equivalent_builder_and_runtime_rasters_share_one_normalization() {
        let width = 7;
        let height = 5;
        let mut builder_foreground = vec![0; width * height];
        builder_foreground[width + 1] = 1;
        builder_foreground[width + 2] = 1;
        builder_foreground[2 * width + 3] = 1;
        builder_foreground[3 * width + 4] = 1;
        builder_foreground[3 * width + 5] = 1;
        let runtime_width = width * 2;
        let runtime_height = height * 2;
        let mut runtime_foreground = vec![0; runtime_width * runtime_height];
        for y in 0..height {
            for x in 0..width {
                if builder_foreground[y * width + x] != 0 {
                    for dy in 0..2 {
                        for dx in 0..2 {
                            runtime_foreground[(2 * y + dy) * runtime_width + 2 * x + dx] = 1;
                        }
                    }
                }
            }
        }

        let builder_mask = normalize_binary_mask(&builder_foreground, width, height)
            .expect("непустая builder-маска нормализуется");
        let runtime_mask =
            normalize_binary_mask(&runtime_foreground, runtime_width, runtime_height)
                .expect("непустая runtime-маска нормализуется");
        assert_eq!(builder_mask, runtime_mask);
        assert!(get_ink(&builder_mask, 18, 18));
        assert!(get_ink(&builder_mask, 45, 45));
    }

    #[test]
    fn blank_and_invalid_binary_rasters_have_no_mask() {
        assert!(normalize_binary_mask(&[], 0, 0).is_none());
        assert!(normalize_binary_mask(&[1], 2, 1).is_none());
        assert!(normalize_binary_mask(&[0; 4], 2, 2).is_none());
    }
}
