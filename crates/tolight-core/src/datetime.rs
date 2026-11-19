//! Native `time` timestamps with the project's existing RFC3339 wire formats.
use time::{OffsetDateTime, UtcOffset, format_description::well_known::Rfc3339};

pub fn parse_rfc3339(value: &str) -> Result<OffsetDateTime, time::error::Parse> {
    let invalid = |field| time::error::ParseFromDescription::InvalidComponent(field).into();
    // `time` accepts arbitrary separator bytes and clamps leap seconds. Keep
    // the existing separator grammar and reject a lossy leap-second conversion.
    if !matches!(value.as_bytes().get(10), Some(b'T' | b't' | b' ')) {
        return Err(invalid("separator"));
    }
    if value.as_bytes().get(17..19) == Some(b"60") {
        return Err(invalid("second"));
    }
    let date = OffsetDateTime::parse(value, &Rfc3339)?
        .checked_to_offset(UtcOffset::UTC)
        .ok_or_else(|| invalid("UTC year"))?;
    if !(0..=9999).contains(&date.year()) {
        return Err(invalid("UTC year"));
    }
    Ok(date)
}

pub fn from_timestamp(
    seconds: i64,
    nanoseconds: u32,
) -> Result<OffsetDateTime, time::error::ComponentRange> {
    OffsetDateTime::from_unix_timestamp(seconds)?.replace_nanosecond(nanoseconds)
}

pub fn from_timestamp_millis(
    milliseconds: i64,
) -> Result<OffsetDateTime, time::error::ComponentRange> {
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(milliseconds) * 1_000_000)
}

fn fraction(date: OffsetDateTime, digits: Option<usize>) -> String {
    let nanos = date.nanosecond();
    let digits = digits.unwrap_or_else(|| {
        if nanos == 0 {
            0
        } else if nanos.is_multiple_of(1_000_000) {
            3
        } else if nanos.is_multiple_of(1_000) {
            6
        } else {
            9
        }
    });
    if digits == 0 {
        String::new()
    } else {
        format!(".{:0digits$}", nanos / 10_u32.pow(9 - digits as u32))
    }
}

fn format(date: OffsetDateTime, digits: Option<usize>, z: bool, display: bool) -> String {
    let date = date.to_offset(UtcOffset::UTC);
    let separator = if display { ' ' } else { 'T' };
    let suffix = if display {
        " UTC"
    } else if z {
        "Z"
    } else {
        "+00:00"
    };
    let year = if date.year() < 0 {
        format!("-{:04}", date.year().unsigned_abs())
    } else {
        format!("{:04}", date.year())
    };
    format!(
        "{year}-{:02}-{:02}{separator}{:02}:{:02}:{:02}{}{suffix}",
        u8::from(date.month()),
        date.day(),
        date.hour(),
        date.minute(),
        date.second(),
        fraction(date, digits),
    )
}

/// Existing explicit RFC3339 display: UTC offset and automatic 0/3/6/9 precision.
pub fn rfc3339(date: OffsetDateTime) -> String {
    format(date, None, false, false)
}

/// Existing timestamp serialization: UTC `Z` and automatic 0/3/6/9 precision.
pub fn json_timestamp(date: OffsetDateTime) -> String {
    format(date, None, true, false)
}

pub fn utc_seconds(date: OffsetDateTime) -> String {
    format(date, Some(0), true, false)
}

pub fn utc_micros(date: OffsetDateTime) -> String {
    format(date, Some(6), true, false)
}

pub fn utc_nanos(date: OffsetDateTime) -> String {
    format(date, Some(9), true, false)
}

/// Preserve the operator CLI's existing human-readable UTC display.
pub fn display_utc(date: OffsetDateTime) -> String {
    format(date, None, false, true)
}

pub fn http_date(date: OffsetDateTime) -> String {
    const DAYS: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let date = date.to_offset(UtcOffset::UTC);
    format!(
        "{}, {:02} {} {:04} {:02}:{:02}:{:02} GMT",
        DAYS[usize::from(date.weekday().number_days_from_monday())],
        date.day(),
        MONTHS[usize::from(u8::from(date.month())) - 1],
        date.year(),
        date.hour(),
        date.minute(),
        date.second()
    )
}

/// Formatting convenience on native timestamps; storage and arithmetic use `time`.
pub trait TimestampExt {
    fn to_rfc3339(self) -> String;
}
impl TimestampExt for OffsetDateTime {
    fn to_rfc3339(self) -> String {
        rfc3339(self)
    }
}

pub mod serde {
    use super::*;
    use ::serde::{Deserialize, Deserializer, Serializer, de::Error};

