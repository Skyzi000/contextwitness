//! How a moment in time is spelled in this database.

use chrono::{DateTime, SecondsFormat, Timelike, Utc};

/// The spelled length of every timestamp this schema stores. Years 0000 through 9999 produce it.
/// Measured 2026-07-28, a year above gains a sign and a digit — 10000 spells `+10000-…` at 32
/// characters — and a year below gains only a sign, so -1 spells `-0001-…` at 31. Both are refused;
/// only the first is refused for the reason the length suggests.
const SPELLED_LENGTH: usize = 30;

/// One more than the largest value a real nanosecond field can hold. chrono spends everything at or
/// above this on the second that follows, which goes wrong two different ways: mid-minute the value
/// takes text an ordinary instant already owns, and at second 59 it takes a `:60` that no instant
/// this schema can store owns at all.
const NANOSECONDS_PER_SECOND: u32 = 1_000_000_000;

/// Spell `at` for a TEXT column.
///
/// Fixed width, nanosecond precision, `Z`. Every timestamp column in this schema is compared with
/// `>=` and `<=` in SQL, which compares the stored text, so string order has to be time order — and
/// that is a property of the spelling, not of RFC 3339. With the fraction dropped when it is zero,
/// `2026-07-25T12:34:56Z` sorts AFTER `2026-07-25T12:34:56.999999999Z`, because `Z` is 0x5A and `.`
/// is 0x2E (measured 2026-07-28), so a row half a second into a window falls outside its own lower
/// bound. Nanoseconds are not decoration either: the clock this program reads carries 100 ns
/// granularity, so a coarser column would hand back a different instant than it was given.
///
/// A value outside years 0000 through 9999 both stops sorting and stops parsing. `+` is 0x2B, below
/// every digit, so year 10000 sorts before year 9999, and the parser rejects the spelling, which
/// would leave a row this program wrote and can never read.
///
/// The nanosecond condition is not about the year: chrono's nanosecond field reaches
/// 1_999_999_999, and anything at or above a billion is spelled as part of the following second, so
/// `12:34:58` carrying 1.333 seconds is written as `12:34:59.333333333Z` — the same text an
/// ordinary, different instant already owns. The width check cannot see that, because the wrong
/// spelling is exactly as wide as the right one. At second 59 the same overflow spells as `:60`,
/// which no storable instant owns — and which the half-open contract still places inside a window
/// (measured 2026-07-30, `start <= leap` and `leap < end` are both true) while the closed-form
/// query misses it at both ends, its text sorting after `23:59:59.999999999Z` and before
/// `2017-01-01T00:00:00.000000000Z`. That is the shape of the danger throughout: not a value
/// outside the range, but one the contract promises to return that the query cannot find.
/// Requiring a real nanosecond field makes the spelling name exactly one instant
/// and keeps the stored instants on the grid, which is what lets `find_in_window` state
/// `[start, end)` as `[start, end - 1ns]` at all, in `observations::find_in_window` and
/// `control::events_in_window` alike.
/// Nothing real is lost: `Utc::now()` builds from a `Duration` since the epoch, whose subsecond part
/// is below one second by construction.
///
/// Every task that stores a timestamp uses this, so the decision is taken once.
pub(crate) fn to_sql(at: DateTime<Utc>) -> Result<String, crate::StoreError> {
    let spelled = at.to_rfc3339_opts(SecondsFormat::Nanos, true);
    if at.nanosecond() >= NANOSECONDS_PER_SECOND {
        // The spelling cannot stand for the value here, and it fails two different ways. Mid-minute
        // it is text an ordinary stored instant already owns, so reporting it alone names the wrong
        // value; at second 59 it is a `:60` no storable instant owns, which the half-open contract
        // still places inside a window while the query misses it at both ends. Naming the field it
        // came from is the only form that fits both.
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
/// Parsing is not enough. RFC 3339 lets the same instant be written many ways, and these columns
/// are compared as TEXT, so a row spelled any other way is decodable and in the wrong place in
/// every range query at once: `…+09:00` does not sort against the `Z` forms, a tenth fractional
/// digit sorts before the `Z` that should follow the ninth, and an offset can carry a year outside
/// the range across the boundary into one `to_sql` refuses to write. Re-spelling what was parsed
/// and demanding the original back is the whole check, and it cannot fall out of step with
/// [`to_sql`] because it is [`to_sql`]. That makes a row this program did not write fail loudly the
/// moment it is decoded — but only then. A range query compares the stored TEXT before anything is
/// decoded, so a row spelled some other way is not refused there, it is simply not selected, and
/// this function never sees it. Nothing but [`to_sql`] may write one of these columns; the decoder
/// catches a bad row, it does not prevent one.
pub(crate) fn from_sql(
    text: &str,
) -> Result<DateTime<Utc>, Box<dyn std::error::Error + Send + Sync>> {
    let at = DateTime::parse_from_rfc3339(text)?.with_timezone(&Utc);
    if to_sql(at)? != text {
        return Err(Box::new(crate::StoreError::TimestampOutOfRange {
            at: text.to_owned(),
        }));
    }
    Ok(at)
}

#[cfg(test)]
mod tests {
    use super::{SPELLED_LENGTH, from_sql, to_sql};
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
            // This ordinary instant would not sort against the `Z` spellings.
            "2026-07-30T12:00:00.000000000+09:00",
            // Exactly thirty characters, and nine hours from where its text sorts: a length check
            // cannot tell this from the spelling this schema writes. Measured 2026-07-30, it parses to
            // 03:00:00 UTC and its text sorts after 11:59:59.999999999Z.
            "2026-07-30T12:00:00.0000+09:00",
            // chrono would drop this tenth digit, but SQLite would keep comparing it.
            "2026-07-30T12:00:00.0000000001Z",
            // This eight-digit fraction would leave the stored text one character short.
            "2026-07-30T12:00:00.00000000Z",
            // This offset would carry the UTC instant into year -1.
            "0000-01-01T00:00:00.000000000+00:01",
            // This form would omit the fractional part entirely.
            "2026-07-30T12:00:00Z",
        ];

        assert_eq!("2026-07-30T12:00:00.0000+09:00".len(), SPELLED_LENGTH);
        for spelling in spellings {
            assert!(from_sql(spelling).is_err(), "{spelling} should be refused");
        }

        let original = Utc
            .with_ymd_and_hms(2026, 7, 30, 12, 0, 0)
            .single()
            .expect("the test timestamp should be valid");
        let stored = to_sql(original).expect("the test timestamp should be spellable");
        let restored = from_sql(&stored).expect("the stored timestamp should parse");

        assert_eq!(restored, original);
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

        // The width and collision above are why refusing the overflowing value is not optional.
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
    fn a_leap_second_belongs_to_no_window_and_is_refused() {
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
        assert!(matches!(
            error.downcast_ref::<StoreError>(),
            Some(StoreError::TimestampOutOfRange { .. })
        ));
    }
}
