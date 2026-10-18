//! RFC 3339 text for the `time` crate's date and time types.
//!
//! This file is shipped into generated crates verbatim, and it is also compiled and unit-tested as
//! part of progeny itself — the same source in both places, so a change cannot pass the tests here
//! and break there.
//!
//! `time`'s own serde impls do not read or write RFC 3339.
//! Without its `serde-human-readable` feature they are a tuple of numbers, and with it they are
//! `time`'s own description, which puts a space before the hour and requires a subsecond.
//! A JSON Schema `date-time`, `date` or `time` is RFC 3339 text, so a generated type holds the
//! `time` value in [`Rfc3339`], whose impls read and write that text.
//! Every serde path a generated crate has goes through a value's own impls — the derive, the
//! hand-written bodies, a union's replay, the lenient decoder and parameter serialization — so the
//! text is right in each of them because it is right here.
//!
//! The paths are spelled out rather than imported because the file is spliced into the support
//! root beside others, where a second `use serde::Serializer` would be a duplicate definition.

/// A `time` value that reads and writes the RFC 3339 text its JSON Schema format names.
///
/// `Rfc3339<time::OffsetDateTime>` is a `date-time`, `Rfc3339<time::Date>` a `full-date`, and
/// `Rfc3339<time::Time>` a `partial-time`.
/// Each one is read only from a string, as `serde_json` reads a `chrono` or `jiff` value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Rfc3339<T>(pub T);

impl<T> From<T> for Rfc3339<T> {
    fn from(value: T) -> Self {
        Self(value)
    }
}

impl<T> std::ops::Deref for Rfc3339<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.0
    }
}

impl<T> std::ops::DerefMut for Rfc3339<T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.0
    }
}

impl serde::Serialize for Rfc3339<time::OffsetDateTime> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        // A year outside 0..=9999 and an offset with seconds have no RFC 3339 spelling.
        let text = self
            .0
            .format(&time::format_description::well_known::Rfc3339)
            .map_err(serde::ser::Error::custom)?;
        serializer.serialize_str(&text)
    }
}

impl<'de> serde::Deserialize<'de> for Rfc3339<time::OffsetDateTime> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_str(Rfc3339Text {
            expecting: "an RFC 3339 date-time",
            read: |text| {
                time::OffsetDateTime::parse(text, &time::format_description::well_known::Rfc3339)
                    .ok()
            },
        })
    }
}

impl serde::Serialize for Rfc3339<time::Date> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let date = self.0;
        if !(0..=9999).contains(&date.year()) {
            return Err(serde::ser::Error::custom(format_args!(
                "the year of {date} has no RFC 3339 full-date"
            )));
        }
        serializer.collect_str(&format_args!(
            "{:04}-{:02}-{:02}",
            date.year(),
            u8::from(date.month()),
            date.day()
        ))
    }
}

impl<'de> serde::Deserialize<'de> for Rfc3339<time::Date> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_str(Rfc3339Text {
            expecting: "an RFC 3339 full-date",
            read: rfc3339_date,
        })
    }
}

impl serde::Serialize for Rfc3339<time::Time> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let time = self.0;
        let whole = format!(
            "{:02}:{:02}:{:02}",
            time.hour(),
            time.minute(),
            time.second()
        );
        // A fraction only when there is one, without the zeros nanosecond precision pads it to.
        let text = match time.nanosecond() {
            0 => whole,
            nanoseconds => {
                let fraction = format!("{nanoseconds:09}");
                format!("{whole}.{}", fraction.trim_end_matches('0'))
            }
        };
        serializer.serialize_str(&text)
    }
}

impl<'de> serde::Deserialize<'de> for Rfc3339<time::Time> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_str(Rfc3339Text {
            expecting: "an RFC 3339 partial-time",
            read: rfc3339_time,
        })
    }
}

