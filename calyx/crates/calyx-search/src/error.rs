//! `calyx-search` error type. A per-crate error that wraps the canonical
//! [`CalyxError`] catalog plus two local conditions (I/O, usage) that have no
//! catalog entry — mirrors the wire contract so a caller can map it onto its own
//! surface (CLI envelope, HTTP error envelope) without losing the structured
//! `code`. No silent fallback: every failure is explicit.
use calyx_core::CalyxError;

#[derive(Debug, Clone)]
pub enum SearchError {
    /// A structured catalog error carried verbatim.
    Calyx(CalyxError),
    /// An OS/I/O or codec failure with no catalog entry.
    Io(String),
    /// A caller-misuse failure (bad filter JSON, path outside root).
    Usage(String),
}

/// Internal alias so moved modules keep using `crate::error::{CliError, CliResult}`.
pub use SearchError as CliError;
pub type CliResult<T = ()> = core::result::Result<T, SearchError>;

impl SearchError {
    pub fn usage(message: impl Into<String>) -> Self {
        Self::Usage(message.into())
    }
    pub fn io(message: impl Into<String>) -> Self {
        Self::Io(message.into())
    }

    /// Stable wire code (mirrors the CLI envelope so callers can dispatch).
    pub fn code(&self) -> &'static str {
        match self {
            Self::Calyx(error) => error.code,
            Self::Io(_) => "CALYX_CLI_IO_ERROR",
            Self::Usage(_) => "CALYX_CLI_USAGE_ERROR",
        }
    }

    /// The concrete failure detail.
    pub fn message(&self) -> &str {
        match self {
            Self::Calyx(error) => &error.message,
            Self::Io(message) | Self::Usage(message) => message,
        }
    }

    /// The catalog error's own remediation, when this failure has one.
    ///
    /// `{code, message, remediation}` is the whole wire contract, and the
    /// remediation is the part that says what to *do*. Without this accessor a
    /// caller mapping a `SearchError` onto its own surface had nothing to
    /// forward, so it substituted a generic sentence and the specific fix was
    /// silently lost (#1909) — a fail-closed error that no longer explains how
    /// to close it.
    ///
    /// `None` for the two local variants that have no catalog entry, so a
    /// caller can tell "this failure carries no remediation" apart from "the
    /// remediation is empty" and fall back deliberately.
    pub fn remediation(&self) -> Option<&'static str> {
        match self {
            Self::Calyx(error) => Some(error.remediation),
            Self::Io(_) | Self::Usage(_) => None,
        }
    }
}

impl std::fmt::Display for SearchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Calyx(error) => write!(f, "{error}"),
            Self::Io(message) | Self::Usage(message) => write!(f, "{message}"),
        }
    }
}
impl std::error::Error for SearchError {}

impl From<CalyxError> for SearchError {
    fn from(error: CalyxError) -> Self {
        Self::Calyx(error)
    }
}
impl From<std::io::Error> for SearchError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error.to_string())
    }
}
impl From<serde_json::Error> for SearchError {
    fn from(error: serde_json::Error) -> Self {
        Self::Io(error.to_string())
    }
}

/// Boundary conversion for callers that speak the `CalyxError` catalog.
impl From<SearchError> for CalyxError {
    fn from(error: SearchError) -> Self {
        match error {
            SearchError::Calyx(catalog) => catalog,
            SearchError::Io(message) | SearchError::Usage(message) => {
                CalyxError::stale_derived(message)
            }
        }
    }
}
