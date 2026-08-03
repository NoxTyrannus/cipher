use crate::common::AgentError;
use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use std::str::FromStr;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct UtcTimestamp(DateTime<Utc>);

impl UtcTimestamp {
    pub fn now() -> Self {
        Self(Utc::now())
    }

    pub fn from_datetime(value: DateTime<Utc>) -> Self {
        Self(value)
    }

    pub fn parse(value: &str) -> Result<Self, AgentError> {
        let parsed = DateTime::parse_from_rfc3339(value)
            .map(|timestamp| Self(timestamp.with_timezone(&Utc)))
            .map_err(|error| {
                AgentError::Parse(format!("parse UTC timestamp '{value}': {error}"))
            })?;

        if parsed.to_string() != value {
            return Err(AgentError::Parse(format!(
                "UTC timestamp must use canonical fixed-nanosecond RFC3339 form: '{value}'"
            )));
        }

        Ok(parsed)
    }

    pub fn path_component(&self) -> String {
        self.to_string().replace(':', "-")
    }

    pub fn date_components(&self) -> (i32, u32, u32) {
        use chrono::Datelike;

        (self.0.year(), self.0.month(), self.0.day())
    }
}

impl fmt::Display for UtcTimestamp {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0.to_rfc3339_opts(SecondsFormat::Nanos, true))
    }
}

impl FromStr for UtcTimestamp {
    type Err = AgentError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

impl Serialize for UtcTimestamp {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for UtcTimestamp {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(&value).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_timestamp_round_trips_with_fixed_precision() {
        let timestamp = UtcTimestamp::parse("2026-07-15T12:34:56.123456789Z").unwrap();

        assert_eq!(timestamp.to_string(), "2026-07-15T12:34:56.123456789Z");
        assert_eq!(
            serde_json::to_string(&timestamp).unwrap(),
            "\"2026-07-15T12:34:56.123456789Z\""
        );
        assert_eq!(
            serde_json::from_str::<UtcTimestamp>("\"2026-07-15T12:34:56.123456789Z\"").unwrap(),
            timestamp
        );
    }

    #[test]
    fn timestamp_rejects_noncanonical_spellings() {
        assert!(UtcTimestamp::parse("2026-07-15T12:34:56Z").is_err());
        assert!(UtcTimestamp::parse("2026-07-15T12:34:56.123456789+00:00").is_err());
        assert!(UtcTimestamp::parse("2026-07-15T12:34:56.123Z").is_err());
    }

    #[test]
    fn path_component_is_windows_safe() {
        let timestamp = UtcTimestamp::parse("2026-07-15T12:34:56.123456789Z").unwrap();
        assert_eq!(timestamp.path_component(), "2026-07-15T12-34-56.123456789Z");
    }
}
