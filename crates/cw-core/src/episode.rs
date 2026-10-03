//! Grouping observations into fixed-length episodes for delivery.

const SCREEN_SOURCE: &str = "screen";

/// Hindsight splits retained text at blank lines before line breaks and packs the pieces into chunks
/// of `retain_chunk_size` characters, 3000 by default; a block this long reaches a chunk whole.
const BLOCK_CHARS: usize = 1000;

/// The line diff fills a table of old by new lines; a pair of captures past this many cells is rendered in full.
const MAX_DIFF_CELLS: usize = 1_000_000;

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
    /// Stable per-window id derived from the source, window start, and window length; the
    /// delivery layer suffixes the episode id onto it for the wire.
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
    /// Number of observation entries in `content` after folding, in decimal.
    pub entry_count: String,
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

/// Render the screen observations that fall in `[window_start, window_start + window_minutes)`,
/// and the capture-state spans that overlap it, into one episode. Returns `None` when there are
/// neither — an empty window must not become an empty document.
///
/// A span is clipped to the window, except an `Unrecorded` one: that renders whole, and only in
/// the window holding its end.
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
    spans: &[crate::model::StateSpan],
) -> Option<Episode> {
    // Taken through `timestamp` rather than by truncating the subsecond field: that field also
    // carries a leap second, as a value at or above one second, and truncating leaves it in place.
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

    entries.sort_unstable_by_key(|(observation, _)| (observation.observed_at, observation.id));
    entries.dedup_by(|current, previous| {
        let current = current.1;
        let previous = previous.1;

        current.foreground_process == previous.foreground_process
            && current.foreground_window_title == previous.foreground_window_title
            && current.ocr_status == previous.ocr_status
            && current.ocr_error == previous.ocr_error
            && current.ocr_text == previous.ocr_text
    });

    let states: Vec<_> = spans
        .iter()
        .filter_map(|span| {
            if span.status.state == crate::model::CaptureState::Unrecorded {
                (span.end_at >= window_start && span.end_at < end_at).then_some((
                    span.start_at,
                    span.end_at,
                    span,
                ))
            } else {
                (span.start_at < end_at && span.end_at >= window_start).then_some((
                    span.start_at.max(window_start),
                    span.end_at.min(end_at),
                    span,
                ))
            }
        })
        .collect();

    if entries.is_empty() && states.is_empty() {
        return None;
    }

    let render = |at: chrono::DateTime<chrono::Utc>| {
        at.with_timezone(&render_offset)
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
    };
    let rendered_start = render(window_start);
    let rendered_end = render(end_at);
    let rendered_chars =
        |lines: &[String]| -> usize { lines.iter().map(|line| line.chars().count() + 1).sum() };
    let mut blocks = Vec::new();
    let mut bases: std::collections::HashMap<_, (_, Vec<_>)> = std::collections::HashMap::new();

    for (from, to, span) in states {
        let mut state_line = format!("{} - {}", render(from), render(to));
        push_subject(
            &mut state_line,
            span.status.process.as_deref(),
            span.status.title.as_deref(),
        );
        let note = match span.status.state {
            crate::model::CaptureState::Paused => "capture paused",
            crate::model::CaptureState::Excluded => "capture skipped by the process blacklist",
            crate::model::CaptureState::NoTarget => "no capturable foreground window",
            crate::model::CaptureState::CaptureFailed => "capture failed",
            crate::model::CaptureState::NoNewFrame => "no new frame received",
            crate::model::CaptureState::Unchanged => "no change above the capture threshold",
            crate::model::CaptureState::SaveFailed => "storing the capture failed",
            crate::model::CaptureState::Unrecorded => "nothing recorded",
        };
        let note = match &span.status.detail {
            Some(detail) => format!("  [{note}: {detail}]"),
            None => format!("  [{note}]"),
        };
        let order = if span.status.state == crate::model::CaptureState::Unrecorded {
            (to, 0, span.id)
        } else {
            (from, 1, span.id)
        };
        blocks.push((order, vec![format!("{state_line}\n{note}")]));
    }

    let headers: Vec<_> = entries
        .iter()
        .map(|(observation, screen)| {
            let mut header = render(observation.observed_at);
            push_subject(
                &mut header,
                screen.foreground_process.as_deref(),
                screen.foreground_window_title.as_deref(),
            );
            header
        })
        .collect();
    let mut header_counts = std::collections::HashMap::new();
    for header in &headers {
        *header_counts.entry(header.as_str()).or_insert(0) += 1;
    }

    for (&(observation, screen), header) in entries.iter().zip(&headers) {
        let window = (
            screen.foreground_process.as_deref(),
            screen.foreground_window_title.as_deref(),
        );
        let mut entry_line = header.clone();
        let mut body = Vec::new();

        match &screen.ocr_status {
            crate::model::OcrStatus::Succeeded => {
                let mut ocr_lines: Vec<_> = screen
                    .ocr_text
                    .as_deref()
                    .unwrap_or_default()
                    .lines()
                    .map(|line| line.trim_end_matches('\r'))
                    .collect();
                while ocr_lines.last().is_some_and(|line| line.is_empty()) {
                    ocr_lines.pop();
                }
                body.extend(ocr_lines.iter().map(|line| format!("  {line}")));
                if let (Some(hwnd), Some(pid)) = (screen.foreground_hwnd, screen.foreground_pid) {
                    let key = (hwnd, pid, window);
                    if let Some((base_at, base_lines)) = bases.get(&key)
                        && let Some(changes) = changed_lines(base_lines, &ocr_lines)
                        && rendered_chars(&changes) < rendered_chars(&body)
                    {
                        entry_line = format!("{entry_line} (changes since {})", render(*base_at));
                        body = changes;
                    }
                    // A change entry names its base by header alone: a capture sharing its header
                    // can be no base, and the capture after it cannot name its previous one.
                    if header_counts[header.as_str()] == 1 {
                        bases.insert(key, (observation.observed_at, ocr_lines));
                    } else {
                        bases.remove(&key);
                    }
                }
            }
            crate::model::OcrStatus::NoText => {}
            crate::model::OcrStatus::Failed => match &screen.ocr_error {
                Some(error) => body.push(format!("  [OCR failed: {error}]")),
                None => body.push("  [OCR failed]".to_owned()),
            },
        }

        blocks.push((
            (observation.observed_at, 2, observation.id),
            entry_blocks(&entry_line, &body),
        ));
    }

    blocks.sort_by_key(|(order, _)| *order);
    let header = format!("[{rendered_start} - {rendered_end}] Screen episode");
    let content = std::iter::once(header)
        .chain(blocks.into_iter().flat_map(|(_, blocks)| blocks))
        .collect::<Vec<_>>()
        .join("\n\n");

    let episode_start = window_start.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let episode_end = end_at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);

    Some(Episode {
        source: SCREEN_SOURCE,
        start_at: window_start,
        end_at,
        document_id: format!("{SCREEN_SOURCE}-{episode_start}-{window_minutes}m"),
        content,
        metadata: EpisodeMetadata {
            episode_start,
            episode_end,
            entry_count: entries.len().to_string(),
        },
    })
}

fn entry_blocks(header: &str, body: &[String]) -> Vec<String> {
    let mut blocks = Vec::new();
    let mut block = header.to_owned();
    let mut block_chars = header.chars().count();

    for (index, line) in body.iter().enumerate() {
        let line_chars = line.chars().count();
        if index > 0 && block_chars + 1 + line_chars > BLOCK_CHARS {
            blocks.push(std::mem::replace(
                &mut block,
                format!("{header} (continued)"),
            ));
            block_chars = block.chars().count();
        }
        block.push('\n');
        block.push_str(line);
        block_chars += 1 + line_chars;
    }
    blocks.push(block);

    blocks
}

