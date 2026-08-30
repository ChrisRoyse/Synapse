//! Operator adjudication of a permanently damaged raw-commitment cohort seal.
//!
//! A cohort seal lives in the payload of an append-only Ledger entry, so a seal
//! torn by a crash can never be rewritten: the entry hash covers the payload and
//! every later entry chains to it. Repairing one in place would break the chain
//! it exists to protect.
//!
//! Without a way to record that fact, one damaged cohort is permanent and total.
//! The verifier latches on its first seal failure and skips every seal after it,
//! so a single torn seal stops the vault being verified at all - not only for
//! that cohort, but for everything written since, forever.
//!
//! An adjudication is the append-only answer. An operator appends an `Admin`
//! entry naming one seal and the exact failure diagnostic they saw. The verifier
//! then treats that one cohort as *unverified* - never as verified - and carries
//! on checking the rest. The damage is not erased: it stays in the chain, it
//! stays in every readback, and the vault's verdict stays out of `verified`.
//!
//! The guard against abuse is the digest. An adjudication binds to one Ledger
//! sequence *and* the byte-exact diagnostic for its damage. Any change in the
//! damage changes the digest, the adjudication stops matching, and the verifier
//! fails closed exactly as it does today.

use core::fmt::Write as _;

use calyx_core::{CalyxError, Result};
use calyx_ledger::{EntryKind, LedgerEntryRef};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

/// Ledger subject reserved for cohort-seal adjudications.
pub(crate) const SEAL_ADJUDICATION_SUBJECT: &[u8] =
    b"calyx-aster/raw-commitment-seal-adjudication/v1";

/// Payload discriminator, matching the house style for `Admin` governance rows.
const SEAL_ADJUDICATION_EVENT: &str = "RAW_COMMITMENT_SEAL_ADJUDICATED";

/// Domain separator for the diagnostic digest.
const DIGEST_DOMAIN: &[u8] = b"calyx-aster/raw-commitment-seal-adjudication/digest/v1";

/// Upper bound on adjudications a single vault may carry.
///
/// Adjudication is for isolated, individually reviewed damage. A vault needing
/// more than this many is not a vault with a torn seal; it is a vault to
/// restore, and the verifier must keep saying so.
pub(crate) const MAX_SEAL_ADJUDICATIONS: usize = 64;

/// Longest diagnostic text retained in the durable record.
const MAX_DIAGNOSTIC_BYTES: usize = 4096;

/// Longest operator reason retained in the durable record.
pub(crate) const MAX_REASON_BYTES: usize = 1024;

/// One operator-authorized exception for a single damaged cohort seal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SealAdjudication {
    /// Ledger sequence of the `batch_commitment` entry holding the damaged seal.
    pub ledger_seq: u64,
    /// Digest binding this adjudication to one exact failure diagnostic.
    pub diagnostic_sha256: [u8; 32],
    /// The diagnostic itself, so the record is readable without the verifier.
    pub diagnostic: String,
    /// Why an operator accepted this cohort as permanently unverifiable.
    pub reason: String,
}

/// Binds a failure diagnostic to the seal it describes.
///
/// The Ledger sequence is inside the digest, so an adjudication cannot be moved
/// onto a different seal that happens to fail the same way.
#[must_use]
pub(crate) fn diagnostic_digest(ledger_seq: u64, diagnostic: &str) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(DIGEST_DOMAIN);
    hasher.update([0]);
    hasher.update(ledger_seq.to_be_bytes());
    hasher.update([0]);
    hasher.update(diagnostic.as_bytes());
    let digest = hasher.finalize();
    let mut output = [0_u8; 32];
    output.copy_from_slice(&digest);
    output
}

#[must_use]
pub(crate) fn hex32(bytes: &[u8; 32]) -> String {
    let mut output = String::with_capacity(64);
    for byte in bytes {
        let _ = write!(output, "{byte:02x}");
    }
    output
}

/// Builds the durable `Admin` payload for one adjudication.
pub(crate) fn encode_adjudication(
    ledger_seq: u64,
    diagnostic: &str,
    reason: &str,
) -> Result<Vec<u8>> {
    if ledger_seq == 0 {
        return Err(CalyxError::aster_corrupt_shard(
            "raw-commitment seal adjudication requires a non-zero Ledger sequence",
        ));
    }
    if diagnostic.is_empty() || diagnostic.len() > MAX_DIAGNOSTIC_BYTES {
        return Err(CalyxError::aster_corrupt_shard(format!(
            "raw-commitment seal adjudication diagnostic must be 1..={MAX_DIAGNOSTIC_BYTES} bytes; actual={}",
            diagnostic.len()
        )));
    }
    if reason.trim().is_empty() || reason.len() > MAX_REASON_BYTES {
        return Err(CalyxError::aster_corrupt_shard(format!(
            "raw-commitment seal adjudication requires a non-empty operator reason of at most {MAX_REASON_BYTES} bytes; actual={}",
            reason.len()
        )));
    }
    let digest = diagnostic_digest(ledger_seq, diagnostic);
    serde_json::to_vec(&json!({
        "event": SEAL_ADJUDICATION_EVENT,
        "ledger_seq": ledger_seq,
        "diagnostic_sha256": hex32(&digest),
        "diagnostic": diagnostic,
        "reason": reason,
    }))
    .map_err(|error| {
        CalyxError::aster_corrupt_shard(format!(
            "encode raw-commitment seal adjudication payload: {error}"
        ))
    })
}

