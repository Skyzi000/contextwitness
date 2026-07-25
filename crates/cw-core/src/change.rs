/// Fixed working width for change detection. Small enough to be cheap per tick,
/// large enough that a single edited text line still moves a detectable number of pixels.
pub const THUMBNAIL_WIDTH: u32 = 256;

/// Fixed working height for change detection. Small enough to be cheap per tick,
/// large enough that a single edited text line still moves a detectable number of pixels.
pub const THUMBNAIL_HEIGHT: u32 = 144;

/// Errors produced while constructing an image buffer from RGBA data.
#[derive(Debug, thiserror::Error)]
pub enum ImageBufferError {
    /// The RGBA byte count does not match the supplied dimensions.
    #[error("expected {expected} bytes of RGBA data for {width}x{height}, got {actual}")]
    SizeMismatch {
        /// Supplied image width.
        width: u32,
        /// Supplied image height.
        height: u32,
        /// Required number of tightly packed RGBA bytes.
        expected: usize,
        /// Actual number of supplied bytes.
        actual: usize,
    },
    /// At least one supplied image dimension is zero.
    #[error("image dimensions must be non-zero")]
    EmptyImage,
}

/// Downscaled grayscale view of a frame, kept between ticks instead of the full frame
/// (a few tens of KiB per monitor rather than tens of MiB).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Thumbnail {
    luma: Vec<u8>,
}

impl Thumbnail {
    /// Build from tightly packed RGBA8. Resizes to the fixed thumbnail size with
    /// [`image::imageops::FilterType::Triangle`], then converts to luma8.
    pub fn from_rgba(rgba: &[u8], width: u32, height: u32) -> Result<Thumbnail, ImageBufferError> {
        let image = rgba_image(rgba, width, height)?;
        let resized = image::imageops::resize(
            &image,
            THUMBNAIL_WIDTH,
            THUMBNAIL_HEIGHT,
            image::imageops::FilterType::Triangle,
        );
        let grayscale = image::imageops::grayscale(&resized);

        Ok(Self {
            luma: grayscale.into_raw(),
        })
    }

    /// Fraction (0.0..=1.0) of samples whose absolute luma difference is strictly greater
    /// than `pixel_threshold`.
    pub fn changed_fraction(&self, other: &Thumbnail, pixel_threshold: u8) -> f64 {
        let changed = self
            .luma
            .iter()
            .zip(&other.luma)
            .filter(|(left, right)| left.abs_diff(**right) > pixel_threshold)
            .count();
        let changed =
            u32::try_from(changed).expect("the fixed thumbnail sample count must fit in u32");
        let sample_count = u32::try_from(self.luma.len())
            .expect("the fixed thumbnail sample count must fit in u32");

        f64::from(changed) / f64::from(sample_count)
    }
}

/// Whether this frame must be stored and OCR-ed.
/// `previous` is `None` for the first frame of a monitor, which always counts as changed.
pub fn frame_changed(
    previous: Option<&Thumbnail>,
    current: &Thumbnail,
    config: &crate::config::CaptureConfig,
) -> bool {
    previous.is_none_or(|previous| {
        previous.changed_fraction(current, config.change_pixel_threshold) > config.change_ratio
    })
}

pub(crate) fn rgba_image(
    rgba: &[u8],
    width: u32,
    height: u32,
) -> Result<image::RgbaImage, ImageBufferError> {
    if width == 0 || height == 0 {
        return Err(ImageBufferError::EmptyImage);
    }

    let expected = usize::try_from(width)
        .expect("u32 image width must fit usize on supported targets")
        * usize::try_from(height).expect("u32 image height must fit usize on supported targets")
        * 4;
    if rgba.len() != expected {
        return Err(ImageBufferError::SizeMismatch {
            width,
            height,
            expected,
            actual: rgba.len(),
        });
    }

    Ok(image::RgbaImage::from_raw(width, height, rgba.to_vec())
        .expect("validated RGBA dimensions and byte count must form an image buffer"))
}

#[cfg(test)]
mod tests {
    use super::{ImageBufferError, Thumbnail, frame_changed};
    use crate::config::CaptureConfig;

    const WIDTH: u32 = 2560;
    const HEIGHT: u32 = 1440;

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

    #[test]
    fn identical_frames_are_unchanged() {
        let before = solid(WIDTH, HEIGHT, 200);
        let after = solid(WIDTH, HEIGHT, 200);
        let before = Thumbnail::from_rgba(&before, WIDTH, HEIGHT)
            .expect("the fixed-size before frame must be valid RGBA");
        let after = Thumbnail::from_rgba(&after, WIDTH, HEIGHT)
            .expect("the fixed-size after frame must be valid RGBA");

        assert!(
            !frame_changed(Some(&before), &after, &CaptureConfig::default()),
            "identical frames must not be treated as changed"
        );
    }

