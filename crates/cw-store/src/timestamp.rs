//! How a moment in time is spelled in this database.

use chrono::{DateTime, SecondsFormat, Timelike, Utc};

/// The spelled length of every timestamp this schema stores. Years 0000 through 9999 produce it.
/// A year outside them changes the width either way — 10000 gains a sign and a digit and spells
/// `+10000-…` at 32, while -1 gains only a sign and spells `-0001-…` at 31 — so one width check
/// refuses both.
const SPELLED_LENGTH: usize = 30;

/// One more than the largest value a real nanosecond field can hold. chrono spends everything at or
/// above this on the second that follows.
const NANOSECONDS_PER_SECOND: u32 = 1_000_000_000;

/// Spell `at` for a TEXT column: fixed width, nanosecond precision, `Z`.
///
/// The columns a range query compares are compared as text, so string order has to be time order,
/// and that is a property of this spelling rather than of RFC 3339. Dropping a zero fraction would
/// break it: `Z` is 0x5A and `.` is 0x2E, so `12:34:56Z` would sort after `12:34:56.999999999Z` and
/// a row half a second into a window would fall outside its own lower bound. Nanoseconds are not
/// decoration either — the clock this program reads carries 100 ns granularity, so a coarser column
/// would hand back a different instant than it was given.
///
/// Two values are refused rather than spelled. A year outside 0000 through 9999, because `+` is
/// 0x2B and sorts below every digit, so year 10000 would sort before year 9999 — and the parser
/// rejects that spelling as well, which would leave a row this program wrote and can never read.
/// And a nanosecond field at or above one second, which chrono carries up to 1_999_999_999: it is
/// spelled into the following second, so it either takes text an ordinary instant already owns or,
/// at second 59, becomes a `:60`. That one sorts after every instant of the minute it belongs to
/// and before the minute that follows, so a window ending inside that minute passes over it while
/// a window reaching past it selects text `from_sql` refuses: chrono parses `:60`, and it is the
/// re-spelling check that will not take it back. A width check cannot see either, because the wrong
/// spelling is exactly as wide as the right one.
///
/// Requiring a real nanosecond field is what keeps stored instants on a grid, and that is what lets
/// a half-open `[start, end)` be asked as `[start, end - 1ns]`. `Utc::now()` cannot produce a
/// refused value: it builds from a `Duration` since the epoch.
pub(crate) fn to_sql(at: DateTime<Utc>) -> Result<String, crate::StoreError> {
    let spelled = at.to_rfc3339_opts(SecondsFormat::Nanos, true);
    if at.nanosecond() >= NANOSECONDS_PER_SECOND {
        return Err(crate::StoreError::TimestampOutOfRange {
            at: format!(
                "{spelled} spelled from a nanosecond field of {}",
                at.nanosecond()
            ),
        });
    }
    if spelled.len() != SPELLED_LENGTH {
        return Err(crate::StoreError::TimestampOutOfRange { at: spelled });
    }
    Ok(spelled)
}

