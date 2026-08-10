//! How long the scheduler waits between iterations.
//!
//! Parsing is deliberately strict. An interval is the one number that decides
//! how often a watch spends money, so a typo must be a refusal rather than a
//! silent fallback: `30` could mean seconds or minutes, `30M` could mean
//! minutes or months, and guessing either would be a bill the operator did not
//! agree to.

use std::{fmt, str::FromStr, time::Duration};

use thiserror::Error;

/// What the accepted forms are, quoted verbatim in every parse error so the
/// message alone is enough to fix the input.
const ACCEPTED: &str = "a positive number followed by s, m, h, or d — e.g. 90s, 30m, 2h, 1d";

/// Milliseconds is the wire unit, so an interval that cannot be expressed in
/// `u64` milliseconds is rejected at the boundary rather than truncated later.
const MAX_SECONDS: u64 = u64::MAX / 1_000;

/// A wait between iterations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Interval(Duration);

impl Interval {
    /// An interval of exactly `duration`.
    ///
    /// Unlike parsing, this does not reject zero: a caller writing a
    /// `Duration` in code has said what it means, and a zero-length wait is
    /// how a test drives the loop without waiting. Text comes from a person
    /// who may have typed it by accident, which is why [`FromStr`] is stricter.
    pub const fn from_duration(duration: Duration) -> Self {
        Self(duration)
    }

    pub const fn duration(self) -> Duration {
        self.0
    }

    /// The interval in milliseconds, which is what the event stream carries.
    pub fn as_millis(self) -> u64 {
        // Saturating, not truncating: parsing rejects anything this cannot
        // hold, but `from_duration` accepts any `Duration`, and a truncated
        // interval on the wire would read as a much shorter one.
        u64::try_from(self.0.as_millis()).unwrap_or(u64::MAX)
    }
}

impl fmt::Display for Interval {
    /// Renders in the largest unit that divides evenly, so an interval read
    /// back from a config looks like the one that was typed.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let seconds = self.0.as_secs();

        for (unit, per) in [("d", 86_400), ("h", 3_600), ("m", 60)] {
            if seconds >= per && seconds.is_multiple_of(per) {
                return write!(f, "{}{unit}", seconds / per);
            }
        }

        write!(f, "{seconds}s")
    }
}

impl FromStr for Interval {
    type Err = IntervalError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let input = text.trim();
        if input.is_empty() {
            return Err(IntervalError::Empty);
        }

        // Splitting on the first non-digit rather than on the last character
        // keeps `30min` and `30 m` as errors that name what is wrong, instead
        // of silently reading them as 30 of something.
        let boundary = input
            .find(|character: char| !character.is_ascii_digit())
            .unwrap_or(input.len());
        let (count, unit) = input.split_at(boundary);

        if count.is_empty() {
            return Err(IntervalError::NotANumber {
                input: input.to_string(),
            });
        }
        if unit.is_empty() {
            return Err(IntervalError::MissingUnit {
                input: input.to_string(),
            });
        }

        let per_second = match unit {
            "s" => 1,
            "m" => 60,
            "h" => 3_600,
            "d" => 86_400,
            _ => {
                return Err(IntervalError::UnknownUnit {
                    unit: unit.to_string(),
                });
            }
        };

        // Only overflow can fail here: every character is already a digit.
        let count: u64 = count.parse().map_err(|_| IntervalError::TooLarge {
            input: input.to_string(),
        })?;
        if count == 0 {
            return Err(IntervalError::Zero);
        }

        let seconds = count
            .checked_mul(per_second)
            .filter(|seconds| *seconds <= MAX_SECONDS)
            .ok_or_else(|| IntervalError::TooLarge {
                input: input.to_string(),
            })?;

        Ok(Self(Duration::from_secs(seconds)))
    }
}

