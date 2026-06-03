//! Human-friendly durations: `"30s"`, `"5m"`, `"2h"`, or bare seconds `"45"`.

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// A [`Duration`] written as `"30s"` / `"5m"` / `"2h"` (or bare seconds `"45"`).
///
/// Parse errors surface at deserialize time with a precise, actionable message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HumanDuration(pub Duration);

impl HumanDuration {
    /// Parses a human duration string, returning a human-readable error on failure.
    ///
    /// # Errors
    /// Returns `Err` if there is no leading number, the unit is not `s`/`m`/`h`/`d`/`w`, or the
    /// value overflows `u64` seconds.
    pub fn parse(s: &str) -> Result<Self, String> {
        let s = s.trim();
        let split = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
        let (num, unit) = s.split_at(split);
        let n: u64 = num
            .parse()
            .map_err(|_| format!("invalid duration {s:?}: expected a leading number"))?;
        // Checked arithmetic: a syntactically valid but huge value (`9999999999999999h`)
        // must error, not panic in debug or silently wrap in release.
        let secs = match unit.trim() {
            "" | "s" => Some(n),
            "m" => n.checked_mul(60),
            "h" => n.checked_mul(3600),
            "d" => n.checked_mul(86_400),
            "w" => n.checked_mul(604_800),
            other => {
                return Err(format!(
                    "invalid duration unit {other:?} in {s:?} (use s, m, h, d, or w)"
                ));
            }
        }
        .ok_or_else(|| format!("duration {s:?} is too large"))?;
        Ok(Self(Duration::from_secs(secs)))
    }

    /// The wrapped [`Duration`].
    #[must_use]
    pub fn as_duration(self) -> Duration {
        self.0
    }
}

impl Serialize for HumanDuration {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&format!("{}s", self.0.as_secs()))
    }
}

impl<'de> Deserialize<'de> for HumanDuration {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        use serde::de::Error as _;
        let s = String::deserialize(d)?;
        Self::parse(&s).map_err(D::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::HumanDuration;
    use std::time::Duration;

    #[test]
    fn parses_units() {
        assert_eq!(
            HumanDuration::parse("30s").unwrap().0,
            Duration::from_secs(30)
        );
        assert_eq!(
            HumanDuration::parse("5m").unwrap().0,
            Duration::from_secs(300)
        );
        assert_eq!(
            HumanDuration::parse("2h").unwrap().0,
            Duration::from_secs(7200)
        );
        assert_eq!(
            HumanDuration::parse("45").unwrap().0,
            Duration::from_secs(45)
        );
        assert_eq!(
            HumanDuration::parse("  10m ").unwrap().0,
            Duration::from_secs(600)
        );
        assert_eq!(
            HumanDuration::parse("10d").unwrap().0,
            Duration::from_secs(864_000)
        );
        assert_eq!(
            HumanDuration::parse("2w").unwrap().0,
            Duration::from_secs(1_209_600)
        );
    }

    #[test]
    fn rejects_bad_input() {
        assert!(HumanDuration::parse("abc").is_err());
        assert!(
            HumanDuration::parse("10y").is_err(),
            "years are not a supported unit"
        );
        assert!(HumanDuration::parse("").is_err());
    }

    #[test]
    fn rejects_overflow_instead_of_panicking() {
        // These are syntactically valid but overflow u64 seconds when multiplied.
        assert!(HumanDuration::parse("9999999999999999h").is_err());
        assert!(HumanDuration::parse("18446744073709551615h").is_err());
        assert!(HumanDuration::parse("400000000000000000m").is_err());
        // Bare-seconds at u64::MAX is fine (no multiply).
        assert!(HumanDuration::parse("18446744073709551615").is_ok());
    }

    #[test]
    fn round_trips_through_serde() {
        let d = HumanDuration::parse("5m").unwrap();
        let s = serde_json::to_string(&d).unwrap();
        assert_eq!(s, r#""300s""#);
        let back: HumanDuration = serde_json::from_str(&s).unwrap();
        assert_eq!(back, d);
    }
}
