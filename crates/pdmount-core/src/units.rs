//! Human-readable durations ("2s", "10m") and byte sizes ("10GiB") used in the
//! configuration file.

use std::fmt;
use std::str::FromStr;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid {kind} {value:?}: {reason}")]
pub struct ParseUnitError {
    kind: &'static str,
    value: String,
    reason: &'static str,
}

fn split_number(value: &str) -> Option<(u64, &str)> {
    let value = value.trim();
    let digits = value
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(value.len());
    let number = value[..digits].parse().ok()?;
    Some((number, value[digits..].trim()))
}

/// A duration written as an integer and a unit: `ms`, `s`, `m` or `h`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HumanDuration(pub Duration);

impl FromStr for HumanDuration {
    type Err = ParseUnitError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let error = |reason| ParseUnitError {
            kind: "duration",
            value: value.to_owned(),
            reason,
        };
        let (number, unit) = split_number(value).ok_or_else(|| error("expected a number"))?;
        let seconds = |factor: u64| {
            number
                .checked_mul(factor)
                .map(Duration::from_secs)
                .ok_or_else(|| error("too large"))
        };
        let duration = match unit {
            "ms" => Duration::from_millis(number),
            "s" => seconds(1)?,
            "m" => seconds(60)?,
            "h" => seconds(3600)?,
            _ => return Err(error("expected unit ms, s, m or h")),
        };
        Ok(Self(duration))
    }
}

impl fmt::Display for HumanDuration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let ms = self.0.as_millis();
        match ms {
            _ if ms.is_multiple_of(3_600_000) && ms > 0 => write!(f, "{}h", ms / 3_600_000),
            _ if ms.is_multiple_of(60_000) && ms > 0 => write!(f, "{}m", ms / 60_000),
            _ if ms.is_multiple_of(1000) => write!(f, "{}s", ms / 1000),
            _ => write!(f, "{ms}ms"),
        }
    }
}

/// A byte size written as an integer and a unit: `B`, `KiB`, `MiB`, `GiB`
/// or `TiB` (binary multiples).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ByteSize(pub u64);

impl FromStr for ByteSize {
    type Err = ParseUnitError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let error = |reason| ParseUnitError {
            kind: "size",
            value: value.to_owned(),
            reason,
        };
        let (number, unit) = split_number(value).ok_or_else(|| error("expected a number"))?;
        let shift = match unit {
            "" | "B" => 0,
            "KiB" => 10,
            "MiB" => 20,
            "GiB" => 30,
            "TiB" => 40,
            _ => return Err(error("expected unit B, KiB, MiB, GiB or TiB")),
        };
        number
            .checked_mul(1u64 << shift)
            .map(Self)
            .ok_or_else(|| error("too large"))
    }
}

const SIZE_UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];

impl ByteSize {
    /// Approximate size with the largest binary unit and one decimal place,
    /// for display to people (`1.5 GiB`).
    pub fn human(&self) -> String {
        let mut value = self.0 as f64;
        let mut unit = 0;
        while value >= 1024.0 && unit < SIZE_UNITS.len() - 1 {
            value /= 1024.0;
            unit += 1;
        }
        if unit == 0 {
            format!("{} B", self.0)
        } else {
            format!("{value:.1} {}", SIZE_UNITS[unit])
        }
    }
}

impl fmt::Display for ByteSize {
    /// Exact form that parses back: the largest unit dividing the value.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let unit = (1..SIZE_UNITS.len())
            .rev()
            .find(|&unit| self.0 != 0 && self.0.is_multiple_of(1u64 << (10 * unit)))
            .unwrap_or(0);
        write!(f, "{}{}", self.0 >> (10 * unit), SIZE_UNITS[unit])
    }
}

macro_rules! serde_via_str {
    ($type:ty) => {
        impl<'de> serde::Deserialize<'de> for $type {
            fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                let value = String::deserialize(deserializer)?;
                value.parse().map_err(serde::de::Error::custom)
            }
        }

        impl serde::Serialize for $type {
            fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.collect_str(self)
            }
        }
    };
}

serde_via_str!(HumanDuration);
serde_via_str!(ByteSize);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_parse_each_unit() {
        let parse = |s: &str| s.parse::<HumanDuration>().map(|d| d.0);
        assert_eq!(parse("250ms"), Ok(Duration::from_millis(250)));
        assert_eq!(parse("2s"), Ok(Duration::from_secs(2)));
        assert_eq!(parse("10m"), Ok(Duration::from_secs(600)));
        assert_eq!(parse(" 1h "), Ok(Duration::from_secs(3600)));
        assert!(parse("2").is_err());
        assert!(parse("s").is_err());
        assert!(parse("2 days").is_err());
        assert!(parse("18446744073709551615h").is_err());
    }

    #[test]
    fn durations_display_round_trips() {
        for text in ["250ms", "2s", "10m", "1h", "0s"] {
            assert_eq!(text.parse::<HumanDuration>().unwrap().to_string(), text);
        }
    }

    #[test]
    fn sizes_parse_binary_units() {
        let parse = |s: &str| s.parse::<ByteSize>().map(|b| b.0);
        assert_eq!(parse("512"), Ok(512));
        assert_eq!(parse("256KiB"), Ok(256 << 10));
        assert_eq!(parse("16MiB"), Ok(16 << 20));
        assert_eq!(parse("10GiB"), Ok(10 << 30));
        assert!(parse("10GB").is_err());
        assert!(parse("lots").is_err());
    }

    #[test]
    fn sizes_display_exactly_and_round_trip() {
        assert_eq!(ByteSize(512).to_string(), "512B");
        assert_eq!(ByteSize(1536).to_string(), "1536B");
        assert_eq!(ByteSize(10 << 30).to_string(), "10GiB");
        assert_eq!(ByteSize(0).to_string(), "0B");
        for size in [0, 1, 1536, 256 << 10, 10 << 30] {
            let text = ByteSize(size).to_string();
            assert_eq!(text.parse::<ByteSize>(), Ok(ByteSize(size)));
        }
    }

    #[test]
    fn sizes_have_a_human_form() {
        assert_eq!(ByteSize(512).human(), "512 B");
        assert_eq!(ByteSize(1536).human(), "1.5 KiB");
        assert_eq!(ByteSize(10 << 30).human(), "10.0 GiB");
    }
}
