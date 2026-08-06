#![deny(unsafe_op_in_unsafe_fn)]
//! The ContextWitness daemon entry point.

use std::collections::HashMap;

use cw_core::change::{Thumbnail, frame_changed};
use cw_core::config::{Config, DataPaths, default_config_path};
use cw_core::model::{Observation, ScreenPayload};

fn main() {
    cw_capture::make_dpi_aware();
    cw_ocr::init_runtime();

    let config_path = default_config_path().expect("resolving the config path failed");
    let created =
        Config::write_default_if_missing(&config_path).expect("writing the default config failed");
    if created {
        println!("wrote default config to {}", config_path.display());
    }
    let config = Config::load_from_path(&config_path).expect("loading the config failed");
    let data_dir = config
        .storage
        .resolve_data_dir()
        .expect("resolving the data directory failed");
    let paths = DataPaths::new(data_dir);
    let mut conn = cw_store::db::open(&paths.database()).expect("opening the database failed");
    println!(
        "capturing every {}s into {}",
        config.capture.interval_secs,
        paths.database().display()
    );

    let mut previous: HashMap<String, Thumbnail> = HashMap::new();
    loop {
        if let Err(error) = tick(&mut conn, &paths, &config, &mut previous) {
            eprintln!("tick failed: {error}");
        }
        std::thread::sleep(std::time::Duration::from_secs(config.capture.interval_secs));
    }
}

fn tick(
    conn: &mut rusqlite::Connection,
    paths: &DataPaths,
    config: &Config,
    previous: &mut HashMap<String, Thumbnail>,
) -> Result<(), Box<dyn std::error::Error>> {
    let foreground = cw_capture::foreground();
    if let Some(process) = &foreground.process
        && config
            .privacy
            .process_blacklist
            .iter()
            .any(|blocked| blocked.eq_ignore_ascii_case(process))
    {
        return Ok(());
    }

    for frame in cw_capture::capture_all() {
        let rgba = bgra_to_rgba(&frame.bgra);
        let thumbnail = Thumbnail::from_rgba(&rgba, frame.width, frame.height, frame.dpi_scale)?;
        if !frame_changed(previous.get(&frame.monitor_id), &thumbnail, &config.capture) {
            continue;
        }

        let now = chrono::Utc::now();
        let ocr = cw_ocr::recognize(&frame.bgra, frame.width, frame.height);
        let text_chars = ocr.text.as_deref().map_or(0, |text| text.chars().count());
        let payload = ScreenPayload {
            monitor_id: frame.monitor_id.clone(),
            width: frame.width,
            height: frame.height,
            // The images table is the truth about which file this observation's frame is in.
            image_path: None,
            ocr_status: ocr.status.clone(),
            ocr_error: ocr.error,
            ocr_text: ocr.text,
            ocr_langs: ocr.langs,
            foreground_process: foreground.process.clone(),
            foreground_window_title: foreground.title.clone(),
        };
        let observation = Observation::new_screen(payload, now);
        cw_store::observations::insert(conn, &observation)?;
        let rgb = bgra_to_rgb(&frame.bgra);
        let stored = cw_store::images::save(
            conn,
            &paths.images(),
            observation.id,
            &rgb,
            frame.width,
            frame.height,
            f32::from(config.capture.webp_quality),
            now,
        )?;
        previous.insert(frame.monitor_id.clone(), thumbnail);
        println!(
            "{} {}x{} ocr {:?} ({} chars) -> {}",
            frame.monitor_id, frame.width, frame.height, ocr.status, text_chars, stored
        );
    }
    Ok(())
}

fn bgra_to_rgba(bgra: &[u8]) -> Vec<u8> {
    let mut rgba = bgra.to_vec();
    for pixel in rgba.chunks_exact_mut(4) {
        pixel.swap(0, 2);
    }
    rgba
}

fn bgra_to_rgb(bgra: &[u8]) -> Vec<u8> {
    bgra.chunks_exact(4)
        .flat_map(|pixel| [pixel[2], pixel[1], pixel[0]])
        .collect()
}
