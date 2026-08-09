use calyx_ledger::{ActorId, EntryKind, SubjectId};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest as _, Sha256};

use crate::{SynapseCalyxError, SynapseCalyxVault};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SynapseCalyxAutonomyDecisionReadback {
    pub routine_id: String,
    pub ledger_seq: u64,
    pub ledger_hash: String,
    pub payload_sha256: String,
}

impl SynapseCalyxVault {
    /// Appends one proactive-autonomy decision to the native provenance chain.
    /// No decision may be released or applied unless this append succeeds.
    ///
    /// # Errors
    ///
    /// Returns a structured error when the decision is invalid or its provenance
    /// payload cannot be encoded and durably appended.
    pub fn append_autonomy_decision(
        &self,
        routine_id: &str,
        decision: &Value,
    ) -> Result<SynapseCalyxAutonomyDecisionReadback, SynapseCalyxError> {
        let routine_id = routine_id.trim();
        if routine_id.is_empty() {
            return Err(SynapseCalyxError::new(
                "SYNAPSE_CALYX_AUTONOMY_DECISION_INVALID",
                "autonomy decision routine_id is blank",
                "supply the exact persisted routine id before making an autonomy decision",
            ));
        }
        if !decision.is_object() {
            return Err(SynapseCalyxError::new(
                "SYNAPSE_CALYX_AUTONOMY_DECISION_INVALID",
                "autonomy decision payload must be a JSON object",
                "supply a structured decision with outcome, predicates, and evidence",
            ));
        }
        let payload = serde_json::to_vec(&serde_json::json!({
            "decision": decision,
        }))
        .map_err(|error| {
            SynapseCalyxError::new(
                "SYNAPSE_CALYX_AUTONOMY_DECISION_ENCODE_FAILED",
                format!("encode autonomy decision payload: {error}"),
                "repair the structured, secret-free autonomy decision before retrying",
            )
        })?;
        let payload_sha256 = hex(&Sha256::digest(&payload));
        let ledger_ref = self
            .vault
            .append_ledger_entry(
                EntryKind::Policy,
                SubjectId::Query(routine_id.as_bytes().to_vec()),
                payload,
                ActorId::Service("synapse-autonomy".to_owned()),
            )
            .map_err(|error| {
                SynapseCalyxError::from_calyx("append autonomy decision policy ledger", &error)
            })?;
        Ok(SynapseCalyxAutonomyDecisionReadback {
            routine_id: routine_id.to_owned(),
            ledger_seq: ledger_ref.seq,
            ledger_hash: hex(&ledger_ref.hash),
            payload_sha256,
        })
    }
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;

    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        })
}