/// The visitor of one RFC 3339 form: a string, read by `read`.
struct Rfc3339Text<T> {
    expecting: &'static str,
    read: fn(&str) -> Option<T>,
}

impl<T> serde::de::Visitor<'_> for Rfc3339Text<T> {
    type Value = Rfc3339<T>;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.expecting)
    }

    fn visit_str<E>(self, text: &str) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        (self.read)(text)
            .map(Rfc3339)
            .ok_or_else(|| E::invalid_value(serde::de::Unexpected::Str(text), &self))
    }
}

/// `YYYY-MM-DD`, RFC 3339's `full-date`, when it names a day of the calendar.
fn rfc3339_date(text: &str) -> Option<time::Date> {
    let (year, rest) = text.split_once('-')?;
    let (month, day) = rest.split_once('-')?;
    let [year, month, day] =
        [(year, 4), (month, 2), (day, 2)].map(|(digits, width)| rfc3339_number(digits, width));
    let month = time::Month::try_from(u8::try_from(month?).ok()?).ok()?;
    let day = u8::try_from(day?).ok()?;
    time::Date::from_calendar_date(i32::try_from(year?).ok()?, month, day).ok()
}

/// `HH:MM:SS` with an optional fraction, RFC 3339's `partial-time`, when it names a time of day.
///
/// A fraction finer than a nanosecond is cut to one, which is what `time` can hold.
/// A leap second has no `time::Time`, so `23:59:60` is refused rather than moved.
fn rfc3339_time(text: &str) -> Option<time::Time> {
    let (whole, fraction) = match text.split_once('.') {
        Some((whole, fraction)) => (whole, Some(fraction)),
        None => (text, None),
    };
    let mut parts = whole.split(':');
    let [hour, minute, second] = [parts.next(), parts.next(), parts.next()]
        .map(|part| part.and_then(|digits| rfc3339_number(digits, 2)));
    if parts.next().is_some() {
        return None;
    }
    let nanoseconds = match fraction {
        None => 0,
        Some(fraction) if fraction.is_empty() || !fraction.bytes().all(|b| b.is_ascii_digit()) => {
            return None;
        }
        // Nine digits are a whole number of nanoseconds, so a shorter fraction is padded to them.
        Some(fraction) => {
            let nine = fraction.get(..9).unwrap_or(fraction);
            let scale = 10_u32.pow(u32::try_from(9 - nine.len()).ok()?);
            nine.parse::<u32>().ok()?.checked_mul(scale)?
        }
    };
    time::Time::from_hms_nano(
        u8::try_from(hour?).ok()?,
        u8::try_from(minute?).ok()?,
        u8::try_from(second?).ok()?,
        nanoseconds,
    )
    .ok()
}

/// Exactly `width` ASCII digits as a number.
fn rfc3339_number(digits: &str, width: usize) -> Option<u32> {
    (digits.len() == width && digits.bytes().all(|b| b.is_ascii_digit()))
        .then(|| digits.parse().ok())
        .flatten()
}

#[cfg(test)]
mod tests {
    use color_eyre::eyre;

    use super::Rfc3339;

