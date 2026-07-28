//! How a moment in time is spelled in this database.

use chrono::{DateTime, SecondsFormat, Utc};

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
/// Every task that stores a timestamp uses this, so the decision is taken once.
pub(crate) fn to_sql(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Nanos, true)
}

/// Read a timestamp back. Any RFC 3339 spelling parses, not only the one [`to_sql`] writes.
pub(crate) fn from_sql(text: &str) -> Result<DateTime<Utc>, chrono::ParseError> {
    Ok(DateTime::parse_from_rfc3339(text)?.with_timezone(&Utc))
}

#[cfg(test)]
mod tests {
    use super::{from_sql, to_sql};
    use chrono::{DateTime, Timelike, Utc};

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
            .map(to_sql)
            .collect::<Vec<_>>();
        let mut sorted = chronological.clone();
        sorted.sort();

        assert_eq!(sorted, chronological);
    }

    #[test]
    fn the_round_trip_keeps_every_nanosecond() {
        for original in instants_in_one_second() {
            let stored = to_sql(original);
            let restored = from_sql(&stored).expect("the stored timestamp should parse");

            assert_eq!(restored, original);
        }
    }
}
