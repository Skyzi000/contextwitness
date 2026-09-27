#![deny(unsafe_op_in_unsafe_fn)]
//! Optical character recognition functionality for ContextWitness.

mod windows;

pub use self::windows::{WindowsOcr, init_runtime};

use cw_core::model::OcrStatus;

pub struct OcrOutcome {
    pub status: OcrStatus,
    pub text: Option<String>,
    pub error: Option<String>,
    pub langs: Vec<String>,
}

/// The OCR boundary: the daemon reads text through this and nothing else, so a
/// different backend is a new implementor, not a new call site.
pub trait OcrEngine {
    /// Recognize text in a tightly packed BGRA8 frame. Never panics; failures come back as
    /// `OcrStatus::Failed` with the error spelled out.
    fn recognize(&self, bgra: &[u8], width: u32, height: u32, languages: &[String]) -> OcrOutcome;
}
