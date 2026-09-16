use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Event {
    pub seq: u64,
    pub at: DateTime<Utc>,
    pub source: EventSource,
    pub kind: String,
    pub data: serde_json::Value,
    #[serde(default)]
    pub correlations: Vec<EventRef>,
}

impl Event {
    #[must_use]
    pub fn summary(&self) -> EventSummary {
        EventSummary {
            seq: self.seq,
            at: self.at,
            source: self.source,
            kind: self.kind.clone(),
            data_excerpt: self.data.clone(),
        }
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EventSource {
    A11yUia,
    A11yWinEvent,
    A11yCdp,
    Perception,
    PerceptionDetection,
    PerceptionHud,
    PerceptionAudio,
    Filesystem,
    Process,
    Clipboard,
    ActionEmitter,
    Reflex,
    System,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EventRef {
    pub seq: u64,
    pub relation: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EventSummary {
    pub seq: u64,
    pub at: DateTime<Utc>,
    pub source: EventSource,
    pub kind: String,
    pub data_excerpt: serde_json::Value,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "op", rename_all = "lowercase")]
pub enum EventFilter {
    All,
    None,
    Kind {
        kind: String,
    },
    Source {
        source: EventSource,
    },
    And {
        args: Vec<Self>,
    },
    Or {
        args: Vec<Self>,
    },
    Not {
        arg: Box<Self>,
    },
    Data {
        path: String,
        predicate: DataPredicate,
    },
}

pub const EVENT_FILTER_MAX_DEPTH: u32 = 8;

impl EventFilter {
    #[must_use]
    pub fn matches(&self, event: &Event) -> bool {
        crate::filter::matches_event_filter(self, event)
    }

    #[must_use]
    pub fn is_trivially_always_true(&self) -> bool {
        match self {
            Self::All => true,
            Self::None | Self::Kind { .. } | Self::Source { .. } | Self::Data { .. } => false,
            Self::And { args } => args.iter().all(Self::is_trivially_always_true),
            Self::Or { args } => args.iter().any(Self::is_trivially_always_true),
            Self::Not { arg } => arg.is_trivially_always_false(),
        }
    }

    fn is_trivially_always_false(&self) -> bool {
        match self {
            Self::None => true,
            Self::All | Self::Kind { .. } | Self::Source { .. } | Self::Data { .. } => false,
            Self::And { args } => args.iter().any(Self::is_trivially_always_false),
            Self::Or { args } => args.iter().all(Self::is_trivially_always_false),
            Self::Not { arg } => arg.is_trivially_always_true(),
        }
    }

    #[must_use]
    pub fn depth(&self) -> u32 {
        match self {
            Self::All
            | Self::None
            | Self::Kind { .. }
            | Self::Source { .. }
            | Self::Data { .. } => 1,
            Self::And { args } | Self::Or { args } => args
                .iter()
                .map(Self::depth)
                .max()
                .map_or(1, |child_depth| child_depth.saturating_add(1)),
            Self::Not { arg } => arg.depth().saturating_add(1),
        }
    }

    /// Validates this filter for M3 reflex/event subscription use.
    ///
    /// # Errors
    ///
    /// Returns an error when an `And`/`Or` node is empty or when the tree depth
    /// exceeds `EVENT_FILTER_MAX_DEPTH`.
    pub fn validate(&self) -> Result<(), EventFilterValidationError> {
        self.validate_with_max_depth(EVENT_FILTER_MAX_DEPTH)
    }

    /// Validates this filter against a caller-provided maximum depth.
    ///
    /// # Errors
    ///
    /// Returns an error when an `And`/`Or` node is empty or when the tree depth
    /// exceeds `max_depth`.
    pub fn validate_with_max_depth(
        &self,
        max_depth: u32,
    ) -> Result<(), EventFilterValidationError> {
        let depth = self.depth();
        if depth > max_depth {
            return Err(EventFilterValidationError::DepthExceeded { depth, max_depth });
        }
        match self {
            Self::And { args } => {
                if args.is_empty() {
                    return Err(EventFilterValidationError::EmptyAnd);
                }
                for arg in args {
                    arg.validate_with_max_depth(max_depth)?;
                }
            }
            Self::Or { args } => {
                if args.is_empty() {
                    return Err(EventFilterValidationError::EmptyOr);
                }
                for arg in args {
                    arg.validate_with_max_depth(max_depth)?;
                }
            }
            Self::Not { arg } => arg.validate_with_max_depth(max_depth)?,
            Self::Data { path, predicate } => validate_data_filter(path, predicate)?,
            Self::All | Self::None | Self::Kind { .. } | Self::Source { .. } => {}
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum EventFilterValidationError {
    #[error("event filter 'and' must contain at least one argument")]
    EmptyAnd,
    #[error("event filter 'or' must contain at least one argument")]
    EmptyOr,
    #[error("event filter depth {depth} exceeds maximum {max_depth}")]
    DepthExceeded { depth: u32, max_depth: u32 },
    #[error("event data filter path '{path}' is invalid: {reason}")]
    InvalidDataPath { path: String, reason: String },
    #[error("event data filter regex at '{path}' is invalid: {detail}")]
    InvalidRegex {
        path: String,
        pattern: String,
        detail: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum DataPredicate {
    Eq { value: serde_json::Value },
    Ne { value: serde_json::Value },
    Lt { value: serde_json::Value },
    Le { value: serde_json::Value },
    Gt { value: serde_json::Value },
    Ge { value: serde_json::Value },
    Regex { pattern: String },
    InSet { values: Vec<serde_json::Value> },
    Exists,
}

impl DataPredicate {
    #[must_use]
    pub fn matches(&self, value: Option<&serde_json::Value>) -> bool {
        crate::filter::matches_data_predicate(self, value)
    }
}

fn validate_data_filter(
    path: &str,
    predicate: &DataPredicate,
) -> Result<(), EventFilterValidationError> {
    validate_json_pointer(path)?;
    if let DataPredicate::Regex { pattern } = predicate {
        regex::Regex::new(pattern).map_err(|error| EventFilterValidationError::InvalidRegex {
            path: path.to_owned(),
            pattern: pattern.to_owned(),
            detail: error.to_string(),
        })?;
    }
    Ok(())
}

fn validate_json_pointer(path: &str) -> Result<(), EventFilterValidationError> {
    if path.is_empty() {
        return Ok(());
    }
    if !path.starts_with('/') {
        return Err(EventFilterValidationError::InvalidDataPath {
            path: path.to_owned(),
            reason: "path must be empty or start with '/'".to_owned(),
        });
    }

    let mut chars = path.chars();
    while let Some(ch) = chars.next() {
        if ch != '~' {
            continue;
        }
        match chars.next() {
            Some('0' | '1') => {}
            Some(value) => {
                return Err(EventFilterValidationError::InvalidDataPath {
                    path: path.to_owned(),
                    reason: format!("invalid '~{value}' escape; use '~0' or '~1'"),
                });
            }
            None => {
                return Err(EventFilterValidationError::InvalidDataPath {
                    path: path.to_owned(),
                    reason: "trailing '~' escape".to_owned(),
                });
            }
        }
    }
    Ok(())
}
