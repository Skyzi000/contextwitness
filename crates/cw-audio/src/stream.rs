use std::error::Error;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::thread::JoinHandle;
use std::time::Instant;

use chrono::{DateTime, Utc};
use wasapi::{
    AudioCaptureClient, AudioClient, Direction, Handle, SampleType, StreamMode, WasapiError,
    WaveFormat,
};

use crate::transcribe::Queue;
use crate::vad::{SAMPLE_RATE, Segment, Segmenter, duration};

const SPEAKER_FRONT_CENTER: u32 = 0x4;
const BUFFER_HNS: i64 = 2_000_000;
const WAIT_MS: u32 = 200;
const BYTES_PER_SAMPLE: usize = size_of::<f32>();

const OPEN: u8 = 0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum Close {
    Flush = 1,
    Discard = 2,
}

pub(crate) struct Stream {
    control: Arc<AtomicU8>,
    started: Arc<AtomicBool>,
    thread: JoinHandle<()>,
}

impl Stream {
    pub(crate) fn open(
        pid: u32,
        process: Arc<str>,
        vad_model: PathBuf,
        queue: Queue,
    ) -> std::io::Result<Self> {
        let control = Arc::new(AtomicU8::new(OPEN));
        let started = Arc::new(AtomicBool::new(false));
        let thread = std::thread::Builder::new()
            .name("cw-audio stream".to_owned())
            .spawn({
                let (control, started) = (control.clone(), started.clone());
                move || {
                    if let Err(error) =
                        record(pid, &process, &vad_model, &queue, &control, &started)
                    {
                        tracing::warn!(process = %process, pid, %error, "an audio stream failed");
                    }
                }
            })?;
        Ok(Self {
            control,
            started,
            thread,
        })
    }

    pub(crate) fn close(&self, how: Close) {
        self.control.store(how as u8, Ordering::Relaxed);
    }

    pub(crate) fn started(&self) -> bool {
        self.started.load(Ordering::Relaxed)
    }

    pub(crate) fn ended(&self) -> bool {
        self.thread.is_finished()
    }
}

fn record(
    pid: u32,
    process: &Arc<str>,
    vad_model: &Path,
    queue: &Queue,
    control: &AtomicU8,
    started: &AtomicBool,
) -> Result<(), Box<dyn Error>> {
    wasapi::initialize_mta().ok()?;
    // `false` records everything except this tree, whatever wasapi's doc comment says.
    let mut client = AudioClient::new_application_loopback_client(pid, true)?;
    let format = WaveFormat::new(
        8 * BYTES_PER_SAMPLE,
        8 * BYTES_PER_SAMPLE,
        &SampleType::Float,
        SAMPLE_RATE,
        1,
        Some(SPEAKER_FRONT_CENTER),
    );
    let mode = StreamMode::EventsShared {
        autoconvert: true,
        buffer_duration_hns: BUFFER_HNS,
    };
    client.initialize_client(&format, &Direction::Capture, &mode)?;
    let event = client.set_get_eventhandle()?;
    let capture = client.get_audiocaptureclient()?;
    let mut utterances = Utterances {
        process,
        queue,
        control,
        segmenter: Segmenter::new(vad_model)
            .ok_or("the voice activity model could not be loaded")?,
        cut: Vec::new(),
        submitted: 0,
    };
    client.start_stream()?;
    started.store(true, Ordering::Relaxed);

    let opened = Instant::now();
    let mut lost = Lost::default();
    let pumped = pump(&event, &capture, control, &mut utterances, &mut lost);
    let unfinished = utterances.finish();
    let _ = client.stop_stream();
    tracing::info!(
        process = %process,
        pid,
        seconds = opened.elapsed().as_secs(),
        utterances = utterances.submitted,
        gaps = lost.gaps,
        lost_ms = lost.hns / 10_000,
        unfinished,
        "an audio stream closed"
    );
    pumped?;
    Ok(())
}

struct Utterances<'a> {
    process: &'a Arc<str>,
    queue: &'a Queue,
    control: &'a AtomicU8,
    segmenter: Segmenter,
    cut: Vec<Segment>,
    submitted: u64,
}

impl Utterances<'_> {
    fn feed(&mut self, samples: &[f32], at: DateTime<Utc>) {
        self.segmenter.push(samples, at, &mut self.cut);
        self.submit();
    }

    fn submit(&mut self) {
        if self.control.load(Ordering::Relaxed) == Close::Discard as u8 {
            self.cut.clear();
        }
        for segment in self.cut.drain(..) {
            self.submitted += 1;
            self.queue.submit(self.process, segment);
        }
    }

    fn finish(&mut self) -> &'static str {
        if !self.segmenter.utterance_open() {
            "none"
        } else if self.control.load(Ordering::Relaxed) == Close::Discard as u8 {
            "discarded"
        } else {
            self.segmenter.flush(&mut self.cut);
            self.submit();
            "transcribed"
        }
    }
}