/// Read a timestamp back, accepting only the spelling [`to_sql`] writes.
///
/// Parsing is not enough. RFC 3339 lets one instant be written many ways and these columns are
/// compared as text, so a row spelled otherwise decodes correctly and sits in the wrong place in
/// every range query at once. Re-spelling what was parsed and demanding the original back is the
/// whole check, and it cannot fall out of step with [`to_sql`] because it is [`to_sql`].
///
/// It catches such a row only when a query reaches it. A range query compares the stored text
/// before anything is decoded, so a wide window reaches this function and fails loudly while a
/// narrow one drops the row before the decoder and loses it in silence. Nothing but [`to_sql`] may
/// write these columns: this reports a bad row, it does not prevent one.
///
/// Both failures report the text in the column rather than what parsing made of it. An error
/// naming `-0001-12-31T23:59:00.000000000Z` for a row that reads
/// `0000-01-01T00:00:00.000000000+00:01` points at nothing anyone can find, and what has to be
/// repaired is the text.
pub(crate) fn from_sql(
    text: &str,
) -> Result<DateTime<Utc>, Box<dyn std::error::Error + Send + Sync>> {
    let Ok(parsed) = DateTime::parse_from_rfc3339(text) else {
        return Err(Box::new(crate::StoreError::TimestampOutOfRange {
            at: text.to_owned(),
        }));
    };
    let at = parsed.with_timezone(&Utc);
    if to_sql(at).is_ok_and(|spelled| spelled == text) {
        Ok(at)
    } else {
        Err(Box::new(crate::StoreError::TimestampOutOfRange {
            at: text.to_owned(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::{NANOSECONDS_PER_SECOND, SPELLED_LENGTH, from_sql, to_sql};
    use crate::StoreError;
    use chrono::{DateTime, SecondsFormat, TimeDelta, TimeZone, Timelike, Utc};

    fn instants_in_one_second() -> Vec<DateTime<Utc>> {
        let second = DateTime::parse_from_rfc3339("2026-07-25T12:34:56Z")
            .expect("the test timestamp should be valid")
            .with_timezone(&Utc);
        [0, 1, 123_456_789, 500_000_000, 999_999_999]
            .into_iter()
            .map(|nanosecond| {
                second
                    .with_nanosecond(nanosecond)
                    .expect("the test nanosecond should be valid")
            })
            .collect()
    }

    #[test]
    fn string_order_is_time_order() {
        let chronological = instants_in_one_second()
            .into_iter()
            .map(|instant| to_sql(instant).expect("the test timestamp should be spellable"))
            .collect::<Vec<_>>();
        let mut sorted = chronological.clone();
        sorted.sort();

        assert_eq!(sorted, chronological);
    }

    #[test]
    fn the_round_trip_keeps_every_nanosecond() {
        for original in instants_in_one_second() {
            let stored = to_sql(original).expect("the test timestamp should be spellable");
            let restored = from_sql(&stored).expect("the stored timestamp should parse");

            assert_eq!(restored, original);
        }
    }

    #[test]
    fn a_spelling_this_schema_does_not_write_is_refused_even_when_it_parses() {
        let spellings = [
            "2026-07-30T12:00:00.000000000+09:00",
            "2026-07-30T12:00:00.0000+09:00",
            "2026-07-30T12:00:00.0000000001Z",
            "2026-07-30T12:00:00.00000000Z",
            "0000-01-01T00:00:00.000000000+00:01",
            "2026-07-30T12:00:00Z",
        ];

        assert_eq!("2026-07-30T12:00:00.0000+09:00".len(), SPELLED_LENGTH);
        for spelling in spellings {
            assert!(from_sql(spelling).is_err(), "{spelling} should be refused");
        }

        let spelling = "0000-01-01T00:00:00.000000000+00:01";
        let error = from_sql(spelling).expect_err("the non-canonical spelling should be refused");
        let Some(StoreError::TimestampOutOfRange { at }) = error.downcast_ref::<StoreError>()
        else {
            panic!("expected TimestampOutOfRange, got {error:?}")
        };
        assert_eq!(at, spelling);

        let original = Utc
            .with_ymd_and_hms(2026, 7, 30, 12, 0, 0)
            .single()
            .expect("the test timestamp should be valid");
        let stored = to_sql(original).expect("the test timestamp should be spellable");
        let restored = from_sql(&stored).expect("the stored timestamp should parse");

        assert_eq!(restored, original);
    }

    #[test]
    fn a_stored_value_that_is_not_a_timestamp_at_all_names_itself() {
        let error = from_sql("not a timestamp")
            .expect_err("a value that is not a timestamp should be refused");
        let Some(StoreError::TimestampOutOfRange { at }) = error.downcast_ref::<StoreError>()
        else {
            panic!("expected TimestampOutOfRange, got {error:?}")
        };
        assert_eq!(at, "not a timestamp");
    }

    #[test]
    fn every_year_the_spelling_accepts_has_the_same_width_and_round_trips() {
        for year in 0..=9999 {
            let original = Utc
                .with_ymd_and_hms(year, 6, 15, 12, 0, 0)
                .single()
                .expect("the test year should be valid")
                + TimeDelta::nanoseconds(1);
            let stored = to_sql(original).expect("the test timestamp should be spellable");
            let restored = from_sql(&stored).expect("the stored timestamp should parse");

            assert_eq!(stored.len(), SPELLED_LENGTH);
            assert!(stored.ends_with('Z'));
            assert_eq!(restored, original);
        }
    }

    #[test]
    fn a_timestamp_with_no_fixed_width_spelling_is_refused() {
        let timestamps = [
            Utc.with_ymd_and_hms(-1, 1, 1, 0, 0, 0)
                .single()
                .expect("year -1 should be valid"),
            Utc.with_ymd_and_hms(10_000, 1, 1, 0, 0, 0)
                .single()
                .expect("year 10000 should be valid"),
            DateTime::<Utc>::MAX_UTC,
        ];

        for timestamp in timestamps {
            let expected = timestamp.to_rfc3339_opts(SecondsFormat::Nanos, true);
            let error =
                to_sql(timestamp).expect_err("the timestamp without a fixed width should fail");

            match error {
                StoreError::TimestampOutOfRange { at } => assert_eq!(at, expected),
                other => panic!("expected TimestampOutOfRange, got {other:?}"),
            }
        }
    }

    #[test]
    fn an_overflowing_nanosecond_would_name_another_instant_and_is_refused() {
        // The smallest value the guard rejects: written `>` instead of `>=` it would let exactly
        // this one through.
        let boundary = Utc
            .with_ymd_and_hms(2026, 7, 25, 12, 34, 58)
            .single()
            .expect("the test timestamp should be valid")
            .with_nanosecond(NANOSECONDS_PER_SECOND)
            .expect("the boundary nanosecond should be valid");
        let boundary_spelling = boundary.to_rfc3339_opts(SecondsFormat::Nanos, true);

        assert_eq!(boundary_spelling, "2026-07-25T12:34:59.000000000Z");

        let boundary_ordinary = Utc
            .with_ymd_and_hms(2026, 7, 25, 12, 34, 59)
            .single()
            .expect("the test timestamp should be valid")
            .with_nanosecond(0)
            .expect("the ordinary nanosecond should be valid");

        assert_eq!(
            boundary_spelling,
            to_sql(boundary_ordinary).expect("the ordinary instant should be spellable")
        );
        assert_ne!(boundary, boundary_ordinary);

        let boundary_error =
            to_sql(boundary).expect_err("the boundary timestamp should be refused");
        let StoreError::TimestampOutOfRange { at } = boundary_error else {
            panic!("expected TimestampOutOfRange, got {boundary_error:?}")
        };
        assert_eq!(
            at,
            "2026-07-25T12:34:59.000000000Z spelled from a nanosecond field of 1000000000"
        );

        let overflowing = Utc
            .with_ymd_and_hms(2026, 7, 25, 12, 34, 58)
            .single()
            .expect("the test timestamp should be valid")
            .with_nanosecond(1_333_333_333)
            .expect("the overflowing nanosecond should be valid");
        let spelled = overflowing.to_rfc3339_opts(SecondsFormat::Nanos, true);

        assert_eq!(spelled, "2026-07-25T12:34:59.333333333Z");
        assert_eq!(spelled.len(), SPELLED_LENGTH);

        let ordinary = Utc
            .with_ymd_and_hms(2026, 7, 25, 12, 34, 59)
            .single()
            .expect("the test timestamp should be valid")
            .with_nanosecond(333_333_333)
            .expect("the ordinary nanosecond should be valid");
        let ordinary_spelling = ordinary.to_rfc3339_opts(SecondsFormat::Nanos, true);

        assert_eq!(spelled, ordinary_spelling);
        assert_ne!(overflowing, ordinary);

        let error =
            to_sql(overflowing).expect_err("the timestamp that names another instant should fail");
        let StoreError::TimestampOutOfRange { at } = error else {
            panic!("expected TimestampOutOfRange, got {error:?}")
        };
        assert_eq!(
            at, "2026-07-25T12:34:59.333333333Z spelled from a nanosecond field of 1333333333",
            "the bare spelling belongs to an ordinary instant this schema stores, so reporting it \
             alone would name the wrong value — which is the collision this test exists to show"
        );
        let restored = from_sql(&spelled).expect("the ordinary instant's spelling should parse");
        assert_eq!(restored, ordinary);
    }

    #[test]
    fn a_leap_second_inside_a_window_but_lost_by_its_query_is_refused() {
        let leap = Utc
            .with_ymd_and_hms(2016, 12, 31, 23, 59, 59)
            .single()
            .expect("the test timestamp should be valid")
            .with_nanosecond(1_500_000_000)
            .expect("the leap second should be valid");
        let spelled = leap.to_rfc3339_opts(SecondsFormat::Nanos, true);

        assert_eq!(spelled, "2016-12-31T23:59:60.500000000Z");
        assert_eq!(spelled.len(), SPELLED_LENGTH);

        let last_ordinary = Utc
            .with_ymd_and_hms(2016, 12, 31, 23, 59, 59)
            .single()
            .expect("the test timestamp should be valid")
            .with_nanosecond(999_999_999)
            .expect("the last ordinary nanosecond should be valid");
        let next_window_start = Utc
            .with_ymd_and_hms(2017, 1, 1, 0, 0, 0)
            .single()
            .expect("the next window start should be valid");
        let last_ordinary_spelling =
            to_sql(last_ordinary).expect("the last ordinary instant should be spellable");
        let next_window_spelling =
            to_sql(next_window_start).expect("the next window start should be spellable");

        assert!(last_ordinary_spelling < spelled);
        assert!(spelled < next_window_spelling);
        assert!(leap > last_ordinary);
        assert!(leap < next_window_start);

        let error = to_sql(leap).expect_err("the leap second should be refused");
        let StoreError::TimestampOutOfRange { at } = error else {
            panic!("expected TimestampOutOfRange, got {error:?}")
        };
        assert_eq!(
            at, "2016-12-31T23:59:60.500000000Z spelled from a nanosecond field of 1500000000",
            "no storable instant owns this spelling, and the half-open contract still places it \
             inside a window the query then misses at both ends — the opposite failure from the \
             mid-minute collision, and both refusals report the field they were spelled from"
        );
        let error = from_sql(&spelled).expect_err("the leap second spelling should be refused");
        let Some(StoreError::TimestampOutOfRange { at }) = error.downcast_ref::<StoreError>()
        else {
            panic!("expected TimestampOutOfRange, got {error:?}")
        };
        assert_eq!(at, "2016-12-31T23:59:60.500000000Z");
    }
}
