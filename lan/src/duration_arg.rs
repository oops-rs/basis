//! How a duration is spelled on the command line.
//!
//! Parsing is deliberately strict. These durations bound what a run may spend,
//! so a typo must be a refusal rather than a silent fallback: `30` could mean
//! seconds or minutes, `30M` could mean minutes or months, and guessing either
//! would be a bill nobody agreed to.
//!
//! The library takes a plain [`Duration`]. This lives in the binary because it
//! is a spelling convention, not a harness concern — a Rust host writing
//! `Duration::from_secs(600)` has already said what it means.

use std::{fmt, str::FromStr, time::Duration};

use thiserror::Error;

/// What the accepted forms are, quoted verbatim in every parse error so the
/// message alone is enough to fix the input.
const ACCEPTED: &str = "a positive number followed by s, m, h, or d — e.g. 90s, 30m, 2h, 1d";

/// About 136 years: long past any bound anyone means, and far enough from the
/// end of `SystemTime`'s range that "now plus this" cannot overflow when the
/// deadline is turned into an instant.
const MAX_SECONDS: u64 = u32::MAX as u64;

/// A duration as typed on the command line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct DurationArg(Duration);

impl DurationArg {
    pub(crate) const fn duration(self) -> Duration {
        self.0
    }
}

impl fmt::Display for DurationArg {
    /// Renders in the largest unit that divides evenly, so a duration read back
    /// looks like the one that was typed.
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

impl FromStr for DurationArg {
    type Err = DurationArgError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let input = text.trim();
        if input.is_empty() {
            return Err(DurationArgError::Empty);
        }

        // Splitting on the first non-digit rather than on the last character
        // keeps `30min` and `30 m` as errors that name what is wrong, instead
        // of silently reading them as 30 of something.
        let boundary = input
            .find(|character: char| !character.is_ascii_digit())
            .unwrap_or(input.len());
        let (count, unit) = input.split_at(boundary);

        if count.is_empty() {
            return Err(DurationArgError::NotANumber {
                input: input.to_string(),
            });
        }
        if unit.is_empty() {
            return Err(DurationArgError::MissingUnit {
                input: input.to_string(),
            });
        }

        let per_second = match unit {
            "s" => 1,
            "m" => 60,
            "h" => 3_600,
            "d" => 86_400,
            _ => {
                return Err(DurationArgError::UnknownUnit {
                    unit: unit.to_string(),
                });
            }
        };

        // Only overflow can fail here: every character is already a digit.
        let count: u64 = count.parse().map_err(|_| DurationArgError::TooLarge {
            input: input.to_string(),
        })?;
        if count == 0 {
            return Err(DurationArgError::Zero);
        }

        let seconds = count
            .checked_mul(per_second)
            .filter(|seconds| *seconds <= MAX_SECONDS)
            .ok_or_else(|| DurationArgError::TooLarge {
                input: input.to_string(),
            })?;

        Ok(Self(Duration::from_secs(seconds)))
    }
}

/// Why a duration could not be read. Each variant names the input and the
/// accepted forms, because the person seeing it is fixing a command line.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub(crate) enum DurationArgError {
    #[error("a duration is required: {ACCEPTED}")]
    Empty,

    #[error("`{input}` has no unit: {ACCEPTED}")]
    MissingUnit { input: String },

    #[error("`{unit}` is not a unit: {ACCEPTED}")]
    UnknownUnit { unit: String },

    #[error("`{input}` does not start with a number: {ACCEPTED}")]
    NotANumber { input: String },

    #[error("a bound of zero would trip before any work happened: {ACCEPTED}")]
    Zero,

    #[error("`{input}` is longer than a bound can represent: {ACCEPTED}")]
    TooLarge { input: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<DurationArg, DurationArgError> {
        text.parse()
    }

    fn secs(seconds: u64) -> DurationArg {
        DurationArg(Duration::from_secs(seconds))
    }

    #[test]
    fn every_unit_is_understood() {
        assert_eq!(parse("90s").expect("seconds"), secs(90));
        assert_eq!(parse("30m").expect("minutes"), secs(1_800));
        assert_eq!(parse("2h").expect("hours"), secs(7_200));
        assert_eq!(parse("1d").expect("days"), secs(86_400));
    }

    #[test]
    fn surrounding_whitespace_is_not_an_error() {
        assert_eq!(parse("  30m \n").expect("trimmed"), parse("30m").unwrap());
    }

    #[test]
    fn a_bare_number_is_refused_rather_than_guessed() {
        // The whole point: `--deadline 30` must not quietly become 30 of
        // whichever unit the implementer happened to prefer.
        assert_eq!(
            parse("30"),
            Err(DurationArgError::MissingUnit {
                input: "30".to_string()
            })
        );
    }

    #[test]
    fn unknown_units_name_themselves() {
        assert_eq!(
            parse("30x"),
            Err(DurationArgError::UnknownUnit {
                unit: "x".to_string()
            })
        );
        // Uppercase is refused too: `M` means minutes in some tools and months
        // in others, and a bound must not pick one silently.
        assert_eq!(
            parse("30M"),
            Err(DurationArgError::UnknownUnit {
                unit: "M".to_string()
            })
        );
        // Long forms would be a guess about which unit was meant.
        assert_eq!(
            parse("30min"),
            Err(DurationArgError::UnknownUnit {
                unit: "min".to_string()
            })
        );
    }

    #[test]
    fn internal_whitespace_is_refused() {
        assert_eq!(
            parse("30 m"),
            Err(DurationArgError::UnknownUnit {
                unit: " m".to_string()
            })
        );
    }

    #[test]
    fn nonsense_is_refused() {
        assert_eq!(parse(""), Err(DurationArgError::Empty));
        assert_eq!(parse("   "), Err(DurationArgError::Empty));
        assert_eq!(
            parse("soon"),
            Err(DurationArgError::NotANumber {
                input: "soon".to_string()
            })
        );
        assert_eq!(
            parse("-5m"),
            Err(DurationArgError::NotANumber {
                input: "-5m".to_string()
            })
        );
        assert_eq!(
            parse("1.5h"),
            Err(DurationArgError::UnknownUnit {
                unit: ".5h".to_string()
            })
        );
    }

    #[test]
    fn zero_is_refused_because_it_is_not_a_bound() {
        assert_eq!(parse("0s"), Err(DurationArgError::Zero));
        assert_eq!(parse("0d"), Err(DurationArgError::Zero));
    }

    #[test]
    fn a_duration_too_large_to_carry_is_refused() {
        // Absurd on its face, and worth refusing rather than truncating: a
        // deadline is turned into "now plus this", which panics on overflow.
        assert!(matches!(
            parse("99999999999999999999d"),
            Err(DurationArgError::TooLarge { .. })
        ));
        assert!(matches!(
            parse("999999999999999999d"),
            Err(DurationArgError::TooLarge { .. })
        ));
        assert!(matches!(
            parse("100000d"),
            Err(DurationArgError::TooLarge { .. })
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
            let parsed = parse(text).expect("parses");
            let printed = parsed.to_string();
            assert_eq!(
                parse(&printed).expect("re-parses"),
                parsed,
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
    fn what_is_parsed_is_what_reaches_the_run() {
        assert_eq!(parse("30m").unwrap().duration(), Duration::from_secs(1_800));
        assert_eq!(parse("90s").unwrap().duration(), Duration::from_secs(90));
    }
}
