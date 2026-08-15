use regex::Regex;
use serde_json::Value;

use crate::{DataPredicate, Event, EventFilter, EventFilterValidationError, EventSource};

/// An event filter whose data-regex predicates were compiled once at the
/// filter acceptance boundary. Runtime event matching performs no parsing or
/// allocation.
#[derive(Clone, Debug)]
pub enum CompiledEventFilter {
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
        predicate: CompiledDataPredicate,
    },
}

#[derive(Clone, Debug)]
pub enum CompiledDataPredicate {
    Eq { value: Value },
    Ne { value: Value },
    Lt { value: Value },
    Le { value: Value },
    Gt { value: Value },
    Ge { value: Value },
    Regex { regex: Regex },
    InSet { values: Vec<Value> },
    Exists,
}

impl CompiledEventFilter {
    /// Validates and compiles one serialized event-filter tree.
    ///
    /// # Errors
    ///
    /// Returns the same structural, JSON-pointer, or regex error as
    /// [`EventFilter::validate`]. No partially compiled filter is returned.
    pub fn compile(filter: &EventFilter) -> Result<Self, EventFilterValidationError> {
        Self::compile_owned(filter.clone())
    }

    /// Validates and consumes one serialized event-filter tree, avoiding a
    /// duplicate tree allocation when the acceptance boundary already owns it.
    ///
    /// # Errors
    ///
    /// Returns the same structural, JSON-pointer, or regex error as
    /// [`EventFilter::validate`]. No partially compiled filter is returned.
    pub fn compile_owned(filter: EventFilter) -> Result<Self, EventFilterValidationError> {
        filter.validate()?;
        Self::compile_validated_owned(filter)
    }

    fn compile_validated_owned(filter: EventFilter) -> Result<Self, EventFilterValidationError> {
        match filter {
            EventFilter::All => Ok(Self::All),
            EventFilter::None => Ok(Self::None),
            EventFilter::Kind { kind } => Ok(Self::Kind { kind }),
            EventFilter::Source { source } => Ok(Self::Source { source }),
            EventFilter::And { args } => Ok(Self::And {
                args: args
                    .into_iter()
                    .map(Self::compile_validated_owned)
                    .collect::<Result<Vec<_>, _>>()?,
            }),
            EventFilter::Or { args } => Ok(Self::Or {
                args: args
                    .into_iter()
                    .map(Self::compile_validated_owned)
                    .collect::<Result<Vec<_>, _>>()?,
            }),
            EventFilter::Not { arg } => Ok(Self::Not {
                arg: Box::new(Self::compile_validated_owned(*arg)?),
            }),
            EventFilter::Data { path, predicate } => Ok(Self::Data {
                predicate: CompiledDataPredicate::compile(&path, predicate)?,
                path,
            }),
        }
    }

    #[must_use]
    pub fn matches(&self, event: &Event) -> bool {
        match self {
            Self::All => true,
            Self::None => false,
            Self::Kind { kind } => event.kind == *kind,
            Self::Source { source } => event.source == *source,
            Self::And { args } => args.iter().all(|item| item.matches(event)),
            Self::Or { args } => args.iter().any(|item| item.matches(event)),
            Self::Not { arg } => !arg.matches(event),
            Self::Data { path, predicate } => predicate.matches(event.data.pointer(path)),
        }
    }
}

impl CompiledDataPredicate {
    fn compile(path: &str, predicate: DataPredicate) -> Result<Self, EventFilterValidationError> {
        match predicate {
            DataPredicate::Eq { value } => Ok(Self::Eq { value }),
            DataPredicate::Ne { value } => Ok(Self::Ne { value }),
            DataPredicate::Lt { value } => Ok(Self::Lt { value }),
            DataPredicate::Le { value } => Ok(Self::Le { value }),
            DataPredicate::Gt { value } => Ok(Self::Gt { value }),
            DataPredicate::Ge { value } => Ok(Self::Ge { value }),
            DataPredicate::Regex { pattern } => {
                let regex = Regex::new(&pattern).map_err(|error| {
                    EventFilterValidationError::InvalidRegex {
                        path: path.to_owned(),
                        pattern: pattern.clone(),
                        detail: error.to_string(),
                    }
                })?;
                Ok(Self::Regex { regex })
            }
            DataPredicate::InSet { values } => Ok(Self::InSet { values }),
            DataPredicate::Exists => Ok(Self::Exists),
        }
    }

    fn matches(&self, value: Option<&Value>) -> bool {
        match self {
            Self::Exists => value.is_some(),
            Self::Eq { value: expected } => value == Some(expected),
            Self::Ne { value: expected } => value.is_some_and(|actual| actual != expected),
            Self::Lt { value: expected } => {
                compare_values(value, expected).is_some_and(std::cmp::Ordering::is_lt)
            }
            Self::Le { value: expected } => {
                compare_values(value, expected).is_some_and(std::cmp::Ordering::is_le)
            }
            Self::Gt { value: expected } => {
                compare_values(value, expected).is_some_and(std::cmp::Ordering::is_gt)
            }
            Self::Ge { value: expected } => {
                compare_values(value, expected).is_some_and(std::cmp::Ordering::is_ge)
            }
            Self::Regex { regex } => value
                .and_then(Value::as_str)
                .is_some_and(|actual| regex.is_match(actual)),
            Self::InSet { values } => {
                value.is_some_and(|actual| values.iter().any(|item| item == actual))
            }
        }
    }
}

fn compare_values(value: Option<&Value>, expected: &Value) -> Option<std::cmp::Ordering> {
    let actual = value?;

    match (actual, expected) {
        (Value::Number(actual), Value::Number(expected)) => {
            actual.as_f64()?.partial_cmp(&expected.as_f64()?)
        }
        (Value::String(actual), Value::String(expected)) => Some(actual.cmp(expected)),
        _ => None,
    }
}
