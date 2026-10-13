use std::collections::VecDeque;
use std::path::Path;

use chrono::{DateTime, TimeDelta, Utc};
use sherpa_onnx::{SileroVadModelConfig, VadModelConfig, VoiceActivityDetector};

pub(crate) const SAMPLE_RATE: usize = 16_000;
const WINDOW: usize = 512;
const BUFFER_SECONDS: f32 = 30.0;
pub(crate) const FORCED_CUT_SAMPLES: i64 = 22 * SAMPLE_RATE as i64;
const RESET_AFTER_SAMPLES: i64 = 1 << 30;
const SETTLE_SAMPLES: usize = 2 * WINDOW;
const MIN_SPEECH_SAMPLES: usize = SAMPLE_RATE / 20;
const LEAD_SAMPLES: usize = 4 * SAMPLE_RATE / 10;
// The detector dates a start two windows and the minimum speech before its newest whole window.
const RECENT_SAMPLES: usize = LEAD_SAMPLES + MIN_SPEECH_SAMPLES + 4 * WINDOW;

pub(crate) struct Segment {
    pub started_at: DateTime<Utc>,
    pub samples: Vec<f32>,
}

pub(crate) fn duration(samples: usize) -> TimeDelta {
    TimeDelta::microseconds((samples * 1_000_000 / SAMPLE_RATE) as i64)
}

pub(crate) struct Segmenter {
    vad: VoiceActivityDetector,
    clock: Clock,
    fed: i64,
    quiet: usize,
    open_since: Option<i64>,
    pub(crate) forced_cut: i64,
    lead: Lead,
}

impl Segmenter {
    pub(crate) fn new(model: &Path) -> Option<Self> {
        let config = VadModelConfig {
            silero_vad: SileroVadModelConfig {
                model: Some(model.to_string_lossy().into_owned()),
                threshold: 0.5,
                min_silence_duration: 0.5,
                min_speech_duration: MIN_SPEECH_SAMPLES as f32 / SAMPLE_RATE as f32,
                window_size: WINDOW as i32,
                max_speech_duration: 20.0,
            },
            sample_rate: SAMPLE_RATE as i32,
            num_threads: 1,
            ..Default::default()
        };
        Some(Self {
            vad: VoiceActivityDetector::create(&config, BUFFER_SECONDS)?,
            clock: Clock::new(),
            fed: 0,
            quiet: SETTLE_SAMPLES,
            open_since: None,
            forced_cut: FORCED_CUT_SAMPLES,
            lead: Lead::default(),
        })
    }

    pub(crate) fn push(&mut self, samples: &[f32], at: DateTime<Utc>, out: &mut Vec<Segment>) {
        let silent = samples.iter().all(|sample| *sample == 0.0);
        if silent && self.quiet >= SETTLE_SAMPLES && !self.vad.detected() {
            self.lead.heard(samples);
            return;
        }
        self.quiet = if silent {
            self.quiet + samples.len()
        } else {
            0
        };

        for (index, window) in samples.chunks(WINDOW).enumerate() {
            let open = self.vad.detected();
            self.clock
                .feed(self.fed, at + duration(index * WINDOW), open);
            self.vad.accept_waveform(window);
            self.fed += window.len() as i64;
            self.lead.heard(window);
            if !open && self.vad.detected() {
                self.lead.opened(self.fed);
            }

            if self.vad.detected() {
                let since = *self.open_since.get_or_insert(self.fed);
                if self.fed - since >= self.forced_cut {
                    self.vad.flush();
                }
            }
            if !self.vad.detected() {
                self.open_since = None;
            }
            self.drain(out);
        }

        // The detector counts samples in an i32.
        if self.fed >= RESET_AFTER_SAMPLES && !self.vad.detected() {
            self.vad.reset();
            self.clock = Clock::new();
            self.fed = 0;
            self.lead = Lead::default();
        }
    }

    pub(crate) fn utterance_open(&self) -> bool {
        self.vad.detected()
    }

    pub(crate) fn flush(&mut self, out: &mut Vec<Segment>) {
        self.vad.flush();
        self.drain(out);
    }

    fn drain(&mut self, out: &mut Vec<Segment>) {
        while let Some(segment) = self.vad.front() {
            self.vad.pop();
            let start = segment.start() as i64;
            let lead = self.lead.before(start, segment.samples().len());
            out.push(Segment {
                started_at: self.clock.time_of(start - lead.len() as i64),
                samples: [lead, segment.samples()].concat(),
            });
        }
    }
}

/// The detector's cut begins where it first took the sound for speech, so a consonant or a soft
/// start before that is missing from it.
#[derive(Default)]
struct Lead {
    recent: VecDeque<f32>,
    kept: Vec<f32>,
    kept_end: i64,
    covered: i64,
}

impl Lead {
    fn heard(&mut self, samples: &[f32]) {
        self.recent.extend(samples);
        let excess = self.recent.len().saturating_sub(RECENT_SAMPLES);
        self.recent.drain(..excess);
    }

