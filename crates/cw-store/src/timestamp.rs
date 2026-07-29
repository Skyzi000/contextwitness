//! How a moment in time is spelled in this database.

use chrono::{DateTime, SecondsFormat, Utc};

/// The spelled length of every timestamp this schema stores. Years 0000 through 9999 produce it;
/// anything outside them gains a sign and a digit (measured 2026-07-28) and is refused.
const SPELLED_LENGTH: usize = 30;

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
/// Every task that stores a timestamp uses this, so the decision is taken once.
pub(crate) fn to_sql(at: DateTime<Utc>) -> Result<String, crate::StoreError> {
    let spelled = at.to_rfc3339_opts(SecondsFormat::Nanos, true);
    if spelled.len() == SPELLED_LENGTH {
        Ok(spelled)
    } else {
        Err(crate::StoreError::TimestampOutOfRange { at: spelled })
    }
}

/// Read a timestamp back. Any RFC 3339 spelling parses, not only the one [`to_sql`] writes.
pub(crate) fn from_sql(text: &str) -> Result<DateTime<Utc>, chrono::ParseError> {
    Ok(DateTime::parse_from_rfc3339(text)?.with_timezone(&Utc))
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
}