    /// Each form reads its RFC 3339 text and writes it back unchanged.
    #[test_util::test]
    fn each_form_round_trips_its_rfc3339_text() {
        let date_time: Rfc3339<time::OffsetDateTime> =
            serde_json::from_str(r#""2024-01-02T03:04:05.5+02:00""#)?;
        assert_eq!(
            serde_json::to_string(&date_time)?,
            r#""2024-01-02T03:04:05.5+02:00""#
        );
        let date: Rfc3339<time::Date> = serde_json::from_str(r#""2024-01-02""#)?;
        assert_eq!(serde_json::to_string(&date)?, r#""2024-01-02""#);
        let time: Rfc3339<time::Time> = serde_json::from_str(r#""03:04:05""#)?;
        assert_eq!(serde_json::to_string(&time)?, r#""03:04:05""#);
        let fractional: Rfc3339<time::Time> = serde_json::from_str(r#""03:04:05.25""#)?;
        assert_eq!(serde_json::to_string(&fractional)?, r#""03:04:05.25""#);
    }

    /// The values read are the instants and days the text names, not merely text that survives.
    #[test_util::test]
    fn each_form_reads_the_value_its_text_names() {
        let date_time: Rfc3339<time::OffsetDateTime> =
            serde_json::from_str(r#""2024-01-02T03:04:05Z""#)?;
        assert_eq!(date_time.unix_timestamp(), 1_704_164_645);
        let date: Rfc3339<time::Date> = serde_json::from_str(r#""2024-02-29""#)?;
        assert_eq!(
            (date.year(), u8::from(date.month()), date.day()),
            (2024, 2, 29)
        );
        // A fraction past nanoseconds is cut to them, and a short one is scaled up to them.
        let fine: Rfc3339<time::Time> = serde_json::from_str(r#""03:04:05.1234567891""#)?;
        assert_eq!(fine.nanosecond(), 123_456_789);
        let short: Rfc3339<time::Time> = serde_json::from_str(r#""03:04:05.5""#)?;
        assert_eq!(short.nanosecond(), 500_000_000);
    }

    /// Text that is not the form, or names no real day or time, is refused rather than guessed.
    #[test_util::test]
    fn text_outside_each_form_is_refused() {
        for text in [
            r#""2024-01-02T03:04:05+0200""#,
            r#""2024-01-02T03:04:05""#,
            r#""2024-01-02""#,
        ] {
            assert!(
                serde_json::from_str::<Rfc3339<time::OffsetDateTime>>(text).is_err(),
                "{text}"
            );
        }
        for text in [
            r#""2023-02-29""#,
            r#""2024-1-02""#,
            r#""20240102""#,
            r#""2024-01-02T00:00:00Z""#,
        ] {
            assert!(
                serde_json::from_str::<Rfc3339<time::Date>>(text).is_err(),
                "{text}"
            );
        }
        for text in [
            r#""24:00:00""#,
            r#""23:59:60""#,
            r#""3:04:05""#,
            r#""03:04""#,
            r#""03:04:05.""#,
            r#""03:04:05.5x""#,
            r#""03:04:05:06""#,
        ] {
            assert!(
                serde_json::from_str::<Rfc3339<time::Time>>(text).is_err(),
                "{text}"
            );
        }
    }

    /// Only a string is a date or time: the tuple `time`'s own impls write is not read back.
    #[test_util::test]
    fn only_a_string_is_read() {
        let tuple = "[2024,2,0,0,0,0,0,0,0]";
        assert!(serde_json::from_str::<Rfc3339<time::OffsetDateTime>>(tuple).is_err());
        assert!(serde_json::from_str::<Rfc3339<time::Date>>("[2024,2]").is_err());
    }

    /// A value inside an option, a list or a map is the same text.
    #[test_util::test]
    fn a_value_is_text_inside_containers() {
        #[derive(Debug, serde::Deserialize, serde::Serialize)]
        struct Holder {
            at: Option<Rfc3339<time::OffsetDateTime>>,
            days: Vec<Rfc3339<time::Date>>,
            by_name: std::collections::BTreeMap<String, Rfc3339<time::Time>>,
        }
        let text =
            r#"{"at":"2024-01-02T03:04:05Z","days":["2024-01-02"],"by_name":{"open":"09:00:00"}}"#;
        let holder: Holder = serde_json::from_str(text)?;
        assert_eq!(serde_json::to_string(&holder)?, text);
    }

    /// A year RFC 3339 cannot spell is an error on the way out, not a malformed string.
    #[test_util::test]
    fn a_date_outside_four_digit_years_does_not_serialize() {
        let date = time::Date::from_calendar_date(-1, time::Month::January, 1)?;
        assert!(serde_json::to_string(&Rfc3339(date)).is_err());
    }
}