/// Why an interval could not be read. Each variant names the input and the
/// accepted forms, because the person seeing it is fixing a command line.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum IntervalError {
    #[error("an interval is required: {ACCEPTED}")]
    Empty,

    #[error("`{input}` has no unit: {ACCEPTED}")]
    MissingUnit { input: String },

    #[error("`{unit}` is not a unit: {ACCEPTED}")]
    UnknownUnit { unit: String },

    #[error("`{input}` does not start with a number: {ACCEPTED}")]
    NotANumber { input: String },

    #[error("an interval of zero would busy-loop rather than schedule: {ACCEPTED}")]
    Zero,

    #[error("`{input}` is longer than a schedule can represent: {ACCEPTED}")]
    TooLarge { input: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<Interval, IntervalError> {
        text.parse()
    }

    #[test]
    fn every_unit_is_understood() {
        assert_eq!(
            parse("90s").expect("seconds"),
            Interval::from_duration(Duration::from_secs(90))
        );
        assert_eq!(
            parse("30m").expect("minutes"),
            Interval::from_duration(Duration::from_secs(1_800))
        );
        assert_eq!(
            parse("2h").expect("hours"),
            Interval::from_duration(Duration::from_secs(7_200))
        );
        assert_eq!(
            parse("1d").expect("days"),
            Interval::from_duration(Duration::from_secs(86_400))
        );
    }

    #[test]
    fn surrounding_whitespace_is_not_an_error() {
        assert_eq!(parse("  30m \n").expect("trimmed"), parse("30m").unwrap());
    }

    #[test]
    fn a_bare_number_is_refused_rather_than_guessed() {
        // The whole point: `--every 30` must not quietly become 30 of
        // whichever unit the implementer happened to prefer.
        assert_eq!(
            parse("30"),
            Err(IntervalError::MissingUnit {
                input: "30".to_string()
            })
        );
    }

    #[test]
    fn unknown_units_name_themselves() {
        assert_eq!(
            parse("30x"),
            Err(IntervalError::UnknownUnit {
                unit: "x".to_string()
            })
        );
        // Uppercase is refused too: `M` means minutes in some tools and months
        // in others, and a scheduler must not pick one silently.
        assert_eq!(
            parse("30M"),
            Err(IntervalError::UnknownUnit {
                unit: "M".to_string()
            })
        );
        // Long forms would be a guess about which unit was meant.
        assert_eq!(
            parse("30min"),
            Err(IntervalError::UnknownUnit {
                unit: "min".to_string()
            })
        );
    }

    #[test]
    fn internal_whitespace_is_refused() {
        assert_eq!(
            parse("30 m"),
            Err(IntervalError::UnknownUnit {
                unit: " m".to_string()
            })
        );
    }

    #[test]
    fn nonsense_is_refused() {
        assert_eq!(parse(""), Err(IntervalError::Empty));
        assert_eq!(parse("   "), Err(IntervalError::Empty));
        assert_eq!(
            parse("soon"),
            Err(IntervalError::NotANumber {
                input: "soon".to_string()
            })
        );
        assert_eq!(
            parse("-5m"),
            Err(IntervalError::NotANumber {
                input: "-5m".to_string()
            })
        );
        assert_eq!(
            parse("1.5h"),
            Err(IntervalError::UnknownUnit {
                unit: ".5h".to_string()
            })
        );
    }

    #[test]
    fn zero_is_refused_because_it_is_not_a_schedule() {
        assert_eq!(parse("0s"), Err(IntervalError::Zero));
        assert_eq!(parse("0d"), Err(IntervalError::Zero));
    }

    #[test]
    fn an_interval_too_large_to_carry_is_refused() {
        assert!(matches!(
            parse("99999999999999999999d"),
            Err(IntervalError::TooLarge { .. })
        ));
        assert!(matches!(
            parse("999999999999999999d"),
            Err(IntervalError::TooLarge { .. })
        ));
    }

    #[test]
    fn errors_say_how_to_fix_the_input() {
        for text in ["", "30", "30x", "soon", "0s"] {
            let message = parse(text).expect_err("refused").to_string();
            assert!(
                message.contains("30m"),
                "`{text}` must show an accepted form, got: {message}"
            );
        }
    }

    #[test]
    fn display_round_trips_through_parsing() {
        for text in ["45s", "30m", "2h", "1d", "90s"] {
            let interval = parse(text).expect("parses");
            let printed = interval.to_string();
            assert_eq!(
                parse(&printed).expect("re-parses"),
                interval,
                "`{text}` printed as `{printed}`"
            );
        }
    }

    #[test]
    fn display_uses_the_largest_whole_unit() {
        assert_eq!(parse("90s").unwrap().to_string(), "90s");
        assert_eq!(parse("120s").unwrap().to_string(), "2m");
        assert_eq!(parse("60m").unwrap().to_string(), "1h");
        assert_eq!(parse("24h").unwrap().to_string(), "1d");
    }

    #[test]
    fn milliseconds_are_what_the_stream_carries() {
        assert_eq!(parse("30m").unwrap().as_millis(), 1_800_000);
        assert_eq!(parse("90s").unwrap().as_millis(), 90_000);
    }

    #[test]
    fn an_unparseably_long_duration_saturates_rather_than_wrapping() {
        // Unreachable through parsing, which is why it is worth pinning: a
        // truncating cast here would turn a century into a few seconds.
        let absurd = Interval::from_duration(Duration::from_secs(u64::MAX));

        assert_eq!(absurd.as_millis(), u64::MAX);
    }
}
