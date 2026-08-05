//! Grouping observations into fixed-length episodes for delivery.

const SCREEN_SOURCE: &str = "screen";

/// A finished episode: the exact text and metadata that will be stored and delivered.
///
/// There is deliberately no `id` field — the ULID is assigned by the store when the row is
/// inserted, so that building an episode twice yields two equal values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Episode {
    /// Source that produced this episode; goes straight into the `episodes.source` column, and
    /// `build_episode` derives the prefix of `document_id` from this same value rather than from a
    /// second spelling of it.
    pub source: &'static str,
    /// Inclusive start of the window.
    pub start_at: chrono::DateTime<chrono::Utc>,
    /// Exclusive end of the window.
    pub end_at: chrono::DateTime<chrono::Utc>,
    /// Stable Hindsight document id derived from the source, window start, and window length.
    pub document_id: String,
    /// Rendered delivery text.
    pub content: String,
    /// Retain metadata snapshot.
    pub metadata: EpisodeMetadata,
}

/// Retain metadata stored alongside the content, already in the form Hindsight accepts.
///
/// Every value is a string because `MemoryItem.metadata` is declared
/// `additionalProperties: {"type": "string"}`; a number or an array there is rejected outright.
/// The snapshot is delivered exactly as stored, so the conversion has to happen here rather than
/// at delivery time.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct EpisodeMetadata {
    /// Window start, RFC 3339 UTC.
    pub episode_start: String,
    /// Window end, RFC 3339 UTC.
    pub episode_end: String,
    /// Monitors that contributed an entry, in render order, as a JSON array string.
    pub monitors: String,
    /// Number of entries in `content` after folding, in decimal.
    pub entry_count: String,
    /// Relative image paths of the rendered entries, in render order, as a JSON array string.
    /// Entries folded as duplicates and entries with no stored image are absent.
    pub image_paths: String,
}

/// Truncate `at` down to a multiple of `window_minutes` counted from the Unix epoch.
///
/// The epoch is aligned to midnight, so for any window length that divides 60 this is also
/// aligned to the clock hour. Panics when `window_minutes` is zero (`Config::validate` rejects
/// that) and when truncation lands before `chrono::DateTime::<Utc>::MIN_UTC`, which only an `at`
/// less than one window length above that bound can reach.
pub fn window_start(
    at: chrono::DateTime<chrono::Utc>,
    window_minutes: u32,
) -> chrono::DateTime<chrono::Utc> {
    let window_seconds = i64::from(window_minutes) * 60;
    let seconds = at.timestamp().div_euclid(window_seconds) * window_seconds;

    chrono::DateTime::from_timestamp(seconds, 0)
        .expect("a truncated timestamp must still be representable")
}

