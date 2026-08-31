//! Seconds since the epoch, as ACP spells a time.
//!
//! ACP types `SessionInfo::updated_at` as "ISO 8601 timestamp of last
//! activity" — a string — and mentra's store keeps whole seconds since the
//! epoch. One of the two has to convert, and it is this side: a timestamp is a
//! wire format, and basis's own [`PersistedSession`](basis::PersistedSession)
//! should carry the number the store holds rather than a rendering of it.
//!
//! # Why this is arithmetic rather than a dependency
//!
//! Twenty lines against a date-time crate, for one direction of one format
//! with no timezone, no parsing, and no locale. `chrono`, `time` and `jiff` are
//! all already in the lockfile transitively, and a transitive dependency is
//! not an API contract (the rule the manifest states for `uuid`), so taking
//! one means adding it — and its `serde`, its leap-second policy and its
//! timezone database — to a crate that needs none of them.
//!
//! The algorithm is Howard Hinnant's `civil_from_days`, which is exact for
//! every day in the proleptic Gregorian calendar. UTC only, which is what the
//! `Z` says and what a stored epoch second means; leap seconds are outside
//! Unix time and so outside this.

/// The number of days from the epoch to 0000-03-01, the algorithm's zero.
///
/// March is chosen as the year's start so that the leap day lands at the end
/// of the year, where it needs no special case.
const DAYS_TO_MARCH_ZERO: u64 = 719_468;

const SECONDS_PER_DAY: u64 = 86_400;

/// One epoch second, as `YYYY-MM-DDThh:mm:ssZ`.
pub fn rfc3339(seconds: u64) -> String {
    let days = seconds / SECONDS_PER_DAY;
    let time_of_day = seconds % SECONDS_PER_DAY;

    let (year, month, day) = civil_from_days(days);
    let (hour, minute, second) = (
        time_of_day / 3_600,
        (time_of_day % 3_600) / 60,
        time_of_day % 60,
    );

    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// The calendar date `days` after 1970-01-01.
fn civil_from_days(days: u64) -> (u64, u64, u64) {
    let shifted = days + DAYS_TO_MARCH_ZERO;
    // A 400-year era is the shortest span over which the Gregorian calendar
    // repeats exactly: 146097 days, always.
    let era = shifted / 146_097;
    let day_of_era = shifted % 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    // Months run March..February, so this index is 0 for March and 11 for
    // February — which is the whole reason the leap day needs no branch.
    let month_index = (5 * day_of_year + 2) / 153;

    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + u64::from(month <= 2);

    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_epoch_is_the_epoch() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
    }

    #[test]
    fn a_time_of_day_is_carried_whole() {
        assert_eq!(rfc3339(1_000_000_000), "2001-09-09T01:46:40Z");
        assert_eq!(rfc3339(86_399), "1970-01-01T23:59:59Z");
        assert_eq!(rfc3339(86_400), "1970-01-02T00:00:00Z");
    }

    #[test]
    fn a_leap_day_is_a_day() {
        // The case a hand-rolled conversion gets wrong, and the reason the
        // algorithm counts years from March: 2024 is a leap year (divisible by
        // four) and 2000 is one (divisible by 400) where 1900 was not.
        assert_eq!(rfc3339(1_709_164_800), "2024-02-29T00:00:00Z");
        assert_eq!(rfc3339(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(rfc3339(1_709_251_199), "2024-02-29T23:59:59Z");
        assert_eq!(rfc3339(1_709_251_200), "2024-03-01T00:00:00Z");
    }

    #[test]
    fn the_year_2038_is_an_ordinary_tuesday() {
        // Where a signed 32-bit second count stops. mentra's is a `u64` and so
        // is this, so the only thing that happens here is a date.
        assert_eq!(rfc3339(2_147_483_647), "2038-01-19T03:14:07Z");
    }

    #[test]
    fn a_rendered_second_sorts_the_way_the_second_did() {
        // What a client does with the string: ISO 8601 in this shape is
        // lexicographically ordered, which is the property that makes sending
        // a formatted timestamp as useful as sending the number.
        let mut rendered = [1_709_164_800_u64, 0, 2_147_483_647, 1_000_000_000]
            .map(rfc3339)
            .to_vec();
        rendered.sort();

        assert_eq!(
            rendered,
            vec![
                "1970-01-01T00:00:00Z",
                "2001-09-09T01:46:40Z",
                "2024-02-29T00:00:00Z",
                "2038-01-19T03:14:07Z",
            ]
        );
    }
}