    pub fn serialize<S: Serializer>(
        date: &OffsetDateTime,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&json_timestamp(*date))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<OffsetDateTime, D::Error> {
        parse_rfc3339(&String::deserialize(deserializer)?).map_err(D::Error::custom)
    }

    pub mod option {
        use super::*;

        pub fn serialize<S: Serializer>(
            date: &Option<OffsetDateTime>,
            serializer: S,
        ) -> Result<S::Ok, S::Error> {
            match date {
                Some(date) => serializer.serialize_some(&json_timestamp(*date)),
                None => serializer.serialize_none(),
            }
        }

        pub fn deserialize<'de, D: Deserializer<'de>>(
            deserializer: D,
        ) -> Result<Option<OffsetDateTime>, D::Error> {
            Option::<String>::deserialize(deserializer)?
                .map(|date| parse_rfc3339(&date).map_err(D::Error::custom))
                .transpose()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ::serde::{Deserialize, Serialize};
    use serde_json::json;

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Record {
        #[serde(with = "super::serde")]
        at: OffsetDateTime,
        #[serde(default, with = "super::serde::option")]
        until: Option<OffsetDateTime>,
    }

    #[test]
    fn timestamp_formats_preserve_utc_precision_and_pre_epoch_instants() {
        for (nanos, fraction) in [
            (0, ""),
            (120_000_000, ".120"),
            (123_400_000, ".123400"),
            (123_456_789, ".123456789"),
        ] {
            let date = from_timestamp(-1, nanos).unwrap();
            assert_eq!(
                rfc3339(date),
                format!("1969-12-31T23:59:59{fraction}+00:00")
            );
            assert_eq!(
                json_timestamp(date),
                format!("1969-12-31T23:59:59{fraction}Z")
            );
            assert_eq!(utc_seconds(date), "1969-12-31T23:59:59Z");
            assert_eq!(
                display_utc(date),
                format!("1969-12-31 23:59:59{fraction} UTC")
            );
        }
        let date = from_timestamp(-1, 123_456_789).unwrap();
        assert_eq!(utc_micros(date), "1969-12-31T23:59:59.123456Z");
        assert_eq!(utc_nanos(date), "1969-12-31T23:59:59.123456789Z");
        assert_eq!(
            from_timestamp_millis(-1).unwrap().unix_timestamp_nanos(),
            -1_000_000
        );
        assert_eq!(
            http_date(from_timestamp(0, 0).unwrap()),
            "Thu, 01 Jan 1970 00:00:00 GMT"
        );
    }

    #[test]
    fn json_dates_remain_strings_and_accept_legacy_offsets_null_and_missing() {
        let legacy = json!({"at":"2026-11-19T08:00:00.123400+08:00"});
        let record: Record = serde_json::from_value(legacy).unwrap();
        assert_eq!(record.until, None);
        assert_eq!(record.at.offset(), UtcOffset::UTC);
        assert_eq!(
            serde_json::to_value(&record).unwrap(),
            json!({"at":"2026-11-19T00:00:00.123400Z","until":null})
        );
        let present = json!({"at":"2026-11-19T00:00:00Z","until":"2026-11-19T01:00:00.001Z"});
        let record: Record = serde_json::from_value(present.clone()).unwrap();
        assert_eq!(serde_json::to_value(record).unwrap(), present);
        for invalid in [
            json!({"at":123}),
            json!({"at":"not-a-date"}),
            json!({"at":"2026-11-19T00:00:00Z","until":[]}),
        ] {
            assert!(serde_json::from_value::<Record>(invalid).is_err());
        }
    }

    #[test]
    fn external_dates_never_silently_change_or_panic_at_utc_boundaries() {
        for separator in ['T', 't', ' '] {
            assert!(parse_rfc3339(&format!("2026-11-19{separator}00:00:00Z")).is_ok());
        }
        for invalid in [
            "2026-11-19X00:00:00Z",
            "2026-11-19\n00:00:00Z",
            "2016-12-31T23:59:60Z",
            "2016-12-31T23:59:60.5Z",
            "9999-12-31T23:59:59-23:59",
            "0000-01-01T00:00:00+00:01",
            "not a date",
        ] {
            assert!(
                std::panic::catch_unwind(|| parse_rfc3339(invalid))
                    .unwrap()
                    .is_err(),
                "{invalid}"
            );
        }
        assert_eq!(
            json_timestamp(parse_rfc3339("2026-11-19T00:00:00.000001+00:01").unwrap()),
            "2026-11-18T23:59:00.000001Z"
        );
    }
}