    fn opened(&mut self, fed: i64) {
        self.kept.clear();
        self.kept.extend(&self.recent);
        self.kept_end = fed;
    }

    fn before(&mut self, start: i64, cut: usize) -> &[f32] {
        let kept = self.kept.len() as i64;
        let to = (kept - (self.kept_end - start)).clamp(0, kept);
        let from = (to - (start - self.covered).clamp(0, LEAD_SAMPLES as i64)).max(0);
        self.covered = start + cut as i64;
        &self.kept[from as usize..to as usize]
    }
}

/// The detector sees only the samples that were fed, and audio can be lost while an utterance is
/// open, so neither a count from the first sample nor a later packet dates its start.
struct Clock {
    anchor: (i64, DateTime<Utc>),
}

impl Clock {
    fn new() -> Self {
        Self {
            anchor: (0, DateTime::UNIX_EPOCH),
        }
    }

    fn feed(&mut self, index: i64, at: DateTime<Utc>, open: bool) {
        if !open {
            self.anchor = (index, at);
        }
    }

    fn time_of(&self, index: i64) -> DateTime<Utc> {
        let (base, at) = self.anchor;
        at + TimeDelta::microseconds((index - base) * 1_000_000 / SAMPLE_RATE as i64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(milliseconds: i64) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-07-25T01:00:00Z")
            .expect("the fixed test timestamp is valid RFC 3339")
            .with_timezone(&Utc)
            + TimeDelta::milliseconds(milliseconds)
    }

    #[test]
    fn an_utterance_is_dated_from_the_last_packet_before_it_opened() {
        let mut clock = Clock::new();
        clock.feed(0, at(0), false);
        clock.feed(16_000_000, at(1_000_500), false);
        clock.feed(16_000_160, at(1_000_510), true);
        assert_eq!(
            clock.time_of(16_000_080),
            at(1_000_505),
            "not counted from the first sample, a thousand seconds of drift ago"
        );
    }

    #[test]
    fn a_start_the_detector_backdates_is_dated_back_from_that_packet() {
        let mut clock = Clock::new();
        clock.feed(0, at(0), false);
        clock.feed(16_000, at(60_000), false);
        assert_eq!(clock.time_of(24_000), at(60_500));
        assert_eq!(clock.time_of(15_200), at(59_950));
    }

    #[test]
    fn an_open_utterance_keeps_the_dating_it_opened_under() {
        let mut clock = Clock::new();
        clock.feed(0, at(0), false);
        clock.feed(160, at(10), true);
        clock.feed(32_000, at(2_300), true);
        assert_eq!(
            clock.time_of(8_000),
            at(500),
            "audio lost in the middle of an utterance does not move its start"
        );
        clock.feed(32_160, at(2_310), false);
        assert_eq!(clock.time_of(48_000), at(3_300));
    }

    fn ramp(range: std::ops::Range<usize>) -> Vec<f32> {
        range.map(|index| index as f32).collect()
    }

    #[test]
    fn an_utterance_is_led_by_the_sound_just_before_it() {
        let mut lead = Lead::default();
        lead.heard(&ramp(0..20_000));
        lead.opened(20_000);
        assert_eq!(
            lead.before(19_000, 5_000),
            ramp(19_000 - LEAD_SAMPLES..19_000)
        );
    }

    #[test]
    fn a_stream_that_just_began_leads_with_what_there_is() {
        let mut lead = Lead::default();
        lead.heard(&ramp(0..1_000));
        lead.opened(1_000);
        assert_eq!(lead.before(700, 5_000), ramp(0..700));
    }

    #[test]
    fn the_lead_does_not_repeat_the_utterance_before_it() {
        let mut lead = Lead::default();
        lead.heard(&ramp(0..20_000));
        lead.opened(20_000);
        lead.before(10_000, 8_000);

        lead.heard(&ramp(20_000..21_000));
        lead.opened(21_000);
        assert_eq!(lead.before(19_000, 3_000), ramp(18_000..19_000));

        lead.heard(&ramp(21_000..22_512));
        lead.opened(22_512);
        assert!(
            lead.before(22_000, 512).is_empty(),
            "an utterance cut by force is followed without a gap"
        );
    }

    #[test]
    fn silence_the_detector_was_not_fed_still_takes_its_time_in_the_lead() {
        let mut lead = Lead::default();
        lead.heard(&ramp(0..9_000));
        lead.heard(&[0.0; 2_000]);
        lead.heard(&ramp(9_000..9_512));
        lead.opened(9_512);
        let led = lead.before(9_200, 1_000);
        let (older, rest) = led.split_at(LEAD_SAMPLES - 2_200);
        let (silence, newest) = rest.split_at(2_000);
        assert_eq!(older, ramp(9_000 - older.len()..9_000));
        assert!(silence.iter().all(|sample| *sample == 0.0));
        assert_eq!(newest, ramp(9_000..9_200));
    }
}
