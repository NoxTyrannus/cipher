use crate::common::AgentError;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use std::str::FromStr;
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ThoughtId(Uuid);

impl ThoughtId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    pub fn parse(value: &str) -> Result<Self, AgentError> {
        let id = Uuid::parse_str(value)
            .map(Self)
            .map_err(|error| AgentError::Parse(format!("parse thought ID '{value}': {error}")))?;

        if id.to_string() != value {
            return Err(AgentError::Parse(format!(
                "thought ID must use canonical hyphenated UUID form: '{value}'"
            )));
        }

        Ok(id)
    }
}

impl Default for ThoughtId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for ThoughtId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl FromStr for ThoughtId {
    type Err = AgentError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

impl Serialize for ThoughtId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for ThoughtId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(&value).map_err(serde::de::Error::custom)
    }
}

pub fn unix_timestamp_now() -> String {
    use std::time::Duration;
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs()
        .to_string()
}