/// Decodes an adjudication from a Ledger entry, or `None` when the entry is not
/// one.
///
/// A malformed payload on the reserved subject is an error rather than a skip:
/// the verifier must never silently ignore a row that claims to grant an
/// exception.
pub(crate) fn ledger_adjudication_ref(
    entry: &LedgerEntryRef<'_>,
) -> Result<Option<SealAdjudication>> {
    if entry.kind() != EntryKind::Admin || !entry.subject_is_query(SEAL_ADJUDICATION_SUBJECT) {
        return Ok(None);
    }
    let value: Value = serde_json::from_slice(entry.payload()).map_err(|error| {
        CalyxError::aster_corrupt_shard(format!(
            "raw-commitment seal adjudication at Ledger entry {} is not decodable JSON: {error}",
            entry.seq()
        ))
    })?;
    let corrupt = |detail: &str| {
        CalyxError::aster_corrupt_shard(format!(
            "raw-commitment seal adjudication at Ledger entry {}: {detail}",
            entry.seq()
        ))
    };
    if value.get("event").and_then(Value::as_str) != Some(SEAL_ADJUDICATION_EVENT) {
        return Err(corrupt(
            "payload on the reserved adjudication subject is not an adjudication event",
        ));
    }
    let ledger_seq = value
        .get("ledger_seq")
        .and_then(Value::as_u64)
        .ok_or_else(|| corrupt("ledger_seq is missing or not a u64"))?;
    let diagnostic = value
        .get("diagnostic")
        .and_then(Value::as_str)
        .ok_or_else(|| corrupt("diagnostic is missing"))?
        .to_owned();
    let recorded_digest = value
        .get("diagnostic_sha256")
        .and_then(Value::as_str)
        .ok_or_else(|| corrupt("diagnostic_sha256 is missing"))?;
    let reason = value
        .get("reason")
        .and_then(Value::as_str)
        .ok_or_else(|| corrupt("reason is missing"))?
        .to_owned();
    let digest = diagnostic_digest(ledger_seq, &diagnostic);
    // The record must be self-consistent. A payload whose stored digest does not
    // commit to its own stored diagnostic proves nothing and grants nothing.
    if hex32(&digest) != recorded_digest {
        return Err(corrupt(
            "diagnostic_sha256 does not commit to the recorded ledger_seq and diagnostic",
        ));
    }
    Ok(Some(SealAdjudication {
        ledger_seq,
        diagnostic_sha256: digest,
        diagnostic,
        reason,
    }))
}

impl SealAdjudication {
    /// True only for the one seal and the one byte-exact failure this
    /// adjudication was authorized against.
    #[must_use]
    pub(crate) fn covers(&self, ledger_seq: u64, diagnostic: &str) -> bool {
        self.ledger_seq == ledger_seq
            && self.diagnostic_sha256 == diagnostic_digest(ledger_seq, diagnostic)
    }
}

impl<C> crate::vault::AsterVault<C>
where
    C: calyx_core::Clock,
{
    /// Records that one damaged cohort seal is permanently unverifiable.
    ///
    /// This never repairs, rewrites, or hides anything. It appends one `Admin`
    /// entry to the same append-only chain that carries the damage, after which
    /// the verifier stops treating that one cohort as a reason to abandon the
    /// whole vault - and resumes checking every seal written after it.
    ///
    /// `diagnostic` must be the byte-exact failure text the verifier reported.
    /// Nothing else will match at verification time, so an adjudication cannot
    /// outlive the exact damage it was authorized against.
    pub fn adjudicate_raw_commitment_seal(
        &self,
        ledger_seq: u64,
        diagnostic: &str,
        reason: &str,
    ) -> calyx_core::Result<calyx_core::LedgerRef> {
        let payload = encode_adjudication(ledger_seq, diagnostic, reason)?;
        self.append_ledger_entry(
            calyx_ledger::EntryKind::Admin,
            calyx_ledger::SubjectId::Query(SEAL_ADJUDICATION_SUBJECT.to_vec()),
            payload,
            calyx_ledger::ActorId::System,
        )
    }
}
