/// 64-bit difference hash: resize to 9x8 luma, then for each row compare horizontally
/// adjacent samples. Bit is 1 when the LEFT sample is strictly greater than the RIGHT one.
/// Bits are emitted most-significant-first in row-major order (row 0 leftmost pair is bit 63).
pub fn dhash_from_rgba(
    rgba: &[u8],
    width: u32,
    height: u32,
) -> Result<u64, crate::change::ImageBufferError> {
    let image = crate::change::rgba_image(rgba, width, height)?;
    let resized = image::imageops::resize(&image, 9, 8, image::imageops::FilterType::Triangle);
    let grayscale = image::imageops::grayscale(&resized);
    let mut hash = 0_u64;

    for y in 0..8 {
        for x in 0..8 {
            hash <<= 1;
            if grayscale.get_pixel(x, y)[0] > grayscale.get_pixel(x + 1, y)[0] {
                hash |= 1;
            }
        }
    }

    Ok(hash)
}

/// Number of differing bits between two fingerprints.
pub fn hamming_distance(a: u64, b: u64) -> u32 {
    (a ^ b).count_ones()
}

#[cfg(test)]
mod tests {
    use super::{dhash_from_rgba, hamming_distance};

    /// Solid RGBA image of `w`x`h filled with (v, v, v, 255).
    fn solid(w: u32, h: u32, v: u8) -> Vec<u8> {
        let pixel_count =
            usize::try_from(w * h).expect("the fixed test image dimensions must fit usize");
        let mut buf = Vec::with_capacity(pixel_count * 4);
        for _ in 0..pixel_count {
            buf.extend_from_slice(&[v, v, v, 255]);
        }
        buf
    }

    /// Fills the axis-aligned rectangle [x0,x1) x [y0,y1) with (v, v, v, 255) in place.
    fn fill_rect(buf: &mut [u8], w: u32, x0: u32, x1: u32, y0: u32, y1: u32, v: u8) {
        for y in y0..y1 {
            for x in x0..x1 {
                let offset = usize::try_from((y * w + x) * 4)
                    .expect("the fixed test rectangle offset must fit usize");
                buf[offset..offset + 4].copy_from_slice(&[v, v, v, 255]);
            }
        }
    }

    fn horizontal_gradient(w: u32, h: u32, decreasing: bool) -> Vec<u8> {
        let mut buf = solid(w, h, 0);
        for y in 0..h {
            for x in 0..w {
                let increasing = u8::try_from(x * 255 / (w - 1))
                    .expect("the gradient value must be in the u8 range");
                let v = if decreasing {
                    255 - increasing
                } else {
                    increasing
                };
                let offset = usize::try_from((y * w + x) * 4)
                    .expect("the fixed gradient offset must fit usize");
                buf[offset..offset + 4].copy_from_slice(&[v, v, v, 255]);
            }
        }
        buf
    }

    #[test]
    fn horizontal_gradient_yields_known_fingerprint() {
        let increasing = horizontal_gradient(256, 256, false);
        let decreasing = horizontal_gradient(256, 256, true);

        assert_eq!(
            dhash_from_rgba(&increasing, 256, 256)
                .expect("the increasing gradient must be valid RGBA"),
            0,
            "a strictly increasing horizontal gradient must produce an all-zero dHash"
        );
        assert_eq!(
            dhash_from_rgba(&decreasing, 256, 256)
                .expect("the decreasing gradient must be valid RGBA"),
            u64::MAX,
            "a strictly decreasing horizontal gradient must produce an all-one dHash"
        );
    }

    #[test]
    fn hamming_distance_of_identical_values_is_zero() {
        assert_eq!(
            hamming_distance(0, 0),
            0,
            "identical fingerprints must have zero Hamming distance"
        );
        assert_eq!(
            hamming_distance(0, u64::MAX),
            64,
            "opposite fingerprints must differ in all 64 bits"
        );
    }

    #[test]
    fn structured_image_has_a_nonflat_fingerprint() {
        let mut structured = solid(256, 256, 30);
        fill_rect(&mut structured, 256, 0, 128, 0, 256, 200);
        let structured_hash = dhash_from_rgba(&structured, 256, 256)
            .expect("the structured image must be valid RGBA");

        assert_ne!(
            structured_hash, 0,
            "a light-to-dark edge must produce set dHash bits"
        );
        assert_ne!(
            structured_hash,
            u64::MAX,
            "only pairs straddling the light-to-dark edge should produce set dHash bits"
        );

        let flat = solid(256, 256, 30);
        // An all-zero fingerprint means no left-to-right variation, so it is shared by
        // every flat image and every strictly increasing gradient.
        assert_eq!(
            dhash_from_rgba(&flat, 256, 256).expect("the solid image must be valid RGBA"),
            0,
            "a solid image must produce an all-zero dHash"
        );
    }
}
