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
    /// The supplied display scale is not a usable positive number.
    #[error("display scale must be finite and greater than zero, got {dpi_scale}")]
    InvalidDpiScale {
        /// Rejected scale value.
        dpi_scale: f32,
    },
}

/// Downscaled grayscale view used to estimate changed logical pixels, kept between ticks
/// instead of the full frame (a few tens of KiB per monitor rather than tens of MiB).
#[derive(Debug, Clone, PartialEq)]
pub struct Thumbnail {
    luma: Vec<u8>,
    source_width: u32,
    source_height: u32,
    dpi_scale: f32,
}

impl Thumbnail {
    /// Build from tightly packed RGBA8. Resizes to the fixed thumbnail size with
    /// [`image::imageops::FilterType::Triangle`], then converts to luma8.
    pub fn from_rgba(
        rgba: &[u8],
        width: u32,
        height: u32,
        dpi_scale: f32,
    ) -> Result<Thumbnail, ImageBufferError> {
        if !dpi_scale.is_finite() || dpi_scale <= 0.0 {
            return Err(ImageBufferError::InvalidDpiScale { dpi_scale });
        }

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
            source_width: width,
            source_height: height,
            dpi_scale,
        })
    }

    /// Source frame dimensions this thumbnail was downscaled from.
    pub fn source_dimensions(&self) -> (u32, u32) {
        (self.source_width, self.source_height)
    }

    /// Display scale this thumbnail was captured at.
    pub fn dpi_scale(&self) -> f32 {
        self.dpi_scale
    }

    /// Estimated number of SOURCE pixels that changed, using this thumbnail's source dimensions.
    /// The thumbnail is a fixed size, so one sample stands for `source_width * source_height /
    /// (THUMBNAIL_WIDTH * THUMBNAIL_HEIGHT)` screen pixels.
    pub fn changed_source_pixels(&self, other: &Thumbnail, pixel_threshold: u8) -> f64 {
        self.changed_fraction(other, pixel_threshold)
            * f64::from(self.source_width)
            * f64::from(self.source_height)
    }

    /// Estimated number of LOGICAL pixels that changed — source pixels divided by the square of the
    /// display scale, so the same edit scores the same on a 1080p screen at 100% and a 4K screen at
    /// 200%.
    pub fn changed_logical_pixels(&self, other: &Thumbnail, pixel_threshold: u8) -> f64 {
        self.changed_source_pixels(other, pixel_threshold) / (self.dpi_scale as f64).powi(2)
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

/// Whether this frame must be stored and OCR-ed based on changed logical pixels.
/// The first frame, a source-resolution change, and a display-scale change always count as changed.
pub fn frame_changed(
    previous: Option<&Thumbnail>,
    current: &Thumbnail,
    config: &crate::config::CaptureConfig,
) -> bool {
    let Some(previous) = previous else {
        return true;
    };
    if previous.source_dimensions() != current.source_dimensions() {
        return true;
    }
    if previous.dpi_scale() != current.dpi_scale() {
        return true;
    }

    previous.changed_logical_pixels(current, config.change_pixel_threshold)
        > f64::from(config.change_area_logical_pixels)
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
    const INK: u8 = 30;

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

    /// One monospace glyph in an 8x16 cell scaled by the display scale `s`: at 200% Windows renders
    /// the same character into 16x32 physical pixels, which is what the capture API returns.
    /// A filled rectangle is NOT a substitute — it overstates a text edit by roughly 2.5x.
    fn glyph(buf: &mut [u8], width: u32, left: u32, column: u32, top: u32, s: f64) {
        let k = |v: f64| (v * s).round() as u32;
        let x = left + column * k(8.0);
        fill_rect(
            buf,
            width,
            x + k(2.0),
            x + k(4.0),
            top + k(2.0),
            top + k(14.0),
            INK,
        );
        fill_rect(
            buf,
            width,
            x + k(1.0),
            x + k(6.0),
            top + k(8.0),
            top + k(10.0),
            INK,
        );
    }

    #[test]
    fn identical_frames_are_unchanged() {
        let before = solid(WIDTH, HEIGHT, 200);
        let after = solid(WIDTH, HEIGHT, 200);
        let before = Thumbnail::from_rgba(&before, WIDTH, HEIGHT, 1.0)
            .expect("the fixed-size before frame must be valid RGBA");
        let after = Thumbnail::from_rgba(&after, WIDTH, HEIGHT, 1.0)
            .expect("the fixed-size after frame must be valid RGBA");

        assert!(
            !frame_changed(Some(&before), &after, &CaptureConfig::default()),
            "identical frames must not be treated as changed"
        );
    }

    #[test]
    fn first_frame_is_always_changed() {
        let frame = solid(WIDTH, HEIGHT, 200);
        let thumbnail = Thumbnail::from_rgba(&frame, WIDTH, HEIGHT, 1.0)
            .expect("the fixed-size frame must be valid RGBA");

        assert!(
            frame_changed(None, &thumbnail, &CaptureConfig::default()),
            "the first frame for a monitor must always be treated as changed"
        );
    }

    #[test]
    fn ten_characters_typed_are_detected() {
        let before = solid(WIDTH, HEIGHT, 200);
        let mut after = before.clone();
        for column in 0..10 {
            glyph(&mut after, WIDTH, 100, column, HEIGHT / 2, 1.0);
        }
        let before = Thumbnail::from_rgba(&before, WIDTH, HEIGHT, 1.0)
            .expect("the fixed-size before frame must be valid RGBA");
        let after = Thumbnail::from_rgba(&after, WIDTH, HEIGHT, 1.0)
            .expect("the fixed-size after frame must be valid RGBA");
        let config = CaptureConfig::default();
        let changed_source_pixels =
            before.changed_source_pixels(&after, config.change_pixel_threshold);

        assert!(
            frame_changed(Some(&before), &after, &config),
            "ten typed characters must be detected; changed_source_pixels={changed_source_pixels}"
        );
    }

    #[test]
    fn a_blinking_cursor_alone_is_ignored() {
        let before = solid(WIDTH, HEIGHT, 200);
        let mut after = before.clone();
        glyph(&mut after, WIDTH, 100, 0, HEIGHT / 2, 1.0);
        let before = Thumbnail::from_rgba(&before, WIDTH, HEIGHT, 1.0)
            .expect("the fixed-size before frame must be valid RGBA");
        let after = Thumbnail::from_rgba(&after, WIDTH, HEIGHT, 1.0)
            .expect("the fixed-size after frame must be valid RGBA");
        let config = CaptureConfig::default();
        let changed_source_pixels =
            before.changed_source_pixels(&after, config.change_pixel_threshold);

        // A caret must not keep the pipeline busy.
        assert!(
            !frame_changed(Some(&before), &after, &config),
            "a blinking cursor alone must be ignored; changed_source_pixels={changed_source_pixels}"
        );
    }

    #[test]
    fn an_eighty_character_line_is_detected() {
        let before = solid(WIDTH, HEIGHT, 200);
        let mut after = before.clone();
        for column in 0..80 {
            glyph(&mut after, WIDTH, 100, column, 1400, 1.0);
        }
        let before = Thumbnail::from_rgba(&before, WIDTH, HEIGHT, 1.0)
            .expect("the fixed-size before frame must be valid RGBA");
        let after = Thumbnail::from_rgba(&after, WIDTH, HEIGHT, 1.0)
            .expect("the fixed-size after frame must be valid RGBA");
        let config = CaptureConfig::default();
        let changed_source_pixels =
            before.changed_source_pixels(&after, config.change_pixel_threshold);

        assert!(
            frame_changed(Some(&before), &after, &config),
            "an eighty-character line must be detected; changed_source_pixels={changed_source_pixels}"
        );
    }

    #[test]
    fn the_same_edit_is_detected_at_every_display_scale() {
        // Measured logical values: 853, 1138, 1238, 900 — all above the 600 default, against a
        // source-pixel spread that put 5120x2880 @200% carets above Full HD ten-character edits.
        // Bounds are the measured ten-character range over a full thumbnail-sample-period offset sweep, rounded
        // outward: a value sitting exactly on a measured edge must be inside. They pin the
        // calibration the default rests on — a change to the resize filter, the glyph fixture or
        // the pixel threshold moves these numbers and should fail here rather than quietly shift
        // how much text it takes to trigger a capture.
        for (width, height, scale, logical_min, logical_max) in [
            (1024, 768, 1.0, 853.0, 1302.0),
            (1366, 768, 1.0, 1138.0, 1480.0),
            (1920, 1080, 1.0, 1237.0, 1857.0),
            (3840, 2160, 1.5, 900.0, 1800.0),
        ] {
            let before = solid(width, height, 200);
            let mut after = before.clone();
            for column in 0..10 {
                glyph(&mut after, width, 100, column, height / 2, scale);
            }
            let before = Thumbnail::from_rgba(&before, width, height, scale as f32)
                .expect("the before frame must be valid RGBA");
            let after = Thumbnail::from_rgba(&after, width, height, scale as f32)
                .expect("the after frame must be valid RGBA");
            let config = CaptureConfig::default();
            let changed_source_pixels =
                before.changed_source_pixels(&after, config.change_pixel_threshold);
            let changed_logical_pixels =
                before.changed_logical_pixels(&after, config.change_pixel_threshold);

            assert!(
                (logical_min..=logical_max).contains(&changed_logical_pixels),
                "ten typed characters at {width}x{height} @{scale}x measured {changed_logical_pixels} logical pixels ({changed_source_pixels} source pixels)"
            );
            assert!(
                frame_changed(Some(&before), &after, &config),
                "ten typed characters must be detected at {width}x{height} @{scale}x; changed_logical_pixels={changed_logical_pixels}; changed_source_pixels={changed_source_pixels}"
            );
        }
    }

    #[test]
    fn a_caret_is_ignored_at_every_display_scale() {
        // Measured logical maxima: 149, 171, 225, 400.
        // Bounds are the measured caret range over a full thumbnail-sample-period offset sweep, rounded
        // outward: a value sitting exactly on a measured edge must be inside. They pin the
        // calibration the default rests on — a change to the resize filter, the glyph fixture or
        // the pixel threshold moves these numbers and should fail here rather than quietly shift
        // how much text it takes to trigger a capture.
        for (width, height, scale, logical_min, logical_max) in [
            (1024, 768, 1.0, 85.0, 150.0),
            (1366, 768, 1.0, 85.0, 171.0),
            (1920, 1080, 1.0, 112.0, 225.0),
            (3840, 2160, 1.5, 100.0, 400.0),
        ] {
            let before = solid(width, height, 200);
            let mut after = before.clone();
            glyph(&mut after, width, 100, 0, height / 2, scale);
            let before = Thumbnail::from_rgba(&before, width, height, scale as f32)
                .expect("the before frame must be valid RGBA");
            let after = Thumbnail::from_rgba(&after, width, height, scale as f32)
                .expect("the after frame must be valid RGBA");
            let config = CaptureConfig::default();
            let changed_source_pixels =
                before.changed_source_pixels(&after, config.change_pixel_threshold);
            let changed_logical_pixels =
                before.changed_logical_pixels(&after, config.change_pixel_threshold);

            assert!(
                (logical_min..=logical_max).contains(&changed_logical_pixels),
                "a caret at {width}x{height} @{scale}x measured {changed_logical_pixels} logical pixels ({changed_source_pixels} source pixels)"
            );
            assert!(
                !frame_changed(Some(&before), &after, &config),
                "a caret must be ignored at {width}x{height} @{scale}x; changed_logical_pixels={changed_logical_pixels}; changed_source_pixels={changed_source_pixels}"
            );
        }
    }

    #[test]
    fn position_does_not_change_the_verdict() {
        let width = 1366;
        let height = 768;
        let before = solid(width, height, 200);
        let before = Thumbnail::from_rgba(&before, width, height, 1.0)
            .expect("the before frame must be valid RGBA");
        let config = CaptureConfig::default();

        // A single-offset measurement is a sample, not a bound: this fixture varies about 1.3x
        // across one thumbnail-sample period, which is how an earlier default was validated against
        // a number that was never an upper bound.
        for left in 100..=105 {
            for top in height / 2..=height / 2 + 5 {
                let mut after = solid(width, height, 200);
                for column in 0..10 {
                    glyph(&mut after, width, left, column, top, 1.0);
                }
                let after = Thumbnail::from_rgba(&after, width, height, 1.0)
                    .expect("the after frame must be valid RGBA");
                let changed_logical_pixels =
                    before.changed_logical_pixels(&after, config.change_pixel_threshold);

                assert!(
                    frame_changed(Some(&before), &after, &config),
                    "ten typed characters must be detected at left={left}, top={top}; changed_logical_pixels={changed_logical_pixels}"
                );
            }
        }
    }

    #[test]
    fn a_resolution_change_counts_as_changed() {
        let before = solid(1920, 1080, 200);
        let after = solid(2560, 1440, 200);
        let before = Thumbnail::from_rgba(&before, 1920, 1080, 1.0)
            .expect("the 1920x1080 before frame must be valid RGBA");
        let after = Thumbnail::from_rgba(&after, 2560, 1440, 1.0)
            .expect("the 2560x1440 after frame must be valid RGBA");

        assert!(
            frame_changed(Some(&before), &after, &CaptureConfig::default()),
            "a source-resolution change must invalidate the stored frame"
        );
    }

    #[test]
    fn a_display_scale_change_counts_as_changed() {
        let frame = solid(WIDTH, HEIGHT, 200);
        let before = Thumbnail::from_rgba(&frame, WIDTH, HEIGHT, 1.0)
            .expect("the fixed-size before frame must be valid RGBA");
        let after = Thumbnail::from_rgba(&frame, WIDTH, HEIGHT, 1.25)
            .expect("the fixed-size after frame must be valid RGBA");

        assert!(
            frame_changed(Some(&before), &after, &CaptureConfig::default()),
            "a display scale change must invalidate the stored frame"
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
        let before = Thumbnail::from_rgba(&before, WIDTH, HEIGHT, 1.0)
            .expect("the fixed-size before frame must be valid RGBA");
        let after = Thumbnail::from_rgba(&after, WIDTH, HEIGHT, 1.0)
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
        let before = Thumbnail::from_rgba(&before, WIDTH, HEIGHT, 1.0)
            .expect("the fixed-size before frame must be valid RGBA");
        let after = Thumbnail::from_rgba(&after, WIDTH, HEIGHT, 1.0)
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
        let thumbnail = Thumbnail::from_rgba(&frame, WIDTH, HEIGHT, 1.0)
            .expect("the fixed-size frame must be valid RGBA");

        assert_eq!(
            thumbnail.changed_fraction(&thumbnail, 8),
            0.0,
            "identical thumbnails must have an exactly zero changed fraction"
        );
    }

    #[test]
    fn a_non_positive_or_nan_display_scale_is_rejected() {
        let frame = solid(1, 1, 200);

        // A NaN scale would silently stop all capture rather than fail.
        for dpi_scale in [0.0, -1.0, f32::NAN] {
            assert!(
                matches!(
                    Thumbnail::from_rgba(&frame, 1, 1, dpi_scale),
                    Err(ImageBufferError::InvalidDpiScale { .. })
                ),
                "display scale {dpi_scale} must return InvalidDpiScale"
            );
        }
    }

    #[test]
    fn rgba_length_mismatch_is_an_error() {
        assert!(
            matches!(
                Thumbnail::from_rgba(&[0_u8; 8], 100, 100, 1.0),
                Err(ImageBufferError::SizeMismatch { .. })
            ),
            "an RGBA buffer with the wrong length must return SizeMismatch"
        );
        assert!(
            matches!(
                Thumbnail::from_rgba(&[], 0, 100, 1.0),
                Err(ImageBufferError::EmptyImage)
            ),
            "zero width must return EmptyImage"
        );
        assert!(
            matches!(
                Thumbnail::from_rgba(&[], 100, 0, 1.0),
                Err(ImageBufferError::EmptyImage)
            ),
            "zero height must return EmptyImage"
        );
    }
}