/// Render the screen observations that fall in `[window_start, window_start + window_minutes)`
/// into one episode. Returns `None` when none do — an empty window must not become an empty
/// document.
///
/// `window_start` is taken down to the second it falls in, and that second's window is the one
/// rendered.
///
/// `render_offset` is the UTC offset used for the human-readable times in the body. Supplying the
/// machine's local offset is what makes a memory read back in the time the user experienced, for
/// any window that does not straddle an offset transition; ids and metadata stay UTC regardless. A
/// window that does straddle one renders every line at the single supplied offset, so lines on the
/// side that offset does not describe are shifted by whatever the transition was; the header prints
/// the offset, so the result stays readable.
///
/// The fold relation is field-wise equality and therefore an equivalence relation, so comparing
/// against the last kept entry and comparing against the immediately preceding one agree here.
///
/// Panics when the window's end would land past `chrono::DateTime::<Utc>::MAX_UTC`, and when a
/// bound rendered at `render_offset` leaves chrono's representable range — a start by `MIN_UTC`
/// with a westward offset, or an end by `MAX_UTC` with an eastward one (measured: both panic in
/// chrono's offset addition).
pub fn build_episode(
    window_start: chrono::DateTime<chrono::Utc>,
    window_minutes: u32,
    render_offset: chrono::FixedOffset,
    observations: &[crate::model::Observation],
) -> Option<Episode> {
    // Everything below spells this to the second, `document_id` among them, so a start carrying a
    // fraction would answer for a window it does not bound and share an id with the rest of its
    // second. Taken through `timestamp` rather than by truncating the subsecond field, because that
    // field also carries a leap second as a value at or above one second and truncating leaves it:
    // measured, `12:34:58` with a nanosecond field of 1_000_000_000 keeps it, spells `12:34:59Z`
    // exactly as the ordinary instant of that name does, and ends one second earlier than it does.
    let window_start = chrono::DateTime::from_timestamp(window_start.timestamp(), 0)
        .expect("a whole second taken from an instant must still be representable");
    let end_at = window_start + chrono::Duration::minutes(i64::from(window_minutes));
    let mut entries: Vec<_> = observations
        .iter()
        .filter_map(|observation| match &observation.payload {
            crate::model::SourcePayload::Screen(screen)
                if observation.observed_at >= window_start && observation.observed_at < end_at =>
            {
                Some((observation, screen))
            }
            _ => None,
        })
        .collect();

    entries.sort_unstable_by(
        |(left_observation, left_screen), (right_observation, right_screen)| {
            left_screen
                .monitor_id
                .cmp(&right_screen.monitor_id)
                .then_with(|| {
                    left_observation
                        .observed_at
                        .cmp(&right_observation.observed_at)
                })
                .then_with(|| left_observation.id.cmp(&right_observation.id))
        },
    );
    entries.dedup_by(|current, previous| {
        // Fold on the fields below and nothing else. The timestamp is excluded because it is
        // what a folded entry usually differs in, and collapsing repeated times is the point — two
        // observations at one instant fold as readily. Text and error are compared whether this
        // entry's status renders them or not, so two that a reader could not tell apart are kept
        // apart when a hidden `ocr_text` differs; erring towards keeping is the safer way for this
        // to be wrong. The other direction is real too: the monitor's size, the image path and the
        // OCR languages are not compared, so entries differing only in those do fold. The size is
        // left out because it is written only where the monitor changes, and an entry matching the
        // last kept one's monitor would not have written it.
        let current = current.1;
        let previous = previous.1;

        current.monitor_id == previous.monitor_id
            && current.foreground_process == previous.foreground_process
            && current.foreground_window_title == previous.foreground_window_title
            && current.ocr_status == previous.ocr_status
            && current.ocr_error == previous.ocr_error
            && current.ocr_text == previous.ocr_text
    });

    if entries.is_empty() {
        return None;
    }

    let rendered_start = window_start
        .with_timezone(&render_offset)
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let rendered_end = end_at
        .with_timezone(&render_offset)
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let mut lines = vec![format!(
        "[{rendered_start} - {rendered_end}] Screen episode"
    )];
    let mut monitors = Vec::new();
    let mut image_paths = Vec::new();
    let mut current_monitor = None;

    for (observation, screen) in &entries {
        if current_monitor != Some(screen.monitor_id.as_str()) {
            lines.push(format!(
                "Monitor {} ({}x{}):",
                screen.monitor_id, screen.width, screen.height
            ));
            monitors.push(screen.monitor_id.clone());
            current_monitor = Some(screen.monitor_id.as_str());
        }

        let mut entry_line = format!(
            "  {}",
            observation
                .observed_at
                .with_timezone(&render_offset)
                .format("%H:%M:%S")
        );
        if let Some(process) = &screen.foreground_process {
            entry_line.push_str(" [");
            entry_line.push_str(process);
            entry_line.push(']');
        }
        if let Some(title) = &screen.foreground_window_title {
            entry_line.push(' ');
            entry_line.push_str(title);
        }
        lines.push(entry_line);

        match &screen.ocr_status {
            crate::model::OcrStatus::Succeeded => {
                if let Some(text) = &screen.ocr_text {
                    let mut ocr_lines: Vec<_> = text
                        .lines()
                        .map(|line| line.trim_end_matches('\r'))
                        .collect();
                    while ocr_lines.last().is_some_and(|line| line.is_empty()) {
                        ocr_lines.pop();
                    }
                    lines.extend(ocr_lines.into_iter().map(|line| {
                        if line.is_empty() {
                            String::new()
                        } else {
                            format!("    {line}")
                        }
                    }));
                }
            }
            crate::model::OcrStatus::NoText => {}
            crate::model::OcrStatus::Failed => match &screen.ocr_error {
                Some(error) => lines.push(format!("    [OCR failed: {error}]")),
                None => lines.push("    [OCR failed]".to_owned()),
            },
        }

        if let Some(image_path) = &screen.image_path {
            image_paths.push(image_path.clone());
        }
    }

    let episode_start = window_start.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let episode_end = end_at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);

    Some(Episode {
        source: SCREEN_SOURCE,
        start_at: window_start,
        end_at,
        document_id: format!("{SCREEN_SOURCE}-{episode_start}-{window_minutes}m"),
        content: lines.join("\n"),
        metadata: EpisodeMetadata {
            episode_start,
            episode_end,
            monitors: serde_json::to_string(&monitors).expect("a list of strings must serialise"),
            entry_count: entries.len().to_string(),
            image_paths: serde_json::to_string(&image_paths)
                .expect("a list of strings must serialise"),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{
        CURRENT_SCHEMA_VERSION, Observation, OcrStatus, ScreenPayload, SourcePayload,
    };
    use chrono::{DateTime, FixedOffset, Utc};

    fn timestamp(value: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(value)
            .expect("test timestamp should be valid RFC 3339")
            .with_timezone(&Utc)
    }

    fn screen_payload(
        monitor_id: &str,
        ocr_status: OcrStatus,
        ocr_text: Option<&str>,
    ) -> ScreenPayload {
        ScreenPayload {
            monitor_id: monitor_id.to_owned(),
            width: 1920,
            height: 1080,
            image_path: None,
            ocr_status,
            ocr_error: None,
            ocr_text: ocr_text.map(str::to_owned),
            ocr_langs: Vec::new(),
            foreground_process: None,
            foreground_window_title: None,
        }
    }

    fn observation(id: u128, observed_at: &str, payload: ScreenPayload) -> Observation {
        Observation {
            id: ulid::Ulid::from(id),
            observed_at: timestamp(observed_at),
            duration_ms: None,
            schema_version: CURRENT_SCHEMA_VERSION,
            payload: SourcePayload::Screen(payload),
        }
    }

    fn golden_observations() -> Vec<Observation> {
        vec![
            observation(
                1,
                "2026-07-24T16:00:02Z",
                ScreenPayload {
                    width: 2560,
                    height: 1440,
                    image_path: Some("images/a.webp".to_owned()),
                    foreground_process: Some("firefox.exe".to_owned()),
                    foreground_window_title: Some("Example Page".to_owned()),
                    ..screen_payload("DISPLAY1", OcrStatus::Succeeded, Some("line one\nline two"))
                },
            ),
            observation(
                2,
                "2026-07-24T16:00:04Z",
                ScreenPayload {
                    width: 2560,
                    height: 1440,
                    image_path: Some("images/b.webp".to_owned()),
                    foreground_process: Some("firefox.exe".to_owned()),
                    foreground_window_title: Some("Example Page".to_owned()),
                    ..screen_payload("DISPLAY1", OcrStatus::Succeeded, Some("line one\nline two"))
                },
            ),
            observation(
                3,
                "2026-07-24T16:01:14Z",
                ScreenPayload {
                    width: 2560,
                    height: 1440,
                    image_path: Some("images/c.webp".to_owned()),
                    foreground_process: Some("Code.exe".to_owned()),
                    foreground_window_title: Some("contextwitness - Visual Studio Code".to_owned()),
                    ..screen_payload("DISPLAY1", OcrStatus::Succeeded, Some("fn main() {}"))
                },
            ),
            observation(
                4,
                "2026-07-24T16:02:00Z",
                ScreenPayload {
                    width: 2560,
                    height: 1440,
                    ocr_error: Some("engine unavailable".to_owned()),
                    foreground_process: Some("Code.exe".to_owned()),
                    foreground_window_title: Some("contextwitness - Visual Studio Code".to_owned()),
                    ..screen_payload("DISPLAY1", OcrStatus::Failed, None)
                },
            ),
            observation(
                5,
                "2026-07-24T16:00:30Z",
                ScreenPayload {
                    image_path: Some("images/e.webp".to_owned()),
                    ..screen_payload("DISPLAY2", OcrStatus::NoText, None)
                },
            ),
        ]
    }

    #[test]
    fn observations_group_into_fixed_five_minute_windows() {
        let expected = timestamp("2026-07-25T01:00:00Z");

        assert_eq!(window_start(timestamp("2026-07-25T01:03:59Z"), 5), expected);
        assert_eq!(window_start(timestamp("2026-07-25T01:04:00Z"), 5), expected);
        assert_eq!(
            window_start(timestamp("2026-07-25T01:06:00Z"), 5),
            timestamp("2026-07-25T01:05:00Z")
        );
        assert_eq!(
            window_start(timestamp("2026-07-25T01:05:00Z"), 5),
            timestamp("2026-07-25T01:05:00Z")
        );
        // A clock set wrong can observe before the epoch, and only there do flooring and
        // truncating toward zero part ways: truncated, 23:57 would get the window start
        // 00:00:00, which is after the instant.
        assert_eq!(
            window_start(timestamp("1969-12-31T23:57:00Z"), 5),
            timestamp("1969-12-31T23:55:00Z")
        );
    }

    #[test]
    fn an_episode_is_the_same_whatever_order_its_observations_arrive_in() {
        let ordered = golden_observations();
        let mut reversed_and_interleaved = ordered.clone();
        reversed_and_interleaved.reverse();
        let display2 = reversed_and_interleaved.remove(0);
        reversed_and_interleaved.insert(2, display2);
        let start = timestamp("2026-07-24T16:00:00Z");
        let offset = FixedOffset::east_opt(9 * 3600).expect("test offset should be valid");

        let first = build_episode(start, 5, offset, &ordered)
            .expect("the golden observations should build an episode");
        let second = build_episode(start, 5, offset, &reversed_and_interleaved)
            .expect("the reordered observations should build an episode");

        assert_eq!(first, second);
    }

    #[test]
    fn two_observations_at_one_instant_render_in_id_order() {
        // Only the id rung of the sort decides between these two — one monitor, one instant, two
        // ids — so this pair is what keeps the rendered order independent of arrival order when
        // the other keys tie. The titles differ so the fold keeps both entries visible.
        let lower = observation(
            1,
            "2026-07-24T16:00:02Z",
            ScreenPayload {
                foreground_window_title: Some("Alpha".to_owned()),
                ..screen_payload("DISPLAY1", OcrStatus::NoText, None)
            },
        );
        let higher = observation(
            2,
            "2026-07-24T16:00:02Z",
            ScreenPayload {
                foreground_window_title: Some("Beta".to_owned()),
                ..screen_payload("DISPLAY1", OcrStatus::NoText, None)
            },
        );
        let start = timestamp("2026-07-24T16:00:00Z");
        let offset = FixedOffset::east_opt(9 * 3600).expect("test offset should be valid");
        let forward_input = [lower.clone(), higher.clone()];
        let swapped_input = [higher, lower];

        let forward = build_episode(start, 5, offset, &forward_input)
            .expect("two observations should build an episode");
        let swapped = build_episode(start, 5, offset, &swapped_input)
            .expect("two observations should build an episode");

        assert_eq!(forward, swapped);
        let alpha = forward
            .content
            .find("Alpha")
            .expect("the lower id's entry should render");
        let beta = forward
            .content
            .find("Beta")
            .expect("the higher id's entry should render");
        assert!(alpha < beta, "the lower id should render first");
    }

    #[test]
    fn document_id_identifies_the_window_including_its_length() {
        let start = timestamp("2026-07-24T16:00:00Z");
        let offset = FixedOffset::east_opt(9 * 3600).expect("test offset should be valid");
        let observations = golden_observations();
        let five_minute_episode = build_episode(start, 5, offset, &observations)
            .expect("the golden observations should build an episode");

        assert_eq!(
            five_minute_episode.document_id,
            "screen-2026-07-24T16:00:00Z-5m"
        );

        let ten_minute_episode = build_episode(start, 10, offset, &observations)
            .expect("the golden observations should build an episode");

        assert_ne!(
            ten_minute_episode.document_id,
            five_minute_episode.document_id
        );
    }

    #[test]
    fn a_window_start_carrying_a_fraction_describes_the_second_it_falls_in() {
        let start = timestamp("2026-07-24T16:00:00Z");
        let offset = FixedOffset::east_opt(9 * 3600).expect("test offset should be valid");
        let observations = vec![observation(
            1,
            "2026-07-24T16:00:00Z",
            screen_payload("DISPLAY1", OcrStatus::Succeeded, Some("line one")),
        )];

        let episode = build_episode(
            start + chrono::Duration::milliseconds(900),
            5,
            offset,
            &observations,
        )
        .expect("an observation at the whole second belongs to the window that second starts");

        assert_eq!(episode.start_at, start);
        assert_eq!(episode.end_at, timestamp("2026-07-24T16:05:00Z"));
        assert_eq!(episode.document_id, "screen-2026-07-24T16:00:00Z-5m");
    }

    #[test]
    fn a_start_inside_a_leap_second_is_not_the_window_it_spells_like() {
        use chrono::Timelike;

        let offset = FixedOffset::east_opt(9 * 3600).expect("test offset should be valid");
        let observations = golden_observations();
        let ordinary = timestamp("2026-07-24T15:59:59Z");
        let leap = timestamp("2026-07-24T15:59:58Z")
            .with_nanosecond(1_000_000_000)
            .expect("a second should accept a leap nanosecond");

        // The two are different instants that RFC 3339 spells alike once the fraction is dropped,
        // which is the spelling `document_id` is built from. Built on `:58` rather than `:59`,
        // because a leap nanosecond on `:59` spells `:60` and collides with no ordinary instant.
        assert_ne!(leap, ordinary);
        assert_eq!(
            leap.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            ordinary.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
        );

        let from_ordinary = build_episode(ordinary, 5, offset, &observations)
            .expect("the golden observations should build an episode");
        let from_leap = build_episode(leap, 5, offset, &observations)
            .expect("the leap start should build an episode of its own");

        assert_ne!(from_leap.document_id, from_ordinary.document_id);
    }

    #[test]
    fn episode_carries_the_source_the_store_column_needs() {
        let episode = build_episode(
            timestamp("2026-07-24T16:00:00Z"),
            5,
            FixedOffset::east_opt(9 * 3600).expect("test offset should be valid"),
            &golden_observations(),
        )
        .expect("the golden observations should build an episode");

        assert_eq!(episode.source, "screen");
        assert!(episode.document_id.starts_with(episode.source));
    }

    #[test]
    fn episode_content_renders_golden_text() {
        let episode = build_episode(
            timestamp("2026-07-24T16:00:00Z"),
            5,
            FixedOffset::east_opt(9 * 3600).expect("test offset should be valid"),
            &golden_observations(),
        )
        .expect("the golden observations should build an episode");
        let golden = r#"[2026-07-25T01:00:00+09:00 - 2026-07-25T01:05:00+09:00] Screen episode
Monitor DISPLAY1 (2560x1440):
  01:00:02 [firefox.exe] Example Page
    line one
    line two
  01:01:14 [Code.exe] contextwitness - Visual Studio Code
    fn main() {}
  01:02:00 [Code.exe] contextwitness - Visual Studio Code
    [OCR failed: engine unavailable]
Monitor DISPLAY2 (1920x1080):
  01:00:30"#;

        assert_eq!(episode.content, golden);
        assert_eq!(
            serde_json::to_value(&episode.metadata)
                .expect("episode metadata should serialize to JSON"),
            serde_json::json!({
                "episode_start": "2026-07-24T16:00:00Z",
                "episode_end": "2026-07-24T16:05:00Z",
                "monitors": "[\"DISPLAY1\",\"DISPLAY2\"]",
                "entry_count": "4",
                "image_paths": "[\"images/a.webp\",\"images/c.webp\",\"images/e.webp\"]"
            })
        );
    }

    #[test]
    fn every_metadata_value_is_a_string_as_hindsight_requires() {
        let episode = build_episode(
            timestamp("2026-07-24T16:00:00Z"),
            5,
            FixedOffset::east_opt(9 * 3600).expect("test offset should be valid"),
            &golden_observations(),
        )
        .expect("the golden observations should build an episode");
        let metadata = serde_json::to_value(&episode.metadata)
            .expect("episode metadata should serialize to JSON");
        let metadata = metadata
            .as_object()
            .expect("episode metadata should serialize as an object");

        // `MemoryItem.metadata` is `additionalProperties: {"type": "string"}` in the 0.8.4
        // schema. A field added later as a number or an array would make every episode fail
        // delivery, and this test catches that instead of a 422 in production.
        for (key, value) in metadata {
            assert!(
                value.is_string(),
                "metadata field `{key}` must be a string, got {value}"
            );
        }
    }

    #[test]
    fn consecutive_entries_with_different_text_are_never_collapsed() {
        // Every field the fold compares is equal in these two except the text, so this is the case
        // where text alone decides. Text is what this product delivers, and one entry standing for
        // another would lose it.
        let observations = vec![
            observation(
                1,
                "2026-07-24T16:00:01Z",
                ScreenPayload {
                    image_path: Some("images/earlier.webp".to_owned()),
                    ..screen_payload("DISPLAY1", OcrStatus::Succeeded, Some("earlier"))
                },
            ),
            observation(
                2,
                "2026-07-24T16:00:02Z",
                ScreenPayload {
                    image_path: Some("images/later.webp".to_owned()),
                    ..screen_payload("DISPLAY1", OcrStatus::Succeeded, Some("later"))
                },
            ),
        ];

        let episode = build_episode(
            timestamp("2026-07-24T16:00:00Z"),
            5,
            FixedOffset::east_opt(0).expect("UTC offset should be valid"),
            &observations,
        )
        .expect("the observations should build an episode");

        assert_eq!(episode.metadata.entry_count, "2");
        assert!(episode.content.contains("earlier"));
        assert!(episode.content.contains("later"));
    }

    #[test]
    fn text_free_entries_from_the_same_application_fold() {
        // A video playing: every frame renders the same line but for its time, and one line is
        // what the reader needs from it.
        let observations = vec![
            observation(
                1,
                "2026-07-24T16:00:01Z",
                ScreenPayload {
                    foreground_process: Some("vlc.exe".to_owned()),
                    foreground_window_title: Some("Movie".to_owned()),
                    ..screen_payload("DISPLAY1", OcrStatus::NoText, None)
                },
            ),
            observation(
                2,
                "2026-07-24T16:00:02Z",
                ScreenPayload {
                    foreground_process: Some("vlc.exe".to_owned()),
                    foreground_window_title: Some("Movie".to_owned()),
                    ..screen_payload("DISPLAY1", OcrStatus::NoText, None)
                },
            ),
        ];
        let episode = build_episode(
            timestamp("2026-07-24T16:00:00Z"),
            5,
            FixedOffset::east_opt(0).expect("UTC offset should be valid"),
            &observations,
        )
        .expect("the observations should build an episode");

        assert_eq!(episode.metadata.entry_count, "1");
    }

    #[test]
    fn an_identical_scene_on_another_monitor_keeps_the_entry() {
        // The two entries differ in nothing the fold compares except the monitor, and folding
        // across that boundary would tell the reader one screen was on when two were: the second
        // monitor's heading, its entry line and its metadata slot all travel with the entry.
        let observations = vec![
            observation(
                1,
                "2026-07-24T16:00:01Z",
                ScreenPayload {
                    foreground_process: Some("vlc.exe".to_owned()),
                    foreground_window_title: Some("Movie".to_owned()),
                    ..screen_payload("DISPLAY1", OcrStatus::NoText, None)
                },
            ),
            observation(
                2,
                "2026-07-24T16:00:02Z",
                ScreenPayload {
                    foreground_process: Some("vlc.exe".to_owned()),
                    foreground_window_title: Some("Movie".to_owned()),
                    ..screen_payload("DISPLAY2", OcrStatus::NoText, None)
                },
            ),
        ];
        let episode = build_episode(
            timestamp("2026-07-24T16:00:00Z"),
            5,
            FixedOffset::east_opt(0).expect("UTC offset should be valid"),
            &observations,
        )
        .expect("the observations should build an episode");

        assert_eq!(episode.metadata.entry_count, "2");
        assert_eq!(episode.metadata.monitors, r#"["DISPLAY1","DISPLAY2"]"#);
    }

    #[test]
    fn a_different_application_keeps_the_entry() {
        // The switch between two text-free applications is the only thing these entries record, so
        // folding them would leave the document with nothing to show for it.
        let observations = vec![
            observation(
                1,
                "2026-07-24T16:00:01Z",
                ScreenPayload {
                    foreground_process: Some("vlc.exe".to_owned()),
                    ..screen_payload("DISPLAY1", OcrStatus::NoText, None)
                },
            ),
            observation(
                2,
                "2026-07-24T16:00:02Z",
                ScreenPayload {
                    foreground_process: Some("game.exe".to_owned()),
                    ..screen_payload("DISPLAY1", OcrStatus::NoText, None)
                },
            ),
        ];
        let episode = build_episode(
            timestamp("2026-07-24T16:00:00Z"),
            5,
            FixedOffset::east_opt(0).expect("UTC offset should be valid"),
            &observations,
        )
        .expect("the observations should build an episode");

        assert_eq!(episode.metadata.entry_count, "2");
        assert!(episode.content.contains("vlc.exe"));
        assert!(episode.content.contains("game.exe"));
    }

    #[test]
    fn a_changed_window_title_keeps_the_entry() {
        let observations = vec![
            observation(
                1,
                "2026-07-24T16:00:01Z",
                ScreenPayload {
                    foreground_process: Some("editor.exe".to_owned()),
                    foreground_window_title: Some("first.txt".to_owned()),
                    ..screen_payload("DISPLAY1", OcrStatus::Succeeded, Some("same text"))
                },
            ),
            observation(
                2,
                "2026-07-24T16:00:02Z",
                ScreenPayload {
                    foreground_process: Some("editor.exe".to_owned()),
                    foreground_window_title: Some("second.txt".to_owned()),
                    ..screen_payload("DISPLAY1", OcrStatus::Succeeded, Some("same text"))
                },
            ),
        ];
        let episode = build_episode(
            timestamp("2026-07-24T16:00:00Z"),
            5,
            FixedOffset::east_opt(0).expect("UTC offset should be valid"),
            &observations,
        )
        .expect("the observations should build an episode");

        assert_eq!(episode.metadata.entry_count, "2");
        assert!(episode.content.contains("first.txt"));
        assert!(episode.content.contains("second.txt"));
    }

    #[test]
    fn a_failed_entry_never_folds_into_a_no_text_one() {
        let observations = vec![
            // The same error on both sides, so the status is the only thing that differs and the
            // only thing that can be keeping them apart. Only the `Failed` one renders it.
            observation(
                1,
                "2026-07-24T16:00:01Z",
                ScreenPayload {
                    ocr_error: Some("engine unavailable".to_owned()),
                    ..screen_payload("DISPLAY1", OcrStatus::NoText, None)
                },
            ),
            observation(
                2,
                "2026-07-24T16:00:02Z",
                ScreenPayload {
                    ocr_error: Some("engine unavailable".to_owned()),
                    ..screen_payload("DISPLAY1", OcrStatus::Failed, None)
                },
            ),
        ];

        let episode = build_episode(
            timestamp("2026-07-24T16:00:00Z"),
            5,
            FixedOffset::east_opt(0).expect("UTC offset should be valid"),
            &observations,
        )
        .expect("the observations should build an episode");

        assert_eq!(episode.metadata.entry_count, "2");
        assert!(episode.content.contains("[OCR failed: engine unavailable]"));
    }

    #[test]
    fn identical_failures_fold_but_different_errors_do_not() {
        let failures = |second_error: &str| {
            vec![
                observation(
                    1,
                    "2026-07-24T16:00:01Z",
                    ScreenPayload {
                        ocr_error: Some("engine unavailable".to_owned()),
                        ..screen_payload("DISPLAY1", OcrStatus::Failed, None)
                    },
                ),
                observation(
                    2,
                    "2026-07-24T16:00:02Z",
                    ScreenPayload {
                        ocr_error: Some(second_error.to_owned()),
                        ..screen_payload("DISPLAY1", OcrStatus::Failed, None)
                    },
                ),
            ]
        };

        let identical = failures("engine unavailable");
        let identical_episode = build_episode(
            timestamp("2026-07-24T16:00:00Z"),
            5,
            FixedOffset::east_opt(0).expect("UTC offset should be valid"),
            &identical,
        )
        .expect("the identical failures should build an episode");

        assert_eq!(identical_episode.metadata.entry_count, "1");

        let different = failures("timed out");
        let different_episode = build_episode(
            timestamp("2026-07-24T16:00:00Z"),
            5,
            FixedOffset::east_opt(0).expect("UTC offset should be valid"),
            &different,
        )
        .expect("the different failures should build an episode");

        assert_eq!(different_episode.metadata.entry_count, "2");
        assert!(
            different_episode
                .content
                .contains("[OCR failed: engine unavailable]")
        );
        assert!(
            different_episode
                .content
                .contains("[OCR failed: timed out]")
        );
    }

    #[test]
    fn a_text_bearing_entry_never_collapses_into_a_text_free_one() {
        let observations = vec![
            observation(
                1,
                "2026-07-24T16:00:01Z",
                screen_payload("DISPLAY1", OcrStatus::NoText, None),
            ),
            observation(
                2,
                "2026-07-24T16:00:02Z",
                screen_payload("DISPLAY1", OcrStatus::Succeeded, Some("typed")),
            ),
        ];

        let episode = build_episode(
            timestamp("2026-07-24T16:00:00Z"),
            5,
            FixedOffset::east_opt(0).expect("UTC offset should be valid"),
            &observations,
        )
        .expect("the observations should build an episode");

        assert_eq!(episode.metadata.entry_count, "2");
        assert!(episode.content.contains("typed"));
    }

    #[test]
    fn separated_duplicates_are_not_collapsed() {
        let observations = vec![
            observation(
                1,
                "2026-07-24T16:00:01Z",
                screen_payload("DISPLAY1", OcrStatus::Succeeded, Some("a")),
            ),
            observation(
                2,
                "2026-07-24T16:00:02Z",
                screen_payload("DISPLAY1", OcrStatus::Succeeded, Some("b")),
            ),
            observation(
                3,
                "2026-07-24T16:00:03Z",
                screen_payload("DISPLAY1", OcrStatus::Succeeded, Some("a")),
            ),
        ];

        let episode = build_episode(
            timestamp("2026-07-24T16:00:00Z"),
            5,
            FixedOffset::east_opt(0).expect("UTC offset should be valid"),
            &observations,
        )
        .expect("the observations should build an episode");

        assert_eq!(episode.metadata.entry_count, "3");
    }

    #[test]
    fn an_empty_window_produces_no_episode() {
        assert_eq!(
            build_episode(
                timestamp("2026-07-24T16:00:00Z"),
                5,
                FixedOffset::east_opt(0).expect("UTC offset should be valid"),
                &[],
            ),
            None
        );
    }

    #[test]
    fn observations_outside_the_window_are_ignored() {
        let observations = vec![
            observation(
                1,
                "2026-07-24T15:59:59Z",
                screen_payload("DISPLAY1", OcrStatus::NoText, None),
            ),
            observation(
                2,
                "2026-07-24T16:05:00Z",
                screen_payload("DISPLAY1", OcrStatus::NoText, None),
            ),
        ];

        assert_eq!(
            build_episode(
                timestamp("2026-07-24T16:00:00Z"),
                5,
                FixedOffset::east_opt(0).expect("UTC offset should be valid"),
                &observations,
            ),
            None
        );
    }

    #[test]
    fn non_screen_observations_are_ignored() {
        let observations = vec![
            Observation {
                id: ulid::Ulid::from(1_u128),
                observed_at: timestamp("2026-07-24T16:00:01Z"),
                duration_ms: None,
                schema_version: CURRENT_SCHEMA_VERSION,
                payload: SourcePayload::Unknown {
                    source: "synthetic".to_owned(),
                    raw: serde_json::json!({"value": "synthetic"}),
                },
            },
            observation(
                2,
                "2026-07-24T16:00:02Z",
                screen_payload("DISPLAY1", OcrStatus::NoText, None),
            ),
        ];

        let episode = build_episode(
            timestamp("2026-07-24T16:00:00Z"),
            5,
            FixedOffset::east_opt(0).expect("UTC offset should be valid"),
            &observations,
        )
        .expect("the screen observation should build an episode");

        assert_eq!(episode.metadata.entry_count, "1");
        assert!(!episode.content.contains("synthetic"));
    }

    #[test]
    fn failed_ocr_without_an_error_message_is_still_marked() {
        let observations = vec![observation(
            1,
            "2026-07-24T16:00:01Z",
            screen_payload("DISPLAY1", OcrStatus::Failed, None),
        )];

        let episode = build_episode(
            timestamp("2026-07-24T16:00:00Z"),
            5,
            FixedOffset::east_opt(0).expect("UTC offset should be valid"),
            &observations,
        )
        .expect("the failed OCR observation should build an episode");

        assert!(episode.content.ends_with("    [OCR failed]"));
    }

    #[test]
    fn a_run_of_identical_text_collapses_to_its_first_entry() {
        let observations = vec![
            observation(
                1,
                "2026-07-24T16:00:01Z",
                ScreenPayload {
                    image_path: Some("images/a.webp".to_owned()),
                    ..screen_payload("DISPLAY1", OcrStatus::Succeeded, Some("x"))
                },
            ),
            observation(
                2,
                "2026-07-24T16:00:02Z",
                ScreenPayload {
                    image_path: Some("images/b.webp".to_owned()),
                    ..screen_payload("DISPLAY1", OcrStatus::Succeeded, Some("x"))
                },
            ),
            observation(
                3,
                "2026-07-24T16:00:03Z",
                ScreenPayload {
                    image_path: Some("images/c.webp".to_owned()),
                    ..screen_payload("DISPLAY1", OcrStatus::Succeeded, Some("x"))
                },
            ),
        ];

        let episode = build_episode(
            timestamp("2026-07-24T16:00:00Z"),
            5,
            FixedOffset::east_opt(0).expect("UTC offset should be valid"),
            &observations,
        )
        .expect("the observations should build an episode");

        assert_eq!(episode.metadata.entry_count, "1");
        assert_eq!(episode.metadata.image_paths, "[\"images/a.webp\"]");
        assert!(episode.content.contains("  16:00:01"));
        assert!(!episode.content.contains("  16:00:02"));
        assert!(!episode.content.contains("  16:00:03"));
    }

    #[test]
    fn trailing_blank_ocr_lines_do_not_end_the_content_with_a_newline() {
        let trailing = vec![observation(
            1,
            "2026-07-24T16:00:01Z",
            screen_payload("DISPLAY1", OcrStatus::Succeeded, Some("only line\n\n")),
        )];
        let trailing_episode = build_episode(
            timestamp("2026-07-24T16:00:00Z"),
            5,
            FixedOffset::east_opt(0).expect("UTC offset should be valid"),
            &trailing,
        )
        .expect("the observation should build an episode");

        assert!(trailing_episode.content.ends_with("    only line"));
        assert!(!trailing_episode.content.ends_with('\n'));

        let interior = vec![observation(
            1,
            "2026-07-24T16:00:01Z",
            screen_payload("DISPLAY1", OcrStatus::Succeeded, Some("a\n\nb\n\n")),
        )];
        let interior_episode = build_episode(
            timestamp("2026-07-24T16:00:00Z"),
            5,
            FixedOffset::east_opt(0).expect("UTC offset should be valid"),
            &interior,
        )
        .expect("the observation should build an episode");

        assert!(interior_episode.content.ends_with("    a\n\n    b"));
        assert!(!interior_episode.content.ends_with('\n'));
    }
}
