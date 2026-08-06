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
    /// The dimensions call for more RGBA bytes than this target can address.
    #[error("{width}x{height} calls for more RGBA bytes than this target can address")]
    OversizedImage {
        /// Supplied image width.
        width: u32,
        /// Supplied image height.
        height: u32,
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
    /// display scale, so a higher pixel density does not inflate the score by itself.
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

/// Largest value `Thumbnail::changed_logical_pixels` can return for a monitor of this size: every
/// sampled pixel changed. Returns 0.0 for a scale that is not finite and positive.
pub fn max_logical_pixels(width: u32, height: u32, dpi_scale: f32) -> f64 {
    if !dpi_scale.is_finite() || dpi_scale <= 0.0 {
        return 0.0;
    }

    f64::from(width) * f64::from(height) / f64::from(dpi_scale).powi(2)
}

/// Whether `config.change_area_logical_pixels` can ever be exceeded on a monitor of this size.
///
/// `frame_changed` compares with `>`, so a threshold at or above the monitor's logical area is
/// never satisfied and no pixel difference on that monitor is ever stored again after its first
/// frame — silently, with no error anywhere. What still gets through is a change of dimensions or
/// DPI scale, which `frame_changed` answers before it compares any pixels. `Config::validate`
/// cannot check this because no monitor is known when the config is read, so it has to be asked
/// once per monitor, as they are enumerated. Kept here so bound and the comparison that makes it a
/// bound stay in the same file.
pub fn change_threshold_is_reachable(
    width: u32,
    height: u32,
    dpi_scale: f32,
    config: &crate::config::CaptureConfig,
) -> bool {
    max_logical_pixels(width, height, dpi_scale) > f64::from(config.change_area_logical_pixels)
}

fn rgba_image(rgba: &[u8], width: u32, height: u32) -> Result<image::RgbaImage, ImageBufferError> {
    if width == 0 || height == 0 {
        return Err(ImageBufferError::EmptyImage);
    }

    // Counted in u128 first: `width * height * 4` can pass 2^64, and a wrapped count that
    // happened to match the buffer would carry garbage dimensions past the check below.
    let expected = usize::try_from(u128::from(width) * u128::from(height) * 4)
        .map_err(|_| ImageBufferError::OversizedImage { width, height })?;
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
    use super::{
        ImageBufferError, Thumbnail, change_threshold_is_reachable, frame_changed,
        max_logical_pixels,
    };
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
    fn a_threshold_at_the_monitor_area_can_never_fire() {
        assert_eq!(max_logical_pixels(1920, 1080, 1.0), 2_073_600.0);

        // A threshold in this range does not merely reduce what gets stored; past the first frame
        // nothing but a dimension or DPI change is ever stored again — silently, with no error.
        let config = |change_area_logical_pixels| CaptureConfig {
            change_area_logical_pixels,
            ..CaptureConfig::default()
        };
        assert!(!change_threshold_is_reachable(
            1920,
            1080,
            1.0,
            &config(2_073_600)
        ));
        assert!(change_threshold_is_reachable(
            1920,
            1080,
            1.0,
            &config(2_073_599)
        ));

        assert_eq!(max_logical_pixels(2560, 1440, 2.0), 921_600.0);
        assert!(change_threshold_is_reachable(
            2560,
            1440,
            2.0,
            &CaptureConfig::default()
        ));
        assert!(!change_threshold_is_reachable(
            2560,
            1440,
            2.0,
            &config(921_600)
        ));
    }

    #[test]
    fn an_unusable_display_scale_makes_no_threshold_reachable() {
        let config = CaptureConfig {
            change_area_logical_pixels: 0,
            ..CaptureConfig::default()
        };

        // Failing closed is right here: a scale we cannot use must surface as a startup error.
        for dpi_scale in [0.0, -1.0, f32::NAN] {
            assert_eq!(max_logical_pixels(1920, 1080, dpi_scale), 0.0);
            assert!(!change_threshold_is_reachable(
                1920, 1080, dpi_scale, &config
            ));
        }
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
    fn the_same_edit_is_detected_in_each_measured_configuration() {
        // Measured logical minima: 853, 1138, 1238, 1125, 900 — all above the 600 default, against a
        // source-pixel spread that put 5120x2880 @200% carets above Full HD ten-character edits.
        // 3840x2160 appears at both 100% and 150% on purpose: unscaled 4K is an ordinary setup on a
        // large panel, and which configuration is tightest does not follow from the pixel count
        // alone.
        // Bounds are the measured range over every glyph offset within one full sampling-phase
        // period — `width / gcd(width, 256)` by `height / gcd(height, 144)` source pixels, which
        // a fractional stride stretches to 683x16 at 1366x768 — rounded outward, so a value on a
        // measured edge is inside. Nothing in the tree reruns that sweep and the loop below
        // places its glyph at one offset, so a change to the resize filter, the glyph fixture or
        // the pixel threshold means deriving these again by hand.
        for (width, height, scale, logical_min, logical_max) in [
            (1024, 768, 1.0, 853.0, 1302.0),
            (1366, 768, 1.0, 1138.0, 1480.0),
            (1920, 1080, 1.0, 1237.0, 1857.0),
            (3840, 2160, 1.0, 1125.0, 2700.0),
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
    fn a_caret_is_ignored_in_each_measured_configuration() {
        // Measured logical maxima: 149, 171, 225, 450, 400. 3840x2160 at 100% is the tightest
        // configuration measured — 450 against a 600 default — and the only realistic one where
        // a caret can measure exactly 0, because at that sample granularity one character
        // can split across four samples with none of them crossing the per-pixel threshold.
        // Bounds are the measured range over every glyph offset within one full sampling-phase
        // period — `width / gcd(width, 256)` by `height / gcd(height, 144)` source pixels, which
        // a fractional stride stretches to 683x16 at 1366x768 — rounded outward, so a value on a
        // measured edge is inside. Nothing in the tree reruns that sweep and the loop below
        // places its glyph at one offset, so a change to the resize filter, the glyph fixture or
        // the pixel threshold means deriving these again by hand.
        for (width, height, scale, logical_min, logical_max) in [
            (1024, 768, 1.0, 85.0, 150.0),
            (1366, 768, 1.0, 85.0, 171.0),
            (1920, 1080, 1.0, 112.0, 225.0),
            (3840, 2160, 1.0, 0.0, 450.0),
            (3840, 2160, 1.5, 99.0, 400.0),
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
    fn the_worst_phase_on_4k_unscaled_still_separates_a_caret_from_typing() {
        // 3840x2160 at 100% has the least room of any measured configuration, and its sample
        // period is exactly 15x15, so all 225 phases were enumerated offline. These two offsets are
        // the true extremes: a caret peaks at 450 logical pixels and ten characters bottom out at
        // 1125, which is the 450 < 600 < 1125 separation the default rests on. The tables above draw
        // at left = 100, where the same fixtures measure a comfortable 225 and 1350 and would keep
        // passing even if the real margin had closed. A caret also drops to 0 at
        // left = 102, top = height / 2 + 3, where one character splits across four samples and none
        // of them crosses the per-pixel threshold.
        let width = 3840;
        let height = 2160;
        let config = CaptureConfig::default();
        let background = solid(width, height, 200);
        let before = Thumbnail::from_rgba(&background, width, height, 1.0)
            .expect("the 4K before frame must be valid RGBA");

        let mut caret = background.clone();
        glyph(&mut caret, width, 102, 0, height / 2, 1.0);
        let caret = Thumbnail::from_rgba(&caret, width, height, 1.0)
            .expect("the 4K caret frame must be valid RGBA");
        let caret_pixels = before.changed_logical_pixels(&caret, config.change_pixel_threshold);

        assert!(
            (449.0..=451.0).contains(&caret_pixels),
            "a caret at its worst phase must still measure about 450 logical pixels, measured {caret_pixels}"
        );
        assert!(
            !frame_changed(Some(&before), &caret, &config),
            "a caret at its worst phase must still be ignored; changed_logical_pixels={caret_pixels}"
        );

        let mut typing = background;
        for column in 0..10 {
            glyph(&mut typing, width, 103, column, height / 2, 1.0);
        }
        let typing = Thumbnail::from_rgba(&typing, width, height, 1.0)
            .expect("the 4K typing frame must be valid RGBA");
        let typing_pixels = before.changed_logical_pixels(&typing, config.change_pixel_threshold);

        assert!(
            (1124.0..=1126.0).contains(&typing_pixels),
            "ten characters at their worst phase must still measure about 1125 logical pixels, measured {typing_pixels}"
        );
        assert!(
            frame_changed(Some(&before), &typing, &config),
            "ten characters at their worst phase must still be detected; changed_logical_pixels={typing_pixels}"
        );
    }

    #[test]
    fn sampled_positions_do_not_change_the_verdict() {
        let width = 1366;
        let height = 768;
        let before = solid(width, height, 200);
        let before = Thumbnail::from_rgba(&before, width, height, 1.0)
            .expect("the before frame must be valid RGBA");
        let config = CaptureConfig::default();

        // A single-offset measurement is a sample, not a bound: this fixture varies about 1.3x
        // across its full 683x16 sampling-phase period, of which this loop samples a 6x6 block.
        // An earlier default was validated against one such sample as if it were an upper bound.
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
    fn a_non_positive_or_nan_display_scale_is_rejected() {
        let frame = solid(1, 1, 200);

        // NaN never equals itself, so `frame_changed`'s scale comparison would answer "changed"
        // on every tick and store every frame.
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

    #[test]
    fn dimensions_wider_than_the_address_space_are_refused() {
        // 2^31 * 2^31 * 4 is exactly 2^64 — one past what a byte count can spell — and wrapped
        // to zero it matched this empty buffer, carrying the pair past the size check into the
        // panic behind it.
        let error = Thumbnail::from_rgba(&[], 1 << 31, 1 << 31, 1.0)
            .expect_err("dimensions past the address space must be refused, not panic");
        assert!(
            matches!(error, ImageBufferError::OversizedImage { .. }),
            "{error:?}"
        );
    }
}
