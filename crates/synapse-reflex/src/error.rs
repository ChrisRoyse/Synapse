use synapse_core::{ReflexId, error_codes};
use thiserror::Error;

pub type ReflexResult<T> = Result<T, ReflexError>;

/// Reflex failures with stable Synapse error codes.
#[derive(Clone, Debug, Eq, PartialEq, Error)]
pub enum ReflexError {
    #[error("reflex cap reached: {detail}")]
    CapReached { detail: String },
    #[error("reflex kind invalid: {detail}")]
    KindInvalid { detail: String },
    #[error("reflex params invalid: {detail}")]
    ParamsInvalid { detail: String },
    #[error("reflex target invalid: {detail}")]
    TargetInvalid { detail: String },
    #[error("reflex filter invalid: {detail}")]
    FilterInvalid { detail: String },
    #[error("reflex priority invalid: {detail}")]
    PriorityInvalid { detail: String },
    #[error("reflex tick late by {late_by_us} us")]
    TickLate { late_by_us: u64 },
    #[error("reflex track lost: {reflex_id}")]
    TrackLost { reflex_id: ReflexId },
    #[error("reflex starved: {reflex_id}")]
    Starved { reflex_id: ReflexId },
    #[error("reflex action permission denied: {reflex_id}: {detail}")]
    ActionPermissionDenied { reflex_id: ReflexId, detail: String },
    #[error("reflex disabled by operator: {detail}")]
    DisabledByOperator { detail: String },
    #[error("reflex lifetime expired: {reflex_id}")]
    LifetimeExpired { reflex_id: ReflexId },
    #[error("reflex recursion limit reached: {reflex_id}")]
    RecursionLimit { reflex_id: ReflexId },
}

impl ReflexError {
    /// Returns the stable Synapse error code for this reflex failure.
    #[must_use]
    #[tracing::instrument(skip_all, fields(reflex_error = ?self))]
    pub fn code(&self) -> &'static str {
        match self {
            Self::CapReached { .. } => error_codes::REFLEX_CAP_REACHED,
            Self::KindInvalid { .. } => error_codes::REFLEX_KIND_INVALID,
            Self::ParamsInvalid { .. } => error_codes::REFLEX_PARAMS_INVALID,
            Self::TargetInvalid { .. } => error_codes::REFLEX_TARGET_INVALID,
            Self::FilterInvalid { .. } => error_codes::REFLEX_FILTER_INVALID,
            Self::PriorityInvalid { .. } => error_codes::REFLEX_PRIORITY_INVALID,
            Self::TickLate { .. } => error_codes::REFLEX_TICK_LATE,
            Self::TrackLost { .. } => error_codes::REFLEX_TRACK_LOST,
            Self::Starved { .. } => error_codes::REFLEX_STARVED,
            Self::ActionPermissionDenied { .. } => error_codes::REFLEX_ACTION_PERMISSION_DENIED,
            Self::DisabledByOperator { .. } => error_codes::REFLEX_DISABLED_BY_OPERATOR,
            Self::LifetimeExpired { .. } => error_codes::REFLEX_LIFETIME_EXPIRED,
            Self::RecursionLimit { .. } => error_codes::REFLEX_RECURSION_LIMIT,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reflex_error_codes_cover_every_variant() {
        let cases = [
            (
                ReflexError::CapReached {
                    detail: "cap 32".to_owned(),
                },
                error_codes::REFLEX_CAP_REACHED,
            ),
            (
                ReflexError::KindInvalid {
                    detail: "nonsense".to_owned(),
                },
                error_codes::REFLEX_KIND_INVALID,
            ),
            (
                ReflexError::ParamsInvalid {
                    detail: "missing action".to_owned(),
                },
                error_codes::REFLEX_PARAMS_INVALID,
            ),
            (
                ReflexError::TargetInvalid {
                    detail: "track missing".to_owned(),
                },
                error_codes::REFLEX_TARGET_INVALID,
            ),
            (
                ReflexError::FilterInvalid {
                    detail: "unknown field".to_owned(),
                },
                error_codes::REFLEX_FILTER_INVALID,
            ),
            (
                ReflexError::PriorityInvalid {
                    detail: "priority overflow".to_owned(),
                },
                error_codes::REFLEX_PRIORITY_INVALID,
            ),
            (
                ReflexError::TickLate { late_by_us: 250 },
                error_codes::REFLEX_TICK_LATE,
            ),
            (
                ReflexError::TrackLost {
                    reflex_id: "reflex-track".to_owned(),
                },
                error_codes::REFLEX_TRACK_LOST,
            ),
            (
                ReflexError::Starved {
                    reflex_id: "reflex-starved".to_owned(),
                },
                error_codes::REFLEX_STARVED,
            ),
            (
                ReflexError::ActionPermissionDenied {
                    reflex_id: "reflex-denied".to_owned(),
                    detail: "unknown scope".to_owned(),
                },
                error_codes::REFLEX_ACTION_PERMISSION_DENIED,
            ),
            (
                ReflexError::DisabledByOperator {
                    detail: "panic hotkey".to_owned(),
                },
                error_codes::REFLEX_DISABLED_BY_OPERATOR,
            ),
            (
                ReflexError::LifetimeExpired {
                    reflex_id: "reflex-expired".to_owned(),
                },
                error_codes::REFLEX_LIFETIME_EXPIRED,
            ),
            (
                ReflexError::RecursionLimit {
                    reflex_id: "reflex-recursion".to_owned(),
                },
                error_codes::REFLEX_RECURSION_LIMIT,
            ),
        ];

        let observed = cases
            .iter()
            .map(|(error, expected)| (error.code(), *expected))
            .collect::<Vec<_>>();

        for (actual, expected) in observed {
            assert_eq!(actual, expected);
        }
    }
}