    #[test]
    fn first_frame_is_always_changed() {
        let frame = solid(WIDTH, HEIGHT, 200);
        let thumbnail = Thumbnail::from_rgba(&frame, WIDTH, HEIGHT)
            .expect("the fixed-size frame must be valid RGBA");

        assert!(
            frame_changed(None, &thumbnail, &CaptureConfig::default()),
            "the first frame for a monitor must always be treated as changed"
        );
    }

    #[test]
    fn single_text_line_edit_is_detected() {
        let before = solid(WIDTH, HEIGHT, 200);
        let mut after = before.clone();
        fill_rect(&mut after, WIDTH, 100, 900, 500, 520, 30);
        let before = Thumbnail::from_rgba(&before, WIDTH, HEIGHT)
            .expect("the fixed-size before frame must be valid RGBA");
        let after = Thumbnail::from_rgba(&after, WIDTH, HEIGHT)
            .expect("the fixed-size after frame must be valid RGBA");
        let config = CaptureConfig::default();
        let changed_fraction = before.changed_fraction(&after, config.change_pixel_threshold);

        assert!(
            frame_changed(Some(&before), &after, &config),
            "the edited text line must be detected; changed_fraction={changed_fraction}"
        );
    }

    #[test]
    fn terminal_line_append_is_detected() {
        let before = solid(WIDTH, HEIGHT, 200);
        let mut after = before.clone();
        fill_rect(&mut after, WIDTH, 0, 600, 1400, 1420, 30);
        let before = Thumbnail::from_rgba(&before, WIDTH, HEIGHT)
            .expect("the fixed-size before frame must be valid RGBA");
        let after = Thumbnail::from_rgba(&after, WIDTH, HEIGHT)
            .expect("the fixed-size after frame must be valid RGBA");
        let config = CaptureConfig::default();
        let changed_fraction = before.changed_fraction(&after, config.change_pixel_threshold);

        assert!(
            frame_changed(Some(&before), &after, &config),
            "the appended terminal line must be detected; changed_fraction={changed_fraction}"
        );
    }

    #[test]
    fn small_scroll_is_detected() {
        let mut before = solid(WIDTH, HEIGHT, 200);
        let mut after = solid(WIDTH, HEIGHT, 200);
        for row in 0..18 {
            fill_rect(
                &mut before,
                WIDTH,
                200,
                2200,
                400 + row * 40,
                400 + row * 40 + 20,
                30,
            );
            fill_rect(
                &mut after,
                WIDTH,
                200,
                2200,
                420 + row * 40,
                420 + row * 40 + 20,
                30,
            );
        }
        let before = Thumbnail::from_rgba(&before, WIDTH, HEIGHT)
            .expect("the fixed-size before frame must be valid RGBA");
        let after = Thumbnail::from_rgba(&after, WIDTH, HEIGHT)
            .expect("the fixed-size after frame must be valid RGBA");
        let config = CaptureConfig::default();
        let changed_fraction = before.changed_fraction(&after, config.change_pixel_threshold);

        assert!(
            frame_changed(Some(&before), &after, &config),
            "the small scroll must be detected; changed_fraction={changed_fraction}"
        );
    }

    #[test]
    fn sensor_noise_below_threshold_is_ignored() {
        let before = solid(WIDTH, HEIGHT, 200);
        let after = solid(WIDTH, HEIGHT, 202);
        let before = Thumbnail::from_rgba(&before, WIDTH, HEIGHT)
            .expect("the fixed-size before frame must be valid RGBA");
        let after = Thumbnail::from_rgba(&after, WIDTH, HEIGHT)
            .expect("the fixed-size after frame must be valid RGBA");
        let config = CaptureConfig::default();
        let changed_fraction = before.changed_fraction(&after, config.change_pixel_threshold);

        assert!(
            !frame_changed(Some(&before), &after, &config),
            "sub-threshold sensor noise must be ignored; changed_fraction={changed_fraction}"
        );
    }

    #[test]
    fn changed_fraction_is_zero_for_identical_thumbnails() {
        let frame = solid(WIDTH, HEIGHT, 200);
        let thumbnail = Thumbnail::from_rgba(&frame, WIDTH, HEIGHT)
            .expect("the fixed-size frame must be valid RGBA");

        assert_eq!(
            thumbnail.changed_fraction(&thumbnail, 8),
            0.0,
            "identical thumbnails must have an exactly zero changed fraction"
        );
    }

    #[test]
    fn rgba_length_mismatch_is_an_error() {
        assert!(
            matches!(
                Thumbnail::from_rgba(&[0_u8; 8], 100, 100),
                Err(ImageBufferError::SizeMismatch { .. })
            ),
            "an RGBA buffer with the wrong length must return SizeMismatch"
        );
        assert!(
            matches!(
                Thumbnail::from_rgba(&[], 0, 100),
                Err(ImageBufferError::EmptyImage)
            ),
            "zero width must return EmptyImage"
        );
        assert!(
            matches!(
                Thumbnail::from_rgba(&[], 100, 0),
                Err(ImageBufferError::EmptyImage)
            ),
            "zero height must return EmptyImage"
        );
    }
}
