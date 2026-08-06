//! The Windows OCR backend: WinRT `Windows.Media.Ocr` behind the crate's trait.

use cw_core::model::OcrStatus;
use windows::Globalization::Language;
use windows::Graphics::Imaging::{BitmapPixelFormat, SoftwareBitmap};
use windows::Media::Ocr::OcrEngine as WinRtOcrEngine;
use windows::Security::Cryptography::CryptographicBuffer;
use windows::Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize};
use windows::core::HSTRING;

use crate::{OcrEngine, OcrOutcome};

/// Initialize the Windows Runtime on the calling thread. Idempotent; `recognize` calls it itself.
pub fn init_runtime() {
    unsafe {
        let _ = RoInitialize(RO_INIT_MULTITHREADED);
    }
}

/// The engine Windows ships. Stateless: the WinRT engine is cheap to look up and per-language, so
/// each recognition resolves its own.
pub struct WindowsOcr;

impl OcrEngine for WindowsOcr {
    fn recognize(&self, bgra: &[u8], width: u32, height: u32, languages: &[String]) -> OcrOutcome {
        // RoInitialize is per-thread, so worker threads reach the runtime through here.
        init_runtime();
        match recognize_inner(bgra, width, height, languages) {
            Ok(outcome) => outcome,
            Err(error) => OcrOutcome {
                status: OcrStatus::Failed,
                text: None,
                error: Some(error),
                langs: Vec::new(),
            },
        }
    }
}

fn recognize_inner(
    bgra: &[u8],
    width: u32,
    height: u32,
    languages: &[String],
) -> Result<OcrOutcome, String> {
    let engine = engine_for(languages)?;
    let langs = engine
        .RecognizerLanguage()
        .and_then(|language| language.LanguageTag())
        .map(|tag| vec![tag.to_string()])
        .unwrap_or_default();

    let max = WinRtOcrEngine::MaxImageDimension().map_err(|error| error.to_string())?;
    let shrunk;
    let (data, width, height) = if width > max || height > max {
        shrunk = shrink(bgra, width, height, max)?;
        (shrunk.0.as_slice(), shrunk.1, shrunk.2)
    } else {
        (bgra, width, height)
    };

    let buffer = CryptographicBuffer::CreateFromByteArray(data)
        .map_err(|error| format!("building the pixel buffer failed: {error}"))?;
    let bitmap = SoftwareBitmap::CreateCopyFromBuffer(
        &buffer,
        BitmapPixelFormat::Bgra8,
        width as i32,
        height as i32,
    )
    .map_err(|error| format!("building the bitmap failed: {error}"))?;
    let result = engine
        .RecognizeAsync(&bitmap)
        .and_then(|operation| operation.join())
        .map_err(|error| format!("recognition failed: {error}"))?;

    let mut lines = Vec::new();
    for line in result
        .Lines()
        .map_err(|error| format!("reading recognized lines failed: {error}"))?
    {
        lines.push(
            line.Text()
                .map_err(|error| format!("reading a recognized line failed: {error}"))?
                .to_string(),
        );
    }
    let text = lines.join("\n");
    if text.trim().is_empty() {
        Ok(OcrOutcome {
            status: OcrStatus::NoText,
            text: None,
            error: None,
            langs,
        })
    } else {
        Ok(OcrOutcome {
            status: OcrStatus::Succeeded,
            text: Some(text),
            error: None,
            langs,
        })
    }
}

/// First configured language that Windows actually ships an OCR engine for, falling back to the
/// user's profile languages when the config names none that are installed.
fn engine_for(languages: &[String]) -> Result<WinRtOcrEngine, String> {
    for tag in languages {
        if let Ok(engine) = Language::CreateLanguage(&HSTRING::from(tag.as_str()))
            .and_then(|language| WinRtOcrEngine::TryCreateFromLanguage(&language))
        {
            return Ok(engine);
        }
    }
    WinRtOcrEngine::TryCreateFromUserProfileLanguages()
        .map_err(|error| format!("no OCR engine for the user profile languages: {error}"))
}

fn shrink(bgra: &[u8], width: u32, height: u32, max: u32) -> Result<(Vec<u8>, u32, u32), String> {
    let image: image::RgbaImage = image::ImageBuffer::from_raw(width, height, bgra.to_vec())
        .ok_or("the pixel buffer does not match its dimensions")?;
    let scale = max as f32 / width.max(height) as f32;
    let new_width = ((width as f32 * scale) as u32).clamp(1, max);
    let new_height = ((height as f32 * scale) as u32).clamp(1, max);
    let resized = image::imageops::resize(
        &image,
        new_width,
        new_height,
        image::imageops::FilterType::Triangle,
    );
    Ok((resized.into_raw(), new_width, new_height))
}
