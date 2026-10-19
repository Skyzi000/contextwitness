#![deny(unsafe_op_in_unsafe_fn)]
//! Core types and interfaces for ContextWitness.

pub mod atomic_file;
pub mod change;
pub mod config;
pub mod episode;
pub mod model;
pub mod privacy;

/// Hangul syllables are not included, so Korean keeps its spaces.
pub fn is_cjk(c: char) -> bool {
    matches!(c,
        '\u{2E80}'..='\u{9FFF}'
        | '\u{F900}'..='\u{FAFF}'
        | '\u{FE30}'..='\u{FE4F}'
        | '\u{FF00}'..='\u{FFEF}'
        | '\u{20000}'..='\u{3FFFF}')
}
