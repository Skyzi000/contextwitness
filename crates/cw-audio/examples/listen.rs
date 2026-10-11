use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use chrono::{DateTime, Local, Utc};
use cw_audio::{Backend, Host, Settings, Utterance};

const USAGE: &str = "usage: listen <SenseVoice model folder> --seconds N [--text] [--verbose] \
                     [--language CODE] [--blacklist EXE]... [--pause-at N --resume-at N]";

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetCurrentProcess() -> isize;
    fn GetProcessTimes(
        process: isize,
        creation: *mut u64,
        exit: *mut u64,
        kernel: *mut u64,
        user: *mut u64,
    ) -> i32;
}

fn cpu_seconds() -> f64 {
    let (mut creation, mut exit, mut kernel, mut user) = (0u64, 0u64, 0u64, 0u64);
    unsafe {
        GetProcessTimes(
            GetCurrentProcess(),
            &mut creation,
            &mut exit,
            &mut kernel,
            &mut user,
        )
    };
    (kernel + user) as f64 / 1e7
}

struct Listener {
    scratch: PathBuf,
    launched: Instant,
    seconds: Duration,
    pause: Option<(Duration, Duration)>,
    show_text: bool,
    patrolling: Option<(Instant, f64)>,
    measured: Option<(f64, f64)>,
    utterances: BTreeMap<String, (u32, u32)>,
    settled: Option<DateTime<Utc>>,
}

impl Listener {
    fn finish(&self, (wall, cpu): (f64, f64)) -> ! {
        println!(
            "patrolled {wall:.1} s: CPU {cpu:.3} s = {:.2}% of one core (whole process: CPU {:.3} s over {:.1} s)",
            cpu / wall * 100.0,
            cpu_seconds(),
            self.launched.elapsed().as_secs_f64()
        );
        if self.utterances.is_empty() {
            println!("no utterance was delivered");
        }
        for (process, (delivered, failed)) in &self.utterances {
            println!("[{process}] {delivered} utterances, {failed} of them failed");
        }
        if let Some(settled) = self.settled {
            println!(
                "last settled mark: {}",
                settled.with_timezone(&Local).format("%H:%M:%S%.3f")
            );
        }
        let _ = std::fs::remove_dir_all(&self.scratch);
        std::process::exit(0)
    }
}

impl Host for Listener {
    fn paused(&mut self) -> bool {
        if let Some(measured) = self.measured {
            self.finish(measured);
        }
        let (since, cpu) = *self
            .patrolling
            .get_or_insert_with(|| (Instant::now(), cpu_seconds()));
        let elapsed = since.elapsed();
        if elapsed >= self.seconds {
            self.measured = Some((elapsed.as_secs_f64(), cpu_seconds() - cpu));
            return true;
        }
        self.pause
            .is_some_and(|(from, to)| (from..to).contains(&elapsed))
    }

    fn utterance(&mut self, utterance: Utterance) {
        let at = utterance
            .started_at
            .with_timezone(&Local)
            .format("%H:%M:%S");
        let count = self
            .utterances
            .entry(utterance.process.clone())
            .or_default();
        count.0 += 1;
        match &utterance.outcome {
            Ok(text) => {
                let shown = if self.show_text { text } else { "" };
                println!(
                    "{at} [{}] {} characters in {} ms {shown}",
                    utterance.process,
                    text.chars().count(),
                    utterance.duration_ms
                );
            }
            Err(reason) => {
                count.1 += 1;
                println!("{at} [{}] failed: {reason}", utterance.process);
            }
        }
    }

    fn settled(&mut self, through: DateTime<Utc>) {
        self.settled = Some(through);
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let has = |name: &str| args.iter().any(|arg| arg == name);
    let values = |name: &str| {
        args.windows(2)
            .filter(|pair| pair[0] == name)
            .map(|pair| pair[1].clone())
            .collect::<Vec<_>>()
    };
    let seconds = |name: &str| {
        values(name)
            .first()
            .and_then(|value| value.parse().ok())
            .map(Duration::from_secs_f64)
    };
    let model_dir = args
        .first()
        .filter(|arg| !arg.starts_with("--"))
        .ok_or(USAGE)?;
    let run_for = seconds("--seconds").ok_or(USAGE)?;

    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_max_level(if has("--verbose") {
            tracing::Level::DEBUG
        } else {
            tracing::Level::INFO
        })
        .init();

    let scratch = std::env::temp_dir().join(format!("cw-audio-listen-{}", std::process::id()));
    let settings = Settings {
        blacklist: values("--blacklist"),
        vad_model: cw_audio::write_vad_model(&scratch)?,
        backend: Backend::Local {
            model_dir: PathBuf::from(model_dir),
        },
        language: values("--language").pop().unwrap_or_default(),
    };
    let mut listener = Listener {
        scratch: scratch.clone(),
        launched: Instant::now(),
        seconds: run_for,
        pause: seconds("--pause-at").zip(seconds("--resume-at")),
        show_text: has("--text"),
        patrolling: None,
        measured: None,
        utterances: BTreeMap::new(),
        settled: None,
    };
    let Err(error) = cw_audio::run(settings, &mut listener);
    let _ = std::fs::remove_dir_all(&scratch);
    Err(error.to_string().into())
}
