//! Calendar and wire timestamps are separate from elapsed, POSIX time. In
//! particular, imported history may contain a leap second even though neither
//! the system clock nor IANA timezone transitions count leap seconds.
use std::{cmp::Ordering, fmt, str::FromStr};

use schemars::{
    JsonSchema,
    schema::{InstanceType, Schema, SchemaObject},
};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use time::{Duration, Month, OffsetDateTime, PrimitiveDateTime, Time, UtcOffset};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct Date(time::Date);

impl Date {
    pub fn new(year: i32, month: u32, day: u32) -> Option<Self> {
        // Preserve the existing date representation's range, including inputs
        // rejected later by the narrower maintenance due-date validation.
        if !(-262_143..=262_142).contains(&year) {
            return None;
        }
        let month = Month::try_from(u8::try_from(month).ok()?).ok()?;
        time::Date::from_calendar_date(year, month, u8::try_from(day).ok()?)
            .ok()
            .map(Self)
    }

    pub fn year(self) -> i32 {
        self.0.year()
    }

    pub fn checked_add_months(self, months: u32) -> Option<Self> {
        let index = i64::from(self.0.year()) * 12 + i64::from(u8::from(self.0.month())) - 1
            + i64::from(months);
        let year = i32::try_from(index.div_euclid(12)).ok()?;
        let month = u32::try_from(index.rem_euclid(12) + 1).ok()?;
        // Calendar recurrence clamps month ends before adding elapsed days.
        (1..=u32::from(self.0.day()))
            .rev()
            .find_map(|day| Self::new(year, month, day))
    }

    pub fn checked_add_days(self, days: u64) -> Option<Self> {
        let seconds = i64::try_from(days).ok()?.checked_mul(86_400)?;
        let date = self.0.checked_add(Duration::seconds(seconds))?;
        Self::new(
            date.year(),
            u32::from(u8::from(date.month())),
            u32::from(date.day()),
        )
    }

    pub fn parse_storage(value: &str) -> Result<Self, ParseError> {
        let mut parser = Parser {
            remaining: value,
            relaxed: false,
        };
        let date = parser.date(false)?;
        parser.finish()?;
        Ok(date)
    }
}

impl fmt::Display for Date {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let year = self.0.year();
        match year {
            0..=9999 => write!(f, "{year:04}"),
            ..0 => write!(f, "-{:04}", -year),
            _ => write!(f, "+{year}"),
        }?;
        write!(f, "-{:02}-{:02}", u8::from(self.0.month()), self.0.day())
    }
}

impl FromStr for Date {
    type Err = ParseError;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let mut parser = Parser {
            remaining: value,
            relaxed: true,
        };
        let date = parser.date(false)?;
        parser.whitespace();
        parser.finish()?;
        Ok(date)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Timestamp {
    utc: OffsetDateTime,
    leap_second: bool,
}

impl Timestamp {
    pub fn now() -> Self {
        Self {
            utc: OffsetDateTime::now_utc(),
            leap_second: false,
        }
    }

    pub fn unix_seconds(self) -> i64 {
        self.utc.unix_timestamp()
    }

    pub fn utc_date(self) -> Date {
        Date(self.utc.date())
    }

    pub fn local_date(self, zone: &jiff::tz::TimeZone) -> Result<Date, crate::error::Error> {
        // The existing compiled calendar has no generated rule transitions
        // after 2099. Preserve that end-of-table offset instead of introducing
        // indefinitely recurring future DST as part of a dependency migration.
        let transition_second = self
            .unix_seconds()
            .clamp(jiff::Timestamp::MIN.as_second(), 4_102_444_799);
        let timestamp = jiff::Timestamp::from_second(transition_second)
            .map_err(|_| crate::error::Error::Corrupt)?;
        let offset = zone.to_offset(timestamp);
        let local = self
            .utc
            .checked_add(Duration::seconds(i64::from(offset.seconds())))
            .ok_or(crate::error::Error::Corrupt)?;
        Date::new(
            local.year(),
            u32::from(u8::from(local.month())),
            u32::from(local.day()),
        )
        .ok_or(crate::error::Error::Corrupt)
    }

    pub fn parse_storage(value: &str) -> Result<Self, ParseError> {
        Self::parse(value, false)
    }

    pub fn to_storage(self) -> String {
        self.format(false)
    }

