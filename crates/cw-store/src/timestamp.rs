//! How a moment in time is spelled in this database.

use chrono::{DateTime, SecondsFormat, Timelike, Utc};

/// The spelled length of every timestamp this schema stores. Years 0000 through 9999 produce it;
/// anything outside them gains a sign and a digit (measured 2026-07-28) and is refused.
const SPELLED_LENGTH: usize = 30;

/// One more than the largest value a real nanosecond field can hold. chrono spends everything at or
/// above this on the second that follows, so two different instants would share one spelling.
const NANOSECONDS_PER_SECOND: u32 = 1_000_000_000;

/// Spell `at` for a TEXT column.
///
/// Fixed width, nanosecond precision, `Z`. Every timestamp column in this schema is compared with
/// `<` and `>=` in SQL, which compares the stored text, so string order has to be time order — and
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
/// which no window can contain, since it falls after the last instant of one window and before the
/// first of the next. Requiring a real nanosecond field makes the spelling name exactly one instant
/// and keeps the stored instants on the grid, which is what lets `find_in_window` state
/// `[start, end)` as `[start, end - 1ns]` at all. Nothing real is lost: `Utc::now()` builds from a
/// `Duration` since the epoch, whose subsecond part is below one second by construction.
///
/// Every task that stores a timestamp uses this, so the decision is taken once.
pub(crate) fn to_sql(at: DateTime<Utc>) -> Result<String, crate::StoreError> {
    let spelled = at.to_rfc3339_opts(SecondsFormat::Nanos, true);
    if spelled.len() == SPELLED_LENGTH && at.nanosecond() < NANOSECONDS_PER_SECOND {
        Ok(spelled)
    } else {
        Err(crate::StoreError::TimestampOutOfRange { at: spelled })
    }
}

/// Read a timestamp back, refusing one this schema does not represent.
///
/// Any RFC 3339 spelling parses, not only the one [`to_sql`] writes, but an overflowing nanosecond
/// field is rejected here too. A row carrying one would decode without complaint and then be missing
/// from every window that contains it; a row this program did not write is meant to fail loudly, not
/// to disappear from a query.
pub(crate) fn from_sql(
    text: &str,
) -> Result<DateTime<Utc>, Box<dyn std::error::Error + Send + Sync>> {
    let at = DateTime::parse_from_rfc3339(text)?.with_timezone(&Utc);
    if at.nanosecond() >= NANOSECONDS_PER_SECOND {
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
        assert!(matches!(error, StoreError::TimestampOutOfRange { .. }));
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
        assert!(matches!(error, StoreError::TimestampOutOfRange { .. }));
        let error = from_sql(&spelled).expect_err("the leap second spelling should be refused");
        assert!(matches!(
            error.downcast_ref::<StoreError>(),
            Some(StoreError::TimestampOutOfRange { .. })
        ));
    }
}