fn changed_lines(old: &[&str], new: &[&str]) -> Option<Vec<String>> {
    let prefix = old.iter().zip(new).take_while(|(a, b)| a == b).count();
    let suffix = old[prefix..]
        .iter()
        .rev()
        .zip(new[prefix..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    let old_middle = &old[prefix..old.len() - suffix];
    let new_middle = &new[prefix..new.len() - suffix];
    if old_middle.len().saturating_mul(new_middle.len()) > MAX_DIFF_CELLS {
        return None;
    }

    let width = new_middle.len() + 1;
    let mut lengths = vec![0_u16; (old_middle.len() + 1) * width];
    for i in (0..old_middle.len()).rev() {
        for j in (0..new_middle.len()).rev() {
            lengths[i * width + j] = if old_middle[i] == new_middle[j] {
                lengths[(i + 1) * width + j + 1] + 1
            } else {
                lengths[(i + 1) * width + j].max(lengths[i * width + j + 1])
            };
        }
    }

    let mut ops: Vec<_> = old[..prefix].iter().map(|line| (' ', *line)).collect();
    let (mut i, mut j) = (0, 0);
    while i < old_middle.len() || j < new_middle.len() {
        if i < old_middle.len() && j < new_middle.len() && old_middle[i] == new_middle[j] {
            ops.push((' ', old_middle[i]));
            i += 1;
            j += 1;
        } else if j == new_middle.len()
            || (i < old_middle.len() && lengths[(i + 1) * width + j] >= lengths[i * width + j + 1])
        {
            ops.push(('-', old_middle[i]));
            i += 1;
        } else {
            ops.push(('+', new_middle[j]));
            j += 1;
        }
    }
    ops.extend(old[old.len() - suffix..].iter().map(|line| (' ', *line)));

    let changed = |k: usize| ops.get(k).is_some_and(|(op, _)| *op != ' ');
    let marked: Vec<_> = (0..ops.len())
        .map(|k| {
            let shown = changed(k) || changed(k + 1) || k.checked_sub(1).is_some_and(changed);
            (shown, ops[k].0, ops[k].1)
        })
        .collect();
    let range = |before: usize, count: usize| match count {
        0 => format!("{before},0"),
        1 => format!("{}", before + 1),
        _ => format!("{},{count}", before + 1),
    };
    let mut body = Vec::new();
    let (mut old_before, mut new_before) = (0, 0);
    for run in marked.chunk_by(|a, b| a.0 == b.0) {
        let old_count = run.iter().filter(|(_, op, _)| *op != '+').count();
        let new_count = run.iter().filter(|(_, op, _)| *op != '-').count();
        if run[0].0 {
            body.push(format!(
                "  @@ -{} +{} @@",
                range(old_before, old_count),
                range(new_before, new_count)
            ));
            body.extend(run.iter().map(|(_, op, line)| format!("  {op} {line}")));
        }
        old_before += old_count;
        new_before += new_count;
    }
    if body.is_empty() {
        body.push("  (no text change)".to_owned());
    }

    Some(body)
}

fn push_subject(line: &mut String, process: Option<&str>, title: Option<&str>) {
    if let Some(process) = process {
        line.push_str(" [");
        line.push_str(process);
        line.push(']');
    }
    if let Some(title) = title {
        line.push(' ');
        line.push_str(title);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{
        CURRENT_SCHEMA_VERSION, CaptureState, CaptureStatus, Observation, OcrStatus, ScreenPayload,
        SourcePayload, StateSpan,
    };
    use chrono::{DateTime, FixedOffset, Utc};

    fn timestamp(value: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(value)
            .expect("test timestamp should be valid RFC 3339")
            .with_timezone(&Utc)
    }

    fn screen_payload(ocr_status: OcrStatus, ocr_text: Option<&str>) -> ScreenPayload {
        ScreenPayload {
            width: 1920,
            height: 1080,
            image_path: None,
            ocr_status,
            ocr_error: None,
            ocr_text: ocr_text.map(str::to_owned),
            ocr_langs: Vec::new(),
            foreground_process: None,
            foreground_window_title: None,
            foreground_hwnd: None,
            foreground_pid: None,
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
                    ..screen_payload(OcrStatus::Succeeded, Some("line one\nline two"))
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
                    ..screen_payload(OcrStatus::Succeeded, Some("line one\nline two"))
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
                    ..screen_payload(OcrStatus::Succeeded, Some("fn main() {}"))
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
                    ..screen_payload(OcrStatus::Failed, None)
                },
            ),
            observation(
                5,
                "2026-07-24T16:00:30Z",
                ScreenPayload {
                    image_path: Some("images/e.webp".to_owned()),
                    ..screen_payload(OcrStatus::NoText, None)
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
        // Only before the epoch do flooring and truncating toward zero part ways: truncated, 23:57
        // would get the window start 00:00:00, which is after the instant.
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
        let moved = reversed_and_interleaved.remove(0);
        reversed_and_interleaved.insert(2, moved);
        let start = timestamp("2026-07-24T16:00:00Z");
        let offset = FixedOffset::east_opt(9 * 3600).expect("test offset should be valid");

        let first = build_episode(start, 5, offset, &ordered, &[])
            .expect("the golden observations should build an episode");
        let second = build_episode(start, 5, offset, &reversed_and_interleaved, &[])
            .expect("the reordered observations should build an episode");

        assert_eq!(first, second);
    }

    #[test]
    fn two_observations_at_one_instant_render_in_id_order() {
        let lower = observation(
            1,
            "2026-07-24T16:00:02Z",
            ScreenPayload {
                foreground_window_title: Some("Alpha".to_owned()),
                ..screen_payload(OcrStatus::NoText, None)
            },
        );
        let higher = observation(
            2,
            "2026-07-24T16:00:02Z",
            ScreenPayload {
                foreground_window_title: Some("Beta".to_owned()),
                ..screen_payload(OcrStatus::NoText, None)
            },
        );
        let start = timestamp("2026-07-24T16:00:00Z");
        let offset = FixedOffset::east_opt(9 * 3600).expect("test offset should be valid");
        let forward_input = [lower.clone(), higher.clone()];
        let swapped_input = [higher, lower];

        let forward = build_episode(start, 5, offset, &forward_input, &[])
            .expect("two observations should build an episode");
        let swapped = build_episode(start, 5, offset, &swapped_input, &[])
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
        let five_minute_episode = build_episode(start, 5, offset, &observations, &[])
            .expect("the golden observations should build an episode");

        assert_eq!(
            five_minute_episode.document_id,
            "screen-2026-07-24T16:00:00Z-5m"
        );

        let ten_minute_episode = build_episode(start, 10, offset, &observations, &[])
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
            screen_payload(OcrStatus::Succeeded, Some("line one")),
        )];

        let episode = build_episode(
            start + chrono::Duration::milliseconds(900),
            5,
            offset,
            &observations,
            &[],
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

        // Built on `:58`: a leap nanosecond on `:59` spells `:60`, which collides with no ordinary
        // instant.
        assert_ne!(leap, ordinary);
        assert_eq!(
            leap.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            ordinary.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
        );

        let from_ordinary = build_episode(ordinary, 5, offset, &observations, &[])
            .expect("the golden observations should build an episode");
        let from_leap = build_episode(leap, 5, offset, &observations, &[])
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
            &[],
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
            &[],
        )
        .expect("the golden observations should build an episode");
        let golden = r#"[2026-07-25T01:00:00+09:00 - 2026-07-25T01:05:00+09:00] Screen episode

2026-07-25T01:00:02+09:00 [firefox.exe] Example Page
  line one
  line two

2026-07-25T01:00:30+09:00

2026-07-25T01:01:14+09:00 [Code.exe] contextwitness - Visual Studio Code
  fn main() {}

2026-07-25T01:02:00+09:00 [Code.exe] contextwitness - Visual Studio Code
  [OCR failed: engine unavailable]"#;

        assert_eq!(episode.content, golden);
        assert_eq!(
            serde_json::to_value(&episode.metadata)
                .expect("episode metadata should serialize to JSON"),
            serde_json::json!({
                "episode_start": "2026-07-24T16:00:00Z",
                "episode_end": "2026-07-24T16:05:00Z",
                "entry_count": "4"
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
            &[],
        )
        .expect("the golden observations should build an episode");
        let metadata = serde_json::to_value(&episode.metadata)
            .expect("episode metadata should serialize to JSON");
        let metadata = metadata
            .as_object()
            .expect("episode metadata should serialize as an object");

        for (key, value) in metadata {
            assert!(
                value.is_string(),
                "metadata field `{key}` must be a string, got {value}"
            );
        }
    }

    #[test]
    fn consecutive_entries_with_different_text_are_never_collapsed() {
        let observations = vec![
            observation(
                1,
                "2026-07-24T16:00:01Z",
                ScreenPayload {
                    image_path: Some("images/earlier.webp".to_owned()),
                    ..screen_payload(OcrStatus::Succeeded, Some("earlier"))
                },
            ),
            observation(
                2,
                "2026-07-24T16:00:02Z",
                ScreenPayload {
                    image_path: Some("images/later.webp".to_owned()),
                    ..screen_payload(OcrStatus::Succeeded, Some("later"))
                },
            ),
        ];

        let episode = build_episode(
            timestamp("2026-07-24T16:00:00Z"),
            5,
            FixedOffset::east_opt(0).expect("UTC offset should be valid"),
            &observations,
            &[],
        )
        .expect("the observations should build an episode");

        assert_eq!(episode.metadata.entry_count, "2");
        assert!(episode.content.contains("earlier"));
        assert!(episode.content.contains("later"));

        let hidden = vec![
            observation(
                3,
                "2026-07-24T16:00:01Z",
                screen_payload(OcrStatus::NoText, Some("earlier")),
            ),
            observation(
                4,
                "2026-07-24T16:00:02Z",
                screen_payload(OcrStatus::NoText, Some("later")),
            ),
        ];
        let episode = build_episode(
            timestamp("2026-07-24T16:00:00Z"),
            5,
            FixedOffset::east_opt(0).expect("UTC offset should be valid"),
            &hidden,
            &[],
        )
        .expect("the hidden-text observations should build an episode");
        assert_eq!(episode.metadata.entry_count, "2");
    }

    #[test]
    fn text_free_entries_from_the_same_application_fold() {
        let observations = vec![
            observation(
                1,
                "2026-07-24T16:00:01Z",
                ScreenPayload {
                    foreground_process: Some("vlc.exe".to_owned()),
                    foreground_window_title: Some("Movie".to_owned()),
                    ..screen_payload(OcrStatus::NoText, None)
                },
            ),
            observation(
                2,
                "2026-07-24T16:00:02Z",
                ScreenPayload {
                    foreground_process: Some("vlc.exe".to_owned()),
                    foreground_window_title: Some("Movie".to_owned()),
                    ..screen_payload(OcrStatus::NoText, None)
                },
            ),
        ];
        let episode = build_episode(
            timestamp("2026-07-24T16:00:00Z"),
            5,
            FixedOffset::east_opt(0).expect("UTC offset should be valid"),
            &observations,
            &[],
        )
        .expect("the observations should build an episode");

        assert_eq!(episode.metadata.entry_count, "1");
    }

    #[test]
    fn a_different_application_keeps_the_entry() {
        let observations = vec![
            observation(
                1,
                "2026-07-24T16:00:01Z",
                ScreenPayload {
                    foreground_process: Some("vlc.exe".to_owned()),
                    ..screen_payload(OcrStatus::NoText, None)
                },
            ),
            observation(
                2,
                "2026-07-24T16:00:02Z",
                ScreenPayload {
                    foreground_process: Some("game.exe".to_owned()),
                    ..screen_payload(OcrStatus::NoText, None)
                },
            ),
        ];
        let episode = build_episode(
            timestamp("2026-07-24T16:00:00Z"),
            5,
            FixedOffset::east_opt(0).expect("UTC offset should be valid"),
            &observations,
            &[],
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
                    ..screen_payload(OcrStatus::Succeeded, Some("same text"))
                },
            ),
            observation(
                2,
                "2026-07-24T16:00:02Z",
                ScreenPayload {
                    foreground_process: Some("editor.exe".to_owned()),
                    foreground_window_title: Some("second.txt".to_owned()),
                    ..screen_payload(OcrStatus::Succeeded, Some("same text"))
                },
            ),
        ];
        let episode = build_episode(
            timestamp("2026-07-24T16:00:00Z"),
            5,
            FixedOffset::east_opt(0).expect("UTC offset should be valid"),
            &observations,
            &[],
        )
        .expect("the observations should build an episode");

        assert_eq!(episode.metadata.entry_count, "2");
        assert!(episode.content.contains("first.txt"));
        assert!(episode.content.contains("second.txt"));
    }

    #[test]
    fn a_failed_entry_never_folds_into_a_no_text_one() {
        let observations = vec![
            observation(
                1,
                "2026-07-24T16:00:01Z",
                ScreenPayload {
                    ocr_error: Some("engine unavailable".to_owned()),
                    ..screen_payload(OcrStatus::NoText, None)
                },
            ),
            observation(
                2,
                "2026-07-24T16:00:02Z",
                ScreenPayload {
                    ocr_error: Some("engine unavailable".to_owned()),
                    ..screen_payload(OcrStatus::Failed, None)
                },
            ),
        ];

        let episode = build_episode(
            timestamp("2026-07-24T16:00:00Z"),
            5,
            FixedOffset::east_opt(0).expect("UTC offset should be valid"),
            &observations,
            &[],
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
                        ..screen_payload(OcrStatus::Failed, None)
                    },
                ),
                observation(
                    2,
                    "2026-07-24T16:00:02Z",
                    ScreenPayload {
                        ocr_error: Some(second_error.to_owned()),
                        ..screen_payload(OcrStatus::Failed, None)
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
            &[],
        )
        .expect("the identical failures should build an episode");

        assert_eq!(identical_episode.metadata.entry_count, "1");

        let different = failures("timed out");
        let different_episode = build_episode(
            timestamp("2026-07-24T16:00:00Z"),
            5,
            FixedOffset::east_opt(0).expect("UTC offset should be valid"),
            &different,
            &[],
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
                screen_payload(OcrStatus::NoText, None),
            ),
            observation(
                2,
                "2026-07-24T16:00:02Z",
                screen_payload(OcrStatus::Succeeded, Some("typed")),
            ),
        ];

        let episode = build_episode(
            timestamp("2026-07-24T16:00:00Z"),
            5,
            FixedOffset::east_opt(0).expect("UTC offset should be valid"),
            &observations,
            &[],
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
                screen_payload(OcrStatus::Succeeded, Some("a")),
            ),
            observation(
                2,
                "2026-07-24T16:00:02Z",
                screen_payload(OcrStatus::Succeeded, Some("b")),
            ),
            observation(
                3,
                "2026-07-24T16:00:03Z",
                screen_payload(OcrStatus::Succeeded, Some("a")),
            ),
        ];

        let episode = build_episode(
            timestamp("2026-07-24T16:00:00Z"),
            5,
            FixedOffset::east_opt(0).expect("UTC offset should be valid"),
            &observations,
            &[],
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
                screen_payload(OcrStatus::NoText, None),
            ),
            observation(
                2,
                "2026-07-24T16:05:00Z",
                screen_payload(OcrStatus::NoText, None),
            ),
        ];

        assert_eq!(
            build_episode(
                timestamp("2026-07-24T16:00:00Z"),
                5,
                FixedOffset::east_opt(0).expect("UTC offset should be valid"),
                &observations,
                &[],
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
                screen_payload(OcrStatus::NoText, None),
            ),
        ];

        let episode = build_episode(
            timestamp("2026-07-24T16:00:00Z"),
            5,
            FixedOffset::east_opt(0).expect("UTC offset should be valid"),
            &observations,
            &[],
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
            screen_payload(OcrStatus::Failed, None),
        )];

        let episode = build_episode(
            timestamp("2026-07-24T16:00:00Z"),
            5,
            FixedOffset::east_opt(0).expect("UTC offset should be valid"),
            &observations,
            &[],
        )
        .expect("the failed OCR observation should build an episode");

        assert!(episode.content.ends_with("\n  [OCR failed]"));
    }

    #[test]
    fn a_run_of_identical_text_collapses_to_its_first_entry() {
        let observations = vec![
            observation(
                1,
                "2026-07-24T16:00:01Z",
                ScreenPayload {
                    image_path: Some("images/a.webp".to_owned()),
                    ..screen_payload(OcrStatus::Succeeded, Some("x"))
                },
            ),
            observation(
                2,
                "2026-07-24T16:00:02Z",
                ScreenPayload {
                    image_path: Some("images/b.webp".to_owned()),
                    ..screen_payload(OcrStatus::Succeeded, Some("x"))
                },
            ),
            observation(
                3,
                "2026-07-24T16:00:03Z",
                ScreenPayload {
                    image_path: Some("images/c.webp".to_owned()),
                    ..screen_payload(OcrStatus::Succeeded, Some("x"))
                },
            ),
        ];

        let episode = build_episode(
            timestamp("2026-07-24T16:00:00Z"),
            5,
            FixedOffset::east_opt(0).expect("UTC offset should be valid"),
            &observations,
            &[],
        )
        .expect("the observations should build an episode");

        assert_eq!(episode.metadata.entry_count, "1");
        assert!(episode.content.contains("\n2026-07-24T16:00:01Z"));
        assert!(!episode.content.contains("\n2026-07-24T16:00:02Z"));
        assert!(!episode.content.contains("\n2026-07-24T16:00:03Z"));
    }

    #[test]
    fn trailing_blank_ocr_lines_do_not_end_the_content_with_a_newline() {
        let trailing = vec![observation(
            1,
            "2026-07-24T16:00:01Z",
            screen_payload(OcrStatus::Succeeded, Some("only line\n\n")),
        )];
        let trailing_episode = build_episode(
            timestamp("2026-07-24T16:00:00Z"),
            5,
            FixedOffset::east_opt(0).expect("UTC offset should be valid"),
            &trailing,
            &[],
        )
        .expect("the observation should build an episode");

        assert!(trailing_episode.content.ends_with("\n  only line"));
        assert!(!trailing_episode.content.ends_with('\n'));

        let interior = vec![observation(
            1,
            "2026-07-24T16:00:01Z",
            screen_payload(OcrStatus::Succeeded, Some("a\n\nb\n\n")),
        )];
        let interior_episode = build_episode(
            timestamp("2026-07-24T16:00:00Z"),
            5,
            FixedOffset::east_opt(0).expect("UTC offset should be valid"),
            &interior,
            &[],
        )
        .expect("the observation should build an episode");

        assert!(interior_episode.content.ends_with("\n  a\n  \n  b"));
        assert!(!interior_episode.content.ends_with('\n'));
    }

    fn status(
        state: CaptureState,
        process: Option<&str>,
        title: Option<&str>,
        detail: Option<&str>,
    ) -> CaptureStatus {
        CaptureStatus {
            state,
            process: process.map(str::to_owned),
            title: title.map(str::to_owned),
            detail: detail.map(str::to_owned),
        }
    }

    fn span(id: u128, start_at: &str, end_at: &str, status: CaptureStatus) -> StateSpan {
        StateSpan {
            id: ulid::Ulid::from(id),
            status,
            start_at: timestamp(start_at),
            end_at: timestamp(end_at),
        }
    }

    fn bare(state: CaptureState) -> CaptureStatus {
        status(state, None, None, None)
    }

    #[test]
    fn a_window_holding_only_capture_states_builds_an_episode() {
        let spans = [span(
            1,
            "2026-07-24T16:01:00Z",
            "2026-07-24T16:02:00Z",
            bare(CaptureState::Paused),
        )];

        let episode = build_episode(
            timestamp("2026-07-24T16:00:00Z"),
            5,
            FixedOffset::east_opt(0).expect("UTC offset should be valid"),
            &[],
            &spans,
        )
        .expect("a span alone should build an episode");

        assert_eq!(
            episode.content,
            "[2026-07-24T16:00:00Z - 2026-07-24T16:05:00Z] Screen episode\n\n\
             2026-07-24T16:01:00Z - 2026-07-24T16:02:00Z\n  [capture paused]"
        );
        assert_eq!(episode.metadata.entry_count, "0");
    }

    #[test]
    fn episode_content_interleaves_every_capture_state_golden() {
        let observations = [
            observation(
                1,
                "2026-07-24T16:01:10Z",
                ScreenPayload {
                    image_path: Some("images/a.webp".to_owned()),
                    foreground_process: Some("editor.exe".to_owned()),
                    foreground_window_title: Some("Example Page".to_owned()),
                    ..screen_payload(OcrStatus::Succeeded, Some("hello world"))
                },
            ),
            observation(
                2,
                "2026-07-24T16:03:00Z",
                ScreenPayload {
                    image_path: Some("images/b.webp".to_owned()),
                    foreground_process: Some("editor.exe".to_owned()),
                    foreground_window_title: Some("notes.txt".to_owned()),
                    ..screen_payload(OcrStatus::Succeeded, Some("line one"))
                },
            ),
        ];
        let page = |state| status(state, Some("editor.exe"), Some("Example Page"), None);
        let spans = [
            span(
                10,
                "2026-07-24T15:52:10Z",
                "2026-07-24T16:00:00Z",
                bare(CaptureState::Unrecorded),
            ),
            span(
                11,
                "2026-07-24T15:58:00Z",
                "2026-07-24T16:00:20Z",
                bare(CaptureState::Paused),
            ),
            span(
                12,
                "2026-07-24T16:00:30Z",
                "2026-07-24T16:00:50Z",
                status(CaptureState::Excluded, None, None, Some("private.exe")),
            ),
            span(
                13,
                "2026-07-24T16:01:00Z",
                "2026-07-24T16:01:00Z",
                status(CaptureState::NoTarget, Some("editor.exe"), None, None),
            ),
            span(
                14,
                "2026-07-24T16:01:20Z",
                "2026-07-24T16:02:00Z",
                page(CaptureState::Unchanged),
            ),
            span(
                15,
                "2026-07-24T16:02:10Z",
                "2026-07-24T16:02:20Z",
                page(CaptureState::NoNewFrame),
            ),
            span(
                16,
                "2026-07-24T16:02:30Z",
                "2026-07-24T16:02:30Z",
                status(
                    CaptureState::CaptureFailed,
                    Some("viewer.exe"),
                    Some("Example Image"),
                    Some("capture session ended"),
                ),
            ),
            span(
                17,
                "2026-07-24T16:02:40Z",
                "2026-07-24T16:02:50Z",
                status(
                    CaptureState::SaveFailed,
                    Some("editor.exe"),
                    Some("notes.txt"),
                    Some("image io: disk full"),
                ),
            ),
            span(
                18,
                "2026-07-24T16:03:10Z",
                "2026-07-24T16:07:30Z",
                status(
                    CaptureState::Unchanged,
                    Some("editor.exe"),
                    Some("notes.txt"),
                    None,
                ),
            ),
            span(
                19,
                "2026-07-24T16:04:00Z",
                "2026-07-24T16:06:00Z",
                bare(CaptureState::Unrecorded),
            ),
            span(
                20,
                "2026-07-24T16:05:00Z",
                "2026-07-24T16:06:00Z",
                bare(CaptureState::Paused),
            ),
            span(
                21,
                "2026-07-24T15:50:00Z",
                "2026-07-24T15:59:59Z",
                bare(CaptureState::Paused),
            ),
        ];

        let episode = build_episode(
            timestamp("2026-07-24T16:00:00Z"),
            5,
            FixedOffset::east_opt(9 * 3600).expect("test offset should be valid"),
            &observations,
            &spans,
        )
        .expect("the golden entries and spans should build an episode");
        let golden = r#"[2026-07-25T01:00:00+09:00 - 2026-07-25T01:05:00+09:00] Screen episode

2026-07-25T00:52:10+09:00 - 2026-07-25T01:00:00+09:00
  [nothing recorded]

2026-07-25T01:00:00+09:00 - 2026-07-25T01:00:20+09:00
  [capture paused]

2026-07-25T01:00:30+09:00 - 2026-07-25T01:00:50+09:00
  [capture skipped by the process blacklist: private.exe]

2026-07-25T01:01:00+09:00 - 2026-07-25T01:01:00+09:00 [editor.exe]
  [no capturable foreground window]

2026-07-25T01:01:10+09:00 [editor.exe] Example Page
  hello world

2026-07-25T01:01:20+09:00 - 2026-07-25T01:02:00+09:00 [editor.exe] Example Page
  [no change above the capture threshold]

2026-07-25T01:02:10+09:00 - 2026-07-25T01:02:20+09:00 [editor.exe] Example Page
  [no new frame received]

2026-07-25T01:02:30+09:00 - 2026-07-25T01:02:30+09:00 [viewer.exe] Example Image
  [capture failed: capture session ended]

2026-07-25T01:02:40+09:00 - 2026-07-25T01:02:50+09:00 [editor.exe] notes.txt
  [storing the capture failed: image io: disk full]

2026-07-25T01:03:00+09:00 [editor.exe] notes.txt
  line one

2026-07-25T01:03:10+09:00 - 2026-07-25T01:05:00+09:00 [editor.exe] notes.txt
  [no change above the capture threshold]"#;

        assert_eq!(episode.content, golden);
        assert_eq!(episode.metadata.entry_count, "2");
    }

    #[test]
    fn a_span_is_clipped_to_both_window_edges() {
        let spans = [span(
            1,
            "2026-07-24T15:58:00Z",
            "2026-07-24T16:07:00Z",
            bare(CaptureState::Unchanged),
        )];

        let episode = build_episode(
            timestamp("2026-07-24T16:00:00Z"),
            5,
            FixedOffset::east_opt(0).expect("UTC offset should be valid"),
            &[],
            &spans,
        )
        .expect("a span covering the window should build an episode");

        assert!(
            episode
                .content
                .ends_with("\n\n2026-07-24T16:00:00Z - 2026-07-24T16:05:00Z\n  [no change above the capture threshold]")
        );
    }

    #[test]
    fn an_unrecorded_span_renders_whole_and_only_in_the_window_holding_its_end() {
        let spans = [span(
            1,
            "2026-07-24T15:50:00Z",
            "2026-07-24T16:12:30Z",
            bare(CaptureState::Unrecorded),
        )];
        let build = |start: &str| {
            build_episode(
                timestamp(start),
                5,
                FixedOffset::east_opt(0).expect("UTC offset should be valid"),
                &[],
                &spans,
            )
        };

        for elsewhere in [
            "2026-07-24T15:50:00Z",
            "2026-07-24T16:00:00Z",
            "2026-07-24T16:05:00Z",
            "2026-07-24T16:15:00Z",
        ] {
            assert_eq!(build(elsewhere), None, "{elsewhere}");
        }
        let episode = build("2026-07-24T16:10:00Z")
            .expect("the window holding the end should build an episode");
        assert!(
            episode
                .content
                .ends_with("\n\n2026-07-24T15:50:00Z - 2026-07-24T16:12:30Z\n  [nothing recorded]")
        );
    }

    #[test]
    fn a_span_renders_before_an_entry_at_the_same_instant_and_spans_tie_by_id() {
        let observations = [observation(
            1,
            "2026-07-24T16:01:00Z",
            ScreenPayload {
                foreground_window_title: Some("Entry".to_owned()),
                ..screen_payload(OcrStatus::NoText, None)
            },
        )];
        let spans = [
            span(
                3,
                "2026-07-24T16:01:00Z",
                "2026-07-24T16:01:00Z",
                status(CaptureState::NoNewFrame, None, Some("Higher"), None),
            ),
            span(
                2,
                "2026-07-24T16:01:00Z",
                "2026-07-24T16:01:00Z",
                status(CaptureState::Unchanged, None, Some("Lower"), None),
            ),
        ];

        let episode = build_episode(
            timestamp("2026-07-24T16:00:00Z"),
            5,
            FixedOffset::east_opt(0).expect("UTC offset should be valid"),
            &observations,
            &spans,
        )
        .expect("the tied entry and spans should build an episode");

        assert_eq!(
            episode.content,
            "[2026-07-24T16:00:00Z - 2026-07-24T16:05:00Z] Screen episode\n\n\
             2026-07-24T16:01:00Z - 2026-07-24T16:01:00Z Lower\n  [no change above the capture threshold]\n\n\
             2026-07-24T16:01:00Z - 2026-07-24T16:01:00Z Higher\n  [no new frame received]\n\n\
             2026-07-24T16:01:00Z Entry"
        );
    }

    #[test]
    fn an_unrecorded_span_renders_where_recording_resumed() {
        let observations = [observation(
            1,
            "2026-07-24T16:01:00Z",
            ScreenPayload {
                foreground_window_title: Some("Before".to_owned()),
                ..screen_payload(OcrStatus::NoText, None)
            },
        )];
        let spans = [
            span(
                2,
                "2026-07-24T16:03:00Z",
                "2026-07-24T16:03:00Z",
                status(CaptureState::NoNewFrame, None, Some("After"), None),
            ),
            span(
                3,
                "2026-07-24T16:01:00Z",
                "2026-07-24T16:03:00Z",
                bare(CaptureState::Unrecorded),
            ),
        ];

        let episode = build_episode(
            timestamp("2026-07-24T16:00:00Z"),
            5,
            FixedOffset::east_opt(0).expect("UTC offset should be valid"),
            &observations,
            &spans,
        )
        .expect("the entry and spans should build an episode");

        assert_eq!(
            episode.content,
            "[2026-07-24T16:00:00Z - 2026-07-24T16:05:00Z] Screen episode\n\n\
             2026-07-24T16:01:00Z Before\n\n\
             2026-07-24T16:01:00Z - 2026-07-24T16:03:00Z\n  [nothing recorded]\n\n\
             2026-07-24T16:03:00Z - 2026-07-24T16:03:00Z After\n  [no new frame received]"
        );
    }

    const EPISODE_HEADER: &str = "[2026-07-24T16:00:00Z - 2026-07-24T16:05:00Z] Screen episode";
    const ENTRY_HEADER: &str = "2026-07-24T16:00:01Z [editor.exe] Example Page";

    fn single_entry_episode(text: &str) -> Episode {
        let observations = [observation(
            1,
            "2026-07-24T16:00:01Z",
            ScreenPayload {
                foreground_process: Some("editor.exe".to_owned()),
                foreground_window_title: Some("Example Page".to_owned()),
                ..screen_payload(OcrStatus::Succeeded, Some(text))
            },
        )];

        build_episode(
            timestamp("2026-07-24T16:00:00Z"),
            5,
            FixedOffset::east_opt(0).expect("UTC offset should be valid"),
            &observations,
            &[],
        )
        .expect("the observation should build an episode")
    }

    #[test]
    fn an_entry_longer_than_a_block_continues_under_its_own_header() {
        let ocr_lines: Vec<_> = (1..=80)
            .map(|n| format!("line {n} {}", "x".repeat(40)))
            .collect();
        let episode = single_entry_episode(&ocr_lines.join("\n"));
        let blocks: Vec<_> = episode.content.split("\n\n").collect();
        let continued = format!("{ENTRY_HEADER} (continued)");

        assert_eq!(blocks[0], EPISODE_HEADER);
        let entry = &blocks[1..];
        assert!(entry.len() > 2, "the entry should span several blocks");

        let mut reassembled = Vec::new();
        for (index, block) in entry.iter().enumerate() {
            assert!(
                block.chars().count() <= BLOCK_CHARS,
                "block {index} holds {} characters",
                block.chars().count()
            );
            let mut lines = block.lines();
            let first = lines.next().expect("a block should have a first line");
            if index == 0 {
                assert_eq!(first, ENTRY_HEADER);
                reassembled.push(first);
            } else {
                assert_eq!(first, continued);
            }
            reassembled.extend(lines);
        }
        let expected: Vec<_> = std::iter::once(ENTRY_HEADER.to_owned())
            .chain(ocr_lines.iter().map(|line| format!("  {line}")))
            .collect();
        assert_eq!(reassembled, expected);

        for pair in entry.windows(2) {
            let next_line = pair[1]
                .lines()
                .nth(1)
                .expect("a continuation block should hold a line after its header");
            assert!(
                pair[0].chars().count() + 1 + next_line.chars().count() > BLOCK_CHARS,
                "a block should close only when its next line would not fit"
            );
        }
    }

    #[test]
    fn an_ocr_line_longer_than_a_block_stays_whole() {
        let long = "y".repeat(BLOCK_CHARS + 200);
        let episode = single_entry_episode(&format!("{long}\nline 2\n{long}"));
        let continued = format!("{ENTRY_HEADER} (continued)");

        assert_eq!(
            episode.content.split("\n\n").collect::<Vec<_>>(),
            [
                EPISODE_HEADER.to_owned(),
                format!("{ENTRY_HEADER}\n  {long}"),
                format!("{continued}\n  line 2"),
                format!("{continued}\n  {long}"),
            ]
        );
    }

    #[test]
    fn a_blank_line_only_ever_separates_blocks() {
        let long: Vec<_> = (1..=40)
            .map(|n| format!("line {n} {}", "x".repeat(40)))
            .collect();
        let observations = [
            observation(
                1,
                "2026-07-24T16:00:01Z",
                ScreenPayload {
                    foreground_process: Some("editor.exe".to_owned()),
                    foreground_window_title: Some("Example Page".to_owned()),
                    ..screen_payload(
                        OcrStatus::Succeeded,
                        Some("\nline 1\n\nline 2\r\n\r\n\r\nline 3\n\n"),
                    )
                },
            ),
            observation(
                2,
                "2026-07-24T16:00:03Z",
                ScreenPayload {
                    foreground_process: Some("editor.exe".to_owned()),
                    foreground_window_title: Some("Other Page".to_owned()),
                    ..screen_payload(OcrStatus::Succeeded, Some(&long.join("\n\n")))
                },
            ),
            capture(
                4,
                "2026-07-24T16:00:04Z",
                "viewer.exe",
                "Example Page",
                "line 1\nline 2\n\n\nline 5\nline 6\n\nline 8\nline 9\nline 10",
            ),
            capture(
                5,
                "2026-07-24T16:00:05Z",
                "viewer.exe",
                "Example Page",
                "line 1\nline 2\n\nline 4\nline 5\n\n\nline 8\nline 9\nline 10",
            ),
        ];
        let spans = [span(
            3,
            "2026-07-24T16:00:02Z",
            "2026-07-24T16:00:02Z",
            bare(CaptureState::Unchanged),
        )];

        let episode = build_episode(
            timestamp("2026-07-24T16:00:00Z"),
            5,
            FixedOffset::east_opt(0).expect("UTC offset should be valid"),
            &observations,
            &spans,
        )
        .expect("the entries and the span should build an episode");
        let headers = [
            EPISODE_HEADER,
            ENTRY_HEADER,
            "2026-07-24T16:00:02Z - 2026-07-24T16:00:02Z",
            "2026-07-24T16:00:03Z [editor.exe] Other Page",
            "2026-07-24T16:00:03Z [editor.exe] Other Page (continued)",
            "2026-07-24T16:00:04Z [viewer.exe] Example Page",
            "2026-07-24T16:00:05Z [viewer.exe] Example Page (changes since 2026-07-24T16:00:04Z)",
        ];

        assert!(!episode.content.contains("\n\n\n"));
        assert!(!episode.content.starts_with('\n') && !episode.content.ends_with('\n'));
        let blocks: Vec<_> = episode.content.split("\n\n").collect();
        assert!(
            blocks.len() > 6,
            "the long entry should span several blocks"
        );
        assert_eq!(
            blocks[blocks.len() - 1],
            format!(
                "{}\n  @@ -3,5 +3,5 @@\n    \n  - \n  + line 4\n    line 5\n  - line 6\n  + \n    ",
                headers[6]
            )
        );
        assert_eq!(blocks[0], EPISODE_HEADER);
        for block in &blocks[1..] {
            let first = block.lines().next().expect("a block should not be empty");
            assert!(
                headers[1..].contains(&first),
                "a block should start at a header, not at `{first}`"
            );
        }
        assert_eq!(
            blocks[1],
            format!("{ENTRY_HEADER}\n  \n  line 1\n  \n  line 2\n  \n  \n  line 3")
        );
    }

    #[test]
    fn a_block_is_measured_in_characters_not_bytes() {
        let first = "あ".repeat(400);
        let filling = BLOCK_CHARS - ENTRY_HEADER.chars().count() - (1 + 2 + 400) - (1 + 2);

        let fits = single_entry_episode(&format!("{first}\n{}", "あ".repeat(filling)));
        let blocks: Vec<_> = fits.content.split("\n\n").collect();
        assert_eq!(
            blocks.len(),
            2,
            "a block of exactly the limit should stay whole"
        );
        assert_eq!(blocks[1].chars().count(), BLOCK_CHARS);
        assert!(blocks[1].len() > BLOCK_CHARS);

        let overflows = single_entry_episode(&format!("{first}\n{}", "あ".repeat(filling + 1)));
        assert_eq!(
            overflows.content.split("\n\n").count(),
            3,
            "one character past the limit should start a new block"
        );
    }

    fn capture(id: u128, observed_at: &str, process: &str, title: &str, text: &str) -> Observation {
        observation(
            id,
            observed_at,
            ScreenPayload {
                foreground_process: Some(process.to_owned()),
                foreground_window_title: Some(title.to_owned()),
                foreground_hwnd: Some(1),
                foreground_pid: Some(10),
                ..screen_payload(OcrStatus::Succeeded, Some(text))
            },
        )
    }

    fn with_ids(mut observation: Observation, hwnd: Option<i64>, pid: Option<u32>) -> Observation {
        if let SourcePayload::Screen(screen) = &mut observation.payload {
            screen.foreground_hwnd = hwnd;
            screen.foreground_pid = pid;
        }
        observation
    }

    fn episode_blocks(observations: &[Observation]) -> Vec<String> {
        build_episode(
            timestamp("2026-07-24T16:00:00Z"),
            5,
            FixedOffset::east_opt(0).expect("UTC offset should be valid"),
            observations,
            &[],
        )
        .expect("the observations should build an episode")
        .content
        .split("\n\n")
        .map(str::to_owned)
        .collect()
    }

    const SECOND_ENTRY_HEADER: &str = "2026-07-24T16:00:02Z [editor.exe] Example Page";
    const SECOND_SINCE_FIRST: &str =
        "2026-07-24T16:00:02Z [editor.exe] Example Page (changes since 2026-07-24T16:00:01Z)";

    #[test]
    fn a_window_seen_again_renders_the_lines_changed_since_its_previous_capture() {
        let first = [
            "Title", "line 2", "line 3", "line 4", "line 5", "line 6", "line 7", "line 8",
            "line 9", "line 10", "line 11",
        ];
        let again = [
            "Title",
            "line 2",
            "line 3 edited",
            "line 4",
            "line 5",
            "line 6",
            "line 7",
            "line 8",
            "line 9",
            "line 10",
            "line 11",
            "line 12",
        ];
        let observations = [
            capture(
                1,
                "2026-07-24T16:00:01Z",
                "editor.exe",
                "Example Page",
                &first.join("\n"),
            ),
            capture(
                2,
                "2026-07-24T16:00:02Z",
                "viewer.exe",
                "Other Page",
                "Status: Done",
            ),
            capture(
                3,
                "2026-07-24T16:00:03Z",
                "editor.exe",
                "Example Page",
                &again.join("\n"),
            ),
        ];

        let episode = build_episode(
            timestamp("2026-07-24T16:00:00Z"),
            5,
            FixedOffset::east_opt(9 * 3600).expect("test offset should be valid"),
            &observations,
            &[],
        )
        .expect("the observations should build an episode");
        let golden = r#"[2026-07-25T01:00:00+09:00 - 2026-07-25T01:05:00+09:00] Screen episode

2026-07-25T01:00:01+09:00 [editor.exe] Example Page
  Title
  line 2
  line 3
  line 4
  line 5
  line 6
  line 7
  line 8
  line 9
  line 10
  line 11

2026-07-25T01:00:02+09:00 [viewer.exe] Other Page
  Status: Done

2026-07-25T01:00:03+09:00 [editor.exe] Example Page (changes since 2026-07-25T01:00:01+09:00)
  @@ -2,3 +2,3 @@
    line 2
  - line 3
  + line 3 edited
    line 4
  @@ -11 +11,2 @@
    line 11
  + line 12"#;

        assert_eq!(episode.content, golden);
        assert_eq!(episode.metadata.entry_count, "3");
    }

    #[test]
    fn a_replaced_line_renders_as_its_deletion_then_its_insertion() {
        let blocks = episode_blocks(&[
            capture(
                1,
                "2026-07-24T16:00:01Z",
                "editor.exe",
                "Example Page",
                "Title\nStatus: Done\nOwner: A\nline 4\nline 5\nline 6\nline 7\nline 8",
            ),
            capture(
                2,
                "2026-07-24T16:00:02Z",
                "editor.exe",
                "Example Page",
                "Title\nStatus: Open\nOwner: A\nline 4\nline 5\nline 6\nline 7\nline 8",
            ),
        ]);

        assert_eq!(
            blocks[2..],
            [format!(
                "{SECOND_SINCE_FIRST}\n  @@ -1,3 +1,3 @@\n    Title\n  - Status: Done\n  + Status: Open\n    Owner: A"
            )]
        );
    }

    #[test]
    fn a_line_that_returns_is_inserted_against_the_capture_that_lacked_it() {
        let with_x = "line 1\nline 2\nline 3\nx\nline 5\nline 6\nline 7";
        let blocks = episode_blocks(&[
            capture(
                1,
                "2026-07-24T16:00:01Z",
                "editor.exe",
                "Example Page",
                with_x,
            ),
            capture(
                2,
                "2026-07-24T16:00:02Z",
                "editor.exe",
                "Example Page",
                "line 1\nline 2\nline 3\nline 5\nline 6\nline 7",
            ),
            capture(
                3,
                "2026-07-24T16:00:03Z",
                "editor.exe",
                "Example Page",
                with_x,
            ),
        ]);

        assert_eq!(
            blocks[2..],
            [
                format!("{SECOND_SINCE_FIRST}\n  @@ -3,3 +3,2 @@\n    line 3\n  - x\n    line 5"),
                "2026-07-24T16:00:03Z [editor.exe] Example Page (changes since 2026-07-24T16:00:02Z)\n  @@ -3,2 +3,3 @@\n    line 3\n  + x\n    line 5".to_owned(),
            ]
        );
    }

    #[test]
    fn a_header_two_entries_share_never_names_a_base() {
        let text = |second: &str| {
            let rest: Vec<_> = (3..=20).map(|n| format!("line {n}")).collect();
            format!("Title\n{second}\n{}", rest.join("\n"))
        };
        let shared_second = "2026-07-24T16:00:01.700Z";
        let partners = [
            capture(
                3,
                shared_second,
                "editor.exe",
                "Example Page",
                &text("line 2 edited twice"),
            ),
            with_ids(
                capture(
                    3,
                    shared_second,
                    "editor.exe",
                    "Example Page",
                    &text("another window"),
                ),
                Some(2),
                Some(20),
            ),
            observation(
                3,
                shared_second,
                ScreenPayload {
                    foreground_process: Some("editor.exe".to_owned()),
                    foreground_window_title: Some("Example Page".to_owned()),
                    foreground_hwnd: Some(1),
                    foreground_pid: Some(10),
                    ..screen_payload(OcrStatus::Failed, None)
                },
            ),
        ];

        for partner in partners {
            let blocks = episode_blocks(&[
                capture(
                    1,
                    "2026-07-24T16:00:00Z",
                    "editor.exe",
                    "Example Page",
                    &text("line 2"),
                ),
                capture(
                    2,
                    "2026-07-24T16:00:01.200Z",
                    "editor.exe",
                    "Example Page",
                    &text("line 2 edited"),
                ),
                partner,
                capture(
                    4,
                    "2026-07-24T16:00:02Z",
                    "editor.exe",
                    "Example Page",
                    &text("line 2 edited again"),
                ),
            ]);

            assert!(
                blocks
                    .iter()
                    .all(|block| !block.contains("(changes since 2026-07-24T16:00:01Z)")),
                "two entries are headed at 16:00:01, so no change entry may name it: {blocks:#?}"
            );
            assert_eq!(
                blocks.last(),
                Some(&format!(
                    "{SECOND_ENTRY_HEADER}\n  {}",
                    text("line 2 edited again").replace('\n', "\n  ")
                )),
                "the capture after them cannot name its previous capture, so it renders in full"
            );
        }
    }

    #[test]
    fn identical_text_seen_again_in_the_same_window_renders_no_text_change() {
        let text = "Status: Done\nline 2\nline 3";
        let blocks = episode_blocks(&[
            capture(
                1,
                "2026-07-24T16:00:01Z",
                "editor.exe",
                "Example Page",
                text,
            ),
            capture(2, "2026-07-24T16:00:02Z", "editor.exe", "Other Page", text),
            capture(
                3,
                "2026-07-24T16:00:03Z",
                "viewer.exe",
                "Example Page",
                text,
            ),
            capture(
                4,
                "2026-07-24T16:00:04Z",
                "editor.exe",
                "Example Page",
                text,
            ),
        ]);

        assert_eq!(
            blocks[2..],
            [
                "2026-07-24T16:00:02Z [editor.exe] Other Page\n  Status: Done\n  line 2\n  line 3",
                "2026-07-24T16:00:03Z [viewer.exe] Example Page\n  Status: Done\n  line 2\n  line 3",
                "2026-07-24T16:00:04Z [editor.exe] Example Page (changes since 2026-07-24T16:00:01Z)\n  (no text change)",
            ]
        );
    }

    #[test]
    fn a_change_list_no_shorter_than_the_full_text_renders_in_full() {
        let blocks = episode_blocks(&[
            capture(
                1,
                "2026-07-24T16:00:01Z",
                "editor.exe",
                "Example Page",
                "line 1\nline 2\nline 3",
            ),
            capture(
                2,
                "2026-07-24T16:00:02Z",
                "editor.exe",
                "Example Page",
                "line 4\nline 5\nline 6",
            ),
        ]);
        assert_eq!(
            blocks[2],
            format!("{SECOND_ENTRY_HEADER}\n  line 4\n  line 5\n  line 6")
        );

        let seen_twice = |text: &str| {
            episode_blocks(&[
                capture(
                    1,
                    "2026-07-24T16:00:01Z",
                    "editor.exe",
                    "Example Page",
                    text,
                ),
                capture(
                    2,
                    "2026-07-24T16:00:02Z",
                    "viewer.exe",
                    "Other Page",
                    "line 1",
                ),
                capture(
                    3,
                    "2026-07-24T16:00:03Z",
                    "editor.exe",
                    "Example Page",
                    text,
                ),
            ])
            .pop()
            .expect("the episode should hold a last block")
        };
        let header = "2026-07-24T16:00:03Z [editor.exe] Example Page";
        assert_eq!(
            "  line 3 of a page".chars().count(),
            "  (no text change)".chars().count()
        );
        assert_eq!(
            seen_twice("line 3 of a page"),
            format!("{header}\n  line 3 of a page")
        );
        assert_eq!(
            seen_twice("line 13 of a page"),
            format!("{header} (changes since 2026-07-24T16:00:01Z)\n  (no text change)")
        );
    }

    #[test]
    fn a_failed_capture_neither_shows_changes_nor_becomes_the_base() {
        let blocks = episode_blocks(&[
            capture(
                1,
                "2026-07-24T16:00:01Z",
                "editor.exe",
                "Example Page",
                "Title\nStatus: Done\nOwner: A\nline 4\nline 5\nline 6\nline 7\nline 8",
            ),
            observation(
                2,
                "2026-07-24T16:00:02Z",
                ScreenPayload {
                    foreground_process: Some("editor.exe".to_owned()),
                    foreground_window_title: Some("Example Page".to_owned()),
                    ocr_error: Some("engine unavailable".to_owned()),
                    ..screen_payload(OcrStatus::Failed, None)
                },
            ),
            observation(
                3,
                "2026-07-24T16:00:03Z",
                ScreenPayload {
                    foreground_process: Some("editor.exe".to_owned()),
                    foreground_window_title: Some("Example Page".to_owned()),
                    ..screen_payload(OcrStatus::NoText, None)
                },
            ),
            capture(
                4,
                "2026-07-24T16:00:04Z",
                "editor.exe",
                "Example Page",
                "Title\nStatus: Open\nOwner: A\nline 4\nline 5\nline 6\nline 7\nline 8",
            ),
        ]);

        assert_eq!(
            blocks[2..],
            [
                format!("{SECOND_ENTRY_HEADER}\n  [OCR failed: engine unavailable]"),
                "2026-07-24T16:00:03Z [editor.exe] Example Page".to_owned(),
                "2026-07-24T16:00:04Z [editor.exe] Example Page (changes since 2026-07-24T16:00:01Z)\n  @@ -1,3 +1,3 @@\n    Title\n  - Status: Done\n  + Status: Open\n    Owner: A".to_owned(),
            ]
        );
    }

    #[test]
    fn a_change_list_longer_than_a_block_continues_under_its_changes_since_header() {
        let line = |n: usize, fill: &str| format!("line {n} {}", fill.repeat(40));
        let page = |changed: &str| {
            (1..=200)
                .map(|n| line(n, if (50..70).contains(&n) { changed } else { "x" }))
                .collect::<Vec<_>>()
                .join("\n")
        };
        let blocks = episode_blocks(&[
            capture(
                1,
                "2026-07-24T16:00:01Z",
                "editor.exe",
                "Example Page",
                &page("x"),
            ),
            capture(
                2,
                "2026-07-24T16:00:02Z",
                "editor.exe",
                "Example Page",
                &page("y"),
            ),
        ]);
        let continued = format!("{SECOND_SINCE_FIRST} (continued)");
        let start = blocks
            .iter()
            .position(|block| block.starts_with(SECOND_ENTRY_HEADER))
            .expect("the second capture should be rendered");
        let entry = &blocks[start..];
        assert!(
            entry.len() > 2,
            "the change list should span several blocks"
        );

        let mut reassembled = Vec::new();
        for (index, block) in entry.iter().enumerate() {
            assert!(block.chars().count() <= BLOCK_CHARS);
            let mut lines = block.lines();
            let expected_header = if index == 0 {
                SECOND_SINCE_FIRST
            } else {
                continued.as_str()
            };
            assert_eq!(lines.next(), Some(expected_header));
            reassembled.extend(lines.map(str::to_owned));
        }
        let expected: Vec<_> = [
            "  @@ -49,22 +49,22 @@".to_owned(),
            format!("    {}", line(49, "x")),
        ]
        .into_iter()
        .chain((50..70).map(|n| format!("  - {}", line(n, "x"))))
        .chain((50..70).map(|n| format!("  + {}", line(n, "y"))))
        .chain(std::iter::once(format!("    {}", line(70, "x"))))
        .collect();
        assert_eq!(reassembled, expected);
    }

    #[test]
    fn only_the_lines_between_the_common_ends_count_against_the_diff_limit() {
        let page = |count: usize, first: &str, last: &str| {
            std::iter::once(first.to_owned())
                .chain((2..count).map(|n| format!("line {n}")))
                .chain(std::iter::once(last.to_owned()))
                .collect::<Vec<_>>()
                .join("\n")
        };
        let pair = |old: &str, new: &str| {
            episode_blocks(&[
                capture(1, "2026-07-24T16:00:01Z", "editor.exe", "Example Page", old),
                capture(2, "2026-07-24T16:00:02Z", "editor.exe", "Example Page", new),
            ])
        };

        let over = pair(
            &page(1001, "first old", "last old"),
            &page(1001, "first new", "last new"),
        );
        assert!(over.iter().all(|block| !block.contains("changes since")));
        assert!(over.iter().any(|block| {
            block.starts_with(&format!("{SECOND_ENTRY_HEADER}\n  first new\n  line 2\n"))
        }));

        let at_limit = pair(
            &page(1000, "first old", "last old"),
            &page(1000, "first new", "last new"),
        );
        assert_eq!(
            at_limit.last(),
            Some(&format!(
                "{SECOND_SINCE_FIRST}\n  @@ -1,2 +1,2 @@\n  - first old\n  + first new\n    line 2\n  @@ -999,2 +999,2 @@\n    line 999\n  - last old\n  + last new"
            ))
        );

        let long = page(1500, "line 1", "line 1500");
        let edited = long.replace("\nline 750\n", "\nline 750 edited\n");
        assert_eq!(
            pair(&long, &edited).last(),
            Some(&format!(
                "{SECOND_SINCE_FIRST}\n  @@ -749,3 +749,3 @@\n    line 749\n  - line 750\n  + line 750 edited\n    line 751"
            ))
        );
    }

    #[test]
    fn a_common_line_between_two_changes_shows_once_and_distant_ones_not_at_all() {
        let blocks = episode_blocks(&[
            capture(
                1,
                "2026-07-24T16:00:01Z",
                "editor.exe",
                "Example Page",
                "line 1\nline 2\nline 3\na1\na2\nline 6\nb\nline 8\nline 9\nline 10\nline 11\nline 12",
            ),
            capture(
                2,
                "2026-07-24T16:00:02Z",
                "editor.exe",
                "Example Page",
                "line 1\nline 2\nline 3\nA1\nA2\nline 6\nB\nline 8\nline 9\nline 10\nline 11\nline 12",
            ),
        ]);

        assert_eq!(
            blocks[2],
            format!(
                "{SECOND_SINCE_FIRST}\n  @@ -3,6 +3,6 @@\n    line 3\n  - a1\n  - a2\n  + A1\n  + A2\n    line 6\n  - b\n  + B\n    line 8"
            )
        );
    }

    #[test]
    fn the_same_edit_at_different_lines_renders_different_hunk_headers() {
        let base = "Item A\nDescription\nValue: 100\nUnit: yen\nItem B\nDescription\nValue: 100\nUnit: yen";
        let edited_at = |index: usize| {
            let mut lines: Vec<_> = base.lines().collect();
            lines[index] = "Value: 200";
            episode_blocks(&[
                capture(
                    1,
                    "2026-07-24T16:00:01Z",
                    "editor.exe",
                    "Example Page",
                    base,
                ),
                capture(
                    2,
                    "2026-07-24T16:00:02Z",
                    "editor.exe",
                    "Example Page",
                    &lines.join("\n"),
                ),
            ])
        };

        assert_eq!(
            edited_at(2)[2..],
            [format!(
                "{SECOND_SINCE_FIRST}\n  @@ -2,3 +2,3 @@\n    Description\n  - Value: 100\n  + Value: 200\n    Unit: yen"
            )]
        );
        assert_eq!(
            edited_at(6)[2..],
            [format!(
                "{SECOND_SINCE_FIRST}\n  @@ -6,3 +6,3 @@\n    Description\n  - Value: 100\n  + Value: 200\n    Unit: yen"
            )]
        );
    }

    #[test]
    fn text_after_a_capture_with_no_lines_renders_in_full() {
        let blocks = episode_blocks(&[
            capture(1, "2026-07-24T16:00:01Z", "editor.exe", "Example Page", ""),
            capture(
                2,
                "2026-07-24T16:00:02Z",
                "editor.exe",
                "Example Page",
                "Item A\nValue: 100",
            ),
        ]);

        assert_eq!(
            blocks[2..],
            [format!("{SECOND_ENTRY_HEADER}\n  Item A\n  Value: 100")]
        );
        assert_eq!(
            changed_lines(&[], &["Item A"]),
            Some(vec!["  @@ -0,0 +1 @@".to_owned(), "  + Item A".to_owned()])
        );
        assert_eq!(
            changed_lines(&["Item A"], &[]),
            Some(vec!["  @@ -1 +0,0 @@".to_owned(), "  - Item A".to_owned()])
        );
    }

    fn page(status: &str, owner: &str) -> String {
        format!("Title\nStatus: {status}\nOwner: {owner}\nline 4\nline 5\nline 6\nline 7\nline 8")
    }

    fn in_full(header: &str, text: &str) -> String {
        std::iter::once(header.to_owned())
            .chain(text.lines().map(|line| format!("  {line}")))
            .collect::<Vec<_>>()
            .join("\n")
    }

    const OWNER_CHANGED_SINCE_FIRST: &str = " (changes since 2026-07-24T16:00:01Z)\n  @@ -2,3 +2,3 @@\n    Status: Done\n  - Owner: A\n  + Owner: B\n    line 4";

    #[test]
    fn another_window_with_the_same_process_and_title_keeps_its_own_base() {
        for (hwnd, pid) in [(2, 10), (1, 11)] {
            let blocks = episode_blocks(&[
                capture(
                    1,
                    "2026-07-24T16:00:01Z",
                    "editor.exe",
                    "Example Page",
                    &page("Done", "A"),
                ),
                with_ids(
                    capture(
                        2,
                        "2026-07-24T16:00:02Z",
                        "editor.exe",
                        "Example Page",
                        &page("Open", "A"),
                    ),
                    Some(hwnd),
                    Some(pid),
                ),
                capture(
                    3,
                    "2026-07-24T16:00:03Z",
                    "editor.exe",
                    "Example Page",
                    &page("Done", "B"),
                ),
            ]);

            assert_eq!(
                blocks[2..],
                [
                    in_full(SECOND_ENTRY_HEADER, &page("Open", "A")),
                    format!(
                        "2026-07-24T16:00:03Z [editor.exe] Example Page{OWNER_CHANGED_SINCE_FIRST}"
                    ),
                ],
                "hwnd {hwnd}, pid {pid}"
            );
        }
    }

    #[test]
    fn a_capture_without_window_ids_neither_uses_nor_becomes_a_base() {
        let blocks = episode_blocks(&[
            capture(
                1,
                "2026-07-24T16:00:01Z",
                "editor.exe",
                "Example Page",
                &page("Done", "A"),
            ),
            with_ids(
                capture(
                    2,
                    "2026-07-24T16:00:02Z",
                    "editor.exe",
                    "Example Page",
                    &page("Open", "A"),
                ),
                None,
                None,
            ),
            with_ids(
                capture(
                    3,
                    "2026-07-24T16:00:03Z",
                    "editor.exe",
                    "Example Page",
                    &page("Open", "B"),
                ),
                None,
                None,
            ),
            capture(
                4,
                "2026-07-24T16:00:04Z",
                "editor.exe",
                "Example Page",
                &page("Done", "B"),
            ),
        ]);

        assert_eq!(
            blocks[2..],
            [
                in_full(SECOND_ENTRY_HEADER, &page("Open", "A")),
                in_full(
                    "2026-07-24T16:00:03Z [editor.exe] Example Page",
                    &page("Open", "B")
                ),
                format!(
                    "2026-07-24T16:00:04Z [editor.exe] Example Page{OWNER_CHANGED_SINCE_FIRST}"
                ),
            ]
        );
    }
}