    fn format(self, zulu: bool) -> String {
        let mut result = format!(
            "{}T{:02}:{:02}:{:02}",
            self.utc_date(),
            self.utc.hour(),
            self.utc.minute(),
            if self.leap_second {
                60
            } else {
                self.utc.second()
            }
        );
        let nano = self.utc.nanosecond();
        if nano != 0 {
            if nano.is_multiple_of(1_000_000) {
                result.push_str(&format!(".{:03}", nano / 1_000_000));
            } else if nano.is_multiple_of(1_000) {
                result.push_str(&format!(".{:06}", nano / 1_000));
            } else {
                result.push_str(&format!(".{nano:09}"));
            }
        }
        result.push_str(if zulu { "Z" } else { "+00:00" });
        result
    }

    fn parse(value: &str, relaxed: bool) -> Result<Self, ParseError> {
        let mut parser = Parser {
            remaining: value,
            relaxed,
        };
        let date = parser.date(!relaxed)?;
        match parser.remaining.as_bytes().first() {
            Some(b'T' | b't' | b' ') => parser.remaining = &parser.remaining[1..],
            _ => return Err(parser.expected()),
        }
        let hour = parser.number(2, !relaxed)?;
        parser.separator(':')?;
        let minute = parser.number(2, !relaxed)?;
        parser.separator(':')?;
        let second = parser.number(2, !relaxed)?;
        let leap_second = second == 60;
        let mut nano = 0;
        if parser.remaining.starts_with('.') {
            parser.remaining = &parser.remaining[1..];
            let digits = parser
                .remaining
                .bytes()
                .take_while(u8::is_ascii_digit)
                .count();
            if digits == 0 {
                return Err(parser.expected());
            }
            // The existing history contract truncates additional fractional
            // precision instead of rounding into the following second.
            let count = digits.min(9);
            nano = parser.remaining[..count]
                .parse::<u32>()
                .map_err(|_| ParseError::Range)?
                * 10_u32.pow(u32::try_from(9 - count).map_err(|_| ParseError::Range)?);
            parser.remaining = &parser.remaining[digits..];
        }
        if relaxed {
            parser.whitespace();
        }
        let offset = if parser.remaining.starts_with(['Z', 'z']) {
            parser.remaining = &parser.remaining[1..];
            0
        } else if relaxed
            && parser
                .remaining
                .get(..3)
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case("UTC"))
        {
            parser.remaining = &parser.remaining[3..];
            0
        } else {
            let sign = match parser.remaining.chars().next() {
                Some('+') => 1,
                Some('-' | '−') => -1,
                _ => return Err(parser.expected()),
            };
            let length = parser
                .remaining
                .chars()
                .next()
                .ok_or(ParseError::Short)?
                .len_utf8();
            parser.remaining = &parser.remaining[length..];
            let hours = parser.offset_number()?;
            if relaxed {
                parser.remaining = parser.remaining.trim_start_matches(|character: char| {
                    character == ':' || character.is_whitespace()
                });
            } else {
                parser.literal(':')?;
            }
            let minutes = parser.offset_number()?;
            if hours > 23 || minutes > 59 {
                return Err(ParseError::Range);
            }
            sign * (hours * 3600 + minutes * 60)
        };
        if relaxed {
            parser.whitespace();
        }
        parser.finish()?;
        let time = Time::from_hms_nano(
            u8::try_from(hour).map_err(|_| ParseError::Range)?,
            u8::try_from(minute).map_err(|_| ParseError::Range)?,
            u8::try_from(if leap_second { 59 } else { second }).map_err(|_| ParseError::Range)?,
            nano,
        )
        .map_err(|_| ParseError::Range)?;
        let offset = UtcOffset::from_whole_seconds(offset).map_err(|_| ParseError::Range)?;
        let utc = PrimitiveDateTime::new(date.0, time)
            .assume_offset(offset)
            .to_offset(UtcOffset::UTC);
        Date::new(
            utc.year(),
            u32::from(u8::from(utc.month())),
            u32::from(utc.day()),
        )
        .ok_or(ParseError::Range)?;
        Ok(Self { utc, leap_second })
    }
}

impl FromStr for Timestamp {
    type Err = ParseError;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value, true)
    }
}