#[derive(Default)]
struct Lost {
    gaps: u64,
    hns: u64,
}

fn pump(
    event: &Handle,
    capture: &AudioCaptureClient,
    control: &AtomicU8,
    utterances: &mut Utterances,
    lost: &mut Lost,
) -> Result<(), WasapiError> {
    let mut bytes = Vec::new();
    let mut samples: Vec<f32> = Vec::new();
    let mut expected_timestamp: Option<u64> = None;
    loop {
        if control.load(Ordering::Relaxed) != OPEN {
            return Ok(());
        }
        // The wait reports a failure as a timeout, so the packets are looked at either way.
        let _ = event.wait_for_event(WAIT_MS);
        while let Some(frames) = capture.get_next_packet_size()?.filter(|frames| *frames > 0) {
            bytes.resize(frames as usize * BYTES_PER_SAMPLE, 0);
            let (frames, info) = capture.read_from_device(&mut bytes)?;
            let frames = frames as usize;
            if frames == 0 {
                break;
            }
            let at = Utc::now() - duration(frames);

            // Lost audio shows only as a jump in the packet timestamps; no flag reports it.
            let packet_hns = (frames * 10_000_000 / SAMPLE_RATE) as u64;
            if let Some(expected) = expected_timestamp
                && info.timestamp > expected + packet_hns / 2
            {
                let lost_hns = info.timestamp - expected;
                lost.gaps += 1;
                lost.hns += lost_hns;
                tracing::debug!(
                    process = %utterances.process,
                    lost_ms = lost_hns / 10_000,
                    "audio was lost between two packets"
                );
            }
            expected_timestamp = Some(info.timestamp + packet_hns);

            samples.clear();
            if info.flags.silent {
                samples.resize(frames, 0.0);
            } else {
                let (packet, _) =
                    bytes[..frames * BYTES_PER_SAMPLE].as_chunks::<BYTES_PER_SAMPLE>();
                samples.extend(packet.iter().map(|sample| f32::from_le_bytes(*sample)));
            }
            utterances.feed(&samples, at);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transcribe::{self, Transcriber};

    struct Mute;

    impl Transcriber for Mute {
        fn transcribe(&mut self, _samples: &[f32]) -> Result<String, String> {
            Ok(String::new())
        }
    }

    fn recorded(during: u8, at_close: u8) -> (u64, &'static str) {
        let model_dir = PathBuf::from(
            std::env::var_os("CW_AUDIO_TEST_MODEL_DIR")
                .expect("CW_AUDIO_TEST_MODEL_DIR names a SenseVoice model folder"),
        );
        let wave =
            sherpa_onnx::Wave::read(&model_dir.join("test_wavs").join("ja.wav").to_string_lossy())
                .expect("the model folder ships test_wavs/ja.wav");
        let samples = wave.samples();
        let dir = tempfile::tempdir().expect("a temporary directory");
        let vad_model = crate::write_vad_model(dir.path()).expect("the model writes");
        let (events, _delivered) = std::sync::mpsc::channel();
        let (queue, _worker) =
            transcribe::start(Box::new(Mute), events).expect("the worker thread starts");
        let process: Arc<str> = "talk.exe".into();
        let control = AtomicU8::new(during);
        let mut utterances = Utterances {
            process: &process,
            queue: &queue,
            control: &control,
            segmenter: Segmenter::new(&vad_model).expect("the embedded model loads"),
            cut: Vec::new(),
            submitted: 0,
        };

        let fed = samples.iter().chain(&samples[..3 * SAMPLE_RATE]);
        for (index, packet) in fed.copied().collect::<Vec<f32>>().chunks(160).enumerate() {
            utterances.feed(packet, DateTime::UNIX_EPOCH + duration(index * 160));
        }
        control.store(at_close, Ordering::Relaxed);
        let unfinished = utterances.finish();
        (utterances.submitted, unfinished)
    }

    #[test]
    #[ignore = "needs a SenseVoice model folder in CW_AUDIO_TEST_MODEL_DIR"]
    fn a_flush_hands_on_what_a_discard_throws_away() {
        assert_eq!(recorded(OPEN, Close::Flush as u8), (2, "transcribed"));
        assert_eq!(recorded(OPEN, Close::Discard as u8), (1, "discarded"));
        assert_eq!(
            recorded(Close::Discard as u8, Close::Discard as u8),
            (0, "discarded"),
            "an utterance cut after the discard was asked for is not handed on either"
        );
    }
}