impl Ord for Timestamp {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.unix_seconds(), self.leap_second, self.utc.nanosecond()).cmp(&(
            other.unix_seconds(),
            other.leap_second,
            other.utc.nanosecond(),
        ))
    }
}
impl PartialOrd for Timestamp {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error("input contains invalid characters")]
    Invalid,
    #[error("input is out of range")]
    Range,
    #[error("premature end of input")]
    Short,
    #[error("trailing input")]
    Trailing,
}

// Keep the existing wire grammar at the boundary, while time owns calendar
// validity, offset arithmetic, bounds and ordering. Storage parsing is narrower
// than serde: imported timestamps are persisted in RFC3339, not the relaxed
// spelling accepted by historical migration payloads.
struct Parser<'a> {
    remaining: &'a str,
    relaxed: bool,
}
impl Parser<'_> {
    fn expected(&self) -> ParseError {
        if self.remaining.is_empty() {
            ParseError::Short
        } else {
            ParseError::Invalid
        }
    }
    fn whitespace(&mut self) {
        self.remaining = self.remaining.trim_start();
    }
    fn number(&mut self, width: usize, exact: bool) -> Result<i32, ParseError> {
        if !exact {
            self.whitespace();
        }
        let count = self
            .remaining
            .bytes()
            .take(width)
            .take_while(u8::is_ascii_digit)
            .count();
        if count == 0 || exact && count != width {
            return Err(self.expected());
        }
        let value = self.remaining[..count]
            .parse()
            .map_err(|_| ParseError::Range)?;
        self.remaining = &self.remaining[count..];
        Ok(value)
    }
    fn offset_number(&mut self) -> Result<i32, ParseError> {
        self.number(2, true)
    }
    fn literal(&mut self, character: char) -> Result<(), ParseError> {
        self.remaining = self
            .remaining
            .strip_prefix(character)
            .ok_or_else(|| self.expected())?;
        Ok(())
    }
    fn separator(&mut self, character: char) -> Result<(), ParseError> {
        if self.relaxed {
            self.whitespace();
        }
        self.literal(character)
    }
    fn date(&mut self, exact: bool) -> Result<Date, ParseError> {
        if !exact {
            self.whitespace();
        }
        let (sign, signed) = if !exact && self.remaining.starts_with(['+', '-']) {
            let sign = if self.remaining.starts_with('-') {
                -1
            } else {
                1
            };
            self.remaining = &self.remaining[1..];
            (sign, true)
        } else {
            (1, false)
        };
        // A sign allows expanded years, but not whitespace after the sign.
        let year = if signed {
            let digits = self
                .remaining
                .bytes()
                .take_while(u8::is_ascii_digit)
                .count();
            if digits == 0 {
                return Err(self.expected());
            }
            let year = self.remaining[..digits]
                .parse::<i32>()
                .map_err(|_| ParseError::Range)?;
            self.remaining = &self.remaining[digits..];
            year.checked_mul(sign).ok_or(ParseError::Range)?
        } else {
            self.number(4, exact)?
        };
        self.separator('-')?;
        let month = self.number(2, exact)?;
        self.separator('-')?;
        let day = self.number(2, exact)?;
        Date::new(
            year,
            u32::try_from(month).map_err(|_| ParseError::Range)?,
            u32::try_from(day).map_err(|_| ParseError::Range)?,
        )
        .ok_or(ParseError::Range)
    }
    fn finish(&self) -> Result<(), ParseError> {
        if self.remaining.is_empty() {
            Ok(())
        } else {
            Err(ParseError::Trailing)
        }
    }
}

impl Serialize for Date {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}
impl Serialize for Timestamp {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.format(true))
    }
}
impl<'de> Deserialize<'de> for Date {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_str(DateVisitor)
    }
}
struct DateVisitor;
impl de::Visitor<'_> for DateVisitor {
    type Value = Date;
    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a formatted date string")
    }
    fn visit_str<E: de::Error>(self, value: &str) -> Result<Date, E> {
        value.parse().map_err(E::custom)
    }
}
impl<'de> Deserialize<'de> for Timestamp {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_str(TimestampVisitor)
    }
}
struct TimestampVisitor;
impl de::Visitor<'_> for TimestampVisitor {
    type Value = Timestamp;
    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("an RFC 3339 formatted date and time string")
    }
    fn visit_str<E: de::Error>(self, value: &str) -> Result<Timestamp, E> {
        value.parse().map_err(E::custom)
    }
}

impl JsonSchema for Date {
    fn schema_name() -> String {
        "NaiveDate".to_owned()
    }
    fn is_referenceable() -> bool {
        false
    }
    fn json_schema(_: &mut schemars::r#gen::SchemaGenerator) -> Schema {
        string_schema("date")
    }
}
impl JsonSchema for Timestamp {
    fn schema_name() -> String {
        "DateTime".to_owned()
    }
    fn is_referenceable() -> bool {
        false
    }
    fn json_schema(_: &mut schemars::r#gen::SchemaGenerator) -> Schema {
        string_schema("date-time")
    }
}
fn string_schema(format: &str) -> Schema {
    SchemaObject {
        instance_type: Some(InstanceType::String.into()),
        format: Some(format.to_owned()),
        ..Default::default()
    }
    .into()
}

#[cfg(test)]
mod tests {
    use crate::calendar::{Date, Timestamp};

    #[test]
    fn timestamp_wire_bounds_fractions_leap_seconds_and_ordering() {
        for (input, expected) in [
            (
                "1969-12-31T23:59:59.123456789Z",
                "1969-12-31T23:59:59.123456789Z",
            ),
            (
                "2017-01-01T01:59:60.123456789123+02:00",
                "2016-12-31T23:59:60.123456789Z",
            ),
            ("2025-02-03T04:05:06−23:59", "2025-02-04T04:04:06Z"),
            ("-262143-01-01T00:00:00Z", "-262143-01-01T00:00:00Z"),
            (
                "+262142-12-31T23:59:59.999999999Z",
                "+262142-12-31T23:59:59.999999999Z",
            ),
        ] {
            let timestamp = input.parse::<Timestamp>().unwrap();
            assert_eq!(
                serde_json::to_string(&timestamp).unwrap(),
                format!("\"{expected}\"")
            );
            assert_eq!(
                serde_json::from_str::<Timestamp>(&serde_json::to_string(&timestamp).unwrap())
                    .unwrap(),
                timestamp
            );
        }
        for input in [
            "+262143-01-01T00:00:00Z",
            "2025-02-29T00:00:00Z",
            "2025-02-03T24:00:00Z",
            "2025-02-03T04:05:61Z",
            "2025-02-03T04:05:06+24:00",
            "2025-02-03T04:05:06.Z",
            "2025-02-03T04:05:06.éZ",
        ] {
            assert!(input.parse::<Timestamp>().is_err(), "{input}");
        }
        let before = "1969-12-31T23:59:59.999999999Z"
            .parse::<Timestamp>()
            .unwrap();
        let leap = "1969-12-31T23:59:60Z".parse::<Timestamp>().unwrap();
        let epoch = "1970-01-01T00:00:00Z".parse::<Timestamp>().unwrap();
        assert!(before < leap && leap < epoch);
        assert_eq!(before.unix_seconds(), -1);
        assert_eq!(leap.unix_seconds(), -1);
        assert_eq!(epoch.unix_seconds(), 0);
    }

    #[test]
    fn storage_grammar_and_checked_calendar_bounds() {
        let timestamp = "2025-02-03 4:5:6 UTC".parse::<Timestamp>().unwrap();
        assert!(Timestamp::parse_storage("2025-02-03 4:5:6 UTC").is_err());
        assert_eq!(timestamp.to_storage(), "2025-02-03T04:05:06+00:00");
        assert_eq!(
            Timestamp::parse_storage(&timestamp.to_storage()).unwrap(),
            timestamp
        );
        assert!(Timestamp::parse_storage("+10000-01-01T00:00:00Z").is_err());
        let date = Date::new(2032, 2, 29).unwrap();
        assert_eq!(Date::parse_storage("2032-2-29").unwrap(), date);
        assert!(Date::parse_storage("2032 -2-29").is_err());
        assert!(Date::parse_storage("2032-2-29 ").is_err());
        assert!(date.checked_add_days(u64::MAX).is_none());
        assert!(
            date.checked_add_days(u64::try_from(i64::MAX).unwrap())
                .is_none()
        );
        assert!(
            Date::new(262_142, 12, 31)
                .unwrap()
                .checked_add_days(1)
                .is_none()
        );
    }
}
