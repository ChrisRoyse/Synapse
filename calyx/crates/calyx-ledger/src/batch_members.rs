//! The membership declaration a batch ledger entry carries about the
//! constellations whose Base-row provenance it stamps.
//!
//! # The question this exists to answer
//!
//! "Which constellations does ledger seq N cover?"
//!
//! Before #2096 the multi-constellation grounding anchor batch answered it with
//! nothing usable: subject `Query(b"synapse.grounding_anchor.multi.v1")` names
//! no constellation, and every payload field naming a member was a **hash of a
//! source-row key**, not a `CxId`. A live entry (seq 1067702) covered 559
//! transcript constellations and recovering that set required re-deriving cx ids
//! from source keys against the vault. A ledger that cannot answer questions
//! about itself is a log, not a ledger.
//!
//! # The contract
//!
//! A batch-stamping writer inserts one object under the top-level key
//! `batch_members` into its JSON payload:
//!
//! ```json
//! {
//!   "batch_members": {
//!     "v": 1,
//!     "total": 559,
//!     "listed": 559,
//!     "truncated": false,
//!     "cx_id": ["0e12...", "1a77...", "..."],
//!     "members_blake3": "<64 hex>"
//!   }
//! }
//! ```
//!
//! * `v` is the contract version. A reader that finds no `batch_members` object
//!   is looking at an entry minted before this contract and must keep the
//!   pre-#2096 trusting path — the ledger is append-only, so historical entries
//!   cannot be upgraded and must not be re-judged under a rule they predate.
//! * `cx_id` holds the listed members in sorted order, so the payload is a
//!   deterministic function of the batch and two writers committing the same
//!   batch produce byte-identical declarations.
//! * `members_blake3` digests the COMPLETE sorted member list (not just the
//!   listed prefix), so a membership set recovered by other means can be checked
//!   against the entry even when the list was truncated.
//! * `truncated` is explicit. Above [`MAX_ENUMERATED_BATCH_MEMBERS`] the list
//!   carries a deterministic sorted prefix, `listed` < `total` says so in the
//!   payload itself, and `authority` names where the complete set lives. A
//!   silent short list would be worse than no list: a reader would read
//!   "not a member" off an incomplete enumeration.
//!
//! The block is deliberately self-contained rather than merged into the
//! top-level `cx_id` convention that `calyx-aster`'s `batch_payload` already
//! uses, so declaring membership can never collide with, or be mistaken for, a
//! payload field the caller owns.
//!
//! The declaration lives here (rather than in `calyx-aster`) for the same reason
//! [`crate::base_stamp`] does: the writer that mints it and the reader that
//! verifies against it are in different crates and must not be free to drift.

use std::collections::BTreeSet;

use calyx_core::{CalyxError, CxId, Result};
use serde_json::{Map, Value};

/// Top-level payload key holding the membership declaration.
pub const BATCH_MEMBERS_FIELD: &str = "batch_members";

/// Current contract version. A reader must refuse a version it does not know
/// rather than guess at the shape behind it.
pub const BATCH_MEMBERS_VERSION: u64 = 1;

/// Largest member list embedded in a ledger payload.
///
/// A `CxId` renders as 32 hex characters, so a full list costs about 36 bytes
/// per member: this bound puts a worst-case declaration around 150 KiB, which is
/// well inside what a ledger entry payload carries (the length prefix is a u32)
/// while refusing to let one pathological batch dominate the Ledger CF. The
/// largest batch observed in production is 559 members.
pub const MAX_ENUMERATED_BATCH_MEMBERS: usize = 4096;

/// Where the complete membership of a truncated declaration is authoritative.
///
/// The Base rows themselves: every constellation whose `provenance.seq` equals
/// this entry's seq is a member, by definition of what the stamp does. That set
/// can be checked against `members_blake3` once recovered.
pub const BATCH_MEMBERS_AUTHORITY: &str = "aster.base_row_provenance_seq";

/// A batch payload could not carry, or could not be read as, a membership
/// declaration.
pub const CALYX_LEDGER_BATCH_MEMBERS_MALFORMED: &str = "CALYX_LEDGER_BATCH_MEMBERS_MALFORMED";

const MALFORMED_REMEDIATION: &str = "a batch ledger entry that stamps Base-row provenance must carry a JSON object payload so its \
     membership can be declared under `batch_members`; fix the writer's payload. The ledger row \
     itself is intact and must not be restored from backup";

/// What a ledger entry payload declares about the constellations it covers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BatchMembers {
    /// The payload carries no `batch_members` object. Either the entry predates
    /// #2096, or its writer is not a batch stamper. Membership is not decidable
    /// from the entry and the reader must fall back to the binding established
    /// by the entry hash and chain link.
    Undeclared,
    /// The declaration enumerates every member.
    Complete {
        total: usize,
        members: BTreeSet<String>,
        members_blake3: String,
    },
    /// The declaration enumerates a deterministic sorted prefix and says so.
    /// A hit inside `members` is positively covered; a hit outside it is
    /// undecided, NOT excluded.
    Truncated {
        total: usize,
        members: BTreeSet<String>,
        members_blake3: String,
        authority: String,
    },
}

impl BatchMembers {
    /// The complete member count the batch declared, when it declared one.
    pub const fn total(&self) -> Option<usize> {
        match self {
            Self::Undeclared => None,
            Self::Complete { total, .. } | Self::Truncated { total, .. } => Some(*total),
        }
    }

    /// Digest over the complete sorted member list, when declared.
    pub fn members_blake3(&self) -> Option<&str> {
        match self {
            Self::Undeclared => None,
            Self::Complete { members_blake3, .. } | Self::Truncated { members_blake3, .. } => {
                Some(members_blake3.as_str())
            }
        }
    }
}

/// The decided relationship between a membership declaration and one hit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemberVerdict {
    /// The declaration lists this constellation.
    Listed,
    /// The declaration is complete and does not list this constellation, so it
    /// positively does not cover it.
    NotListed,
    /// No usable declaration, or a truncated one whose listed prefix does not
    /// reach this constellation. Nothing is decided either way.
    Undecided,
}

impl BatchMembers {
    /// Decides one constellation against this declaration.
    pub fn verdict(&self, cx_id: CxId) -> MemberVerdict {
        let target = cx_id.to_string();
        match self {
            Self::Undeclared => MemberVerdict::Undecided,
            Self::Complete { members, .. } => {
                if members.contains(&target) {
                    MemberVerdict::Listed
                } else {
                    MemberVerdict::NotListed
                }
            }
            Self::Truncated { members, .. } => {
                if members.contains(&target) {
                    MemberVerdict::Listed
                } else {
                    // The complete set is larger than what is listed, so absence
                    // from the prefix proves nothing. Saying `NotListed` here
                    // would turn a size bound into a false negative on a
                    // legitimately covered row.
                    MemberVerdict::Undecided
                }
            }
        }
    }
}

/// Inserts the membership declaration into a batch writer's JSON payload.
///
/// The returned bytes are the payload the entry is minted with. The declaration
/// is a pure function of `members`, so re-running the same batch produces the
/// same payload bytes.
///
/// # Errors
///
/// [`CALYX_LEDGER_BATCH_MEMBERS_MALFORMED`] when `payload` is not a JSON object
/// (a batch stamper must be able to declare its membership), when it already
/// carries a `batch_members` key (two declarations, neither authoritative), or
/// when `members` is empty (a batch that covers nothing must not be minted).
pub fn declare_batch_members(payload: &[u8], members: &[CxId]) -> Result<Vec<u8>> {
    let mut object = payload_object(payload)?;
    if object.contains_key(BATCH_MEMBERS_FIELD) {
        return Err(malformed(format!(
            "payload already carries a `{BATCH_MEMBERS_FIELD}` declaration; a batch entry must \
             have exactly one, minted by the writer that stamps the Base rows"
        )));
    }
    if members.is_empty() {
        return Err(malformed(
            "a batch membership declaration must name at least one constellation",
        ));
    }
    // Sorted + deduplicated: the same batch always renders the same bytes, and
    // the digest below is over a canonical sequence rather than over whatever
    // order the caller's rows happened to arrive in.
    let sorted = members
        .iter()
        .map(ToString::to_string)
        .collect::<BTreeSet<_>>();
    let total = sorted.len();
    let members_blake3 = members_digest(&sorted);
    let truncated = total > MAX_ENUMERATED_BATCH_MEMBERS;
    let listed = sorted
        .iter()
        .take(MAX_ENUMERATED_BATCH_MEMBERS)
        .cloned()
        .collect::<Vec<_>>();

    let mut declaration = Map::new();
    declaration.insert("v".to_owned(), Value::from(BATCH_MEMBERS_VERSION));
    declaration.insert("total".to_owned(), Value::from(total as u64));
    declaration.insert("listed".to_owned(), Value::from(listed.len() as u64));
    declaration.insert("truncated".to_owned(), Value::Bool(truncated));
    declaration.insert(
        "cx_id".to_owned(),
        Value::Array(listed.into_iter().map(Value::String).collect()),
    );
    declaration.insert("members_blake3".to_owned(), Value::String(members_blake3));
    if truncated {
        declaration.insert(
            "authority".to_owned(),
            Value::String(BATCH_MEMBERS_AUTHORITY.to_owned()),
        );
    }
    object.insert(BATCH_MEMBERS_FIELD.to_owned(), Value::Object(declaration));
    serde_json::to_vec(&Value::Object(object))
        .map_err(|error| malformed(format!("encode batch membership declaration: {error}")))
}

/// Reads the membership declaration a ledger entry payload carries.
///
/// A payload with no declaration reads as [`BatchMembers::Undeclared`] — the
/// pre-#2096 contract — and so does a payload that is not JSON at all, because
/// entries minted before this contract were never required to be JSON. Only a
/// payload that *has* a `batch_members` key and cannot be read as one is an
/// error: that is a writer defect, and guessing at it would be exactly the
/// silent degradation #2084 exists to stop.
///
/// # Errors
///
/// [`CALYX_LEDGER_BATCH_MEMBERS_MALFORMED`] when a present declaration is not a
/// readable object of a known version.
pub fn read_batch_members(payload: &[u8]) -> Result<BatchMembers> {
    let Ok(Value::Object(object)) = serde_json::from_slice::<Value>(payload) else {
        return Ok(BatchMembers::Undeclared);
    };
    let Some(raw) = object.get(BATCH_MEMBERS_FIELD) else {
        return Ok(BatchMembers::Undeclared);
    };
    let declaration = raw.as_object().ok_or_else(|| {
        malformed(format!(
            "`{BATCH_MEMBERS_FIELD}` is present but is not an object ({})",
            json_type_name(raw)
        ))
    })?;
    let version = declaration
        .get("v")
        .and_then(Value::as_u64)
        .ok_or_else(|| malformed("membership declaration carries no numeric `v` version"))?;
    if version != BATCH_MEMBERS_VERSION {
        return Err(malformed(format!(
            "membership declaration is contract version {version}, which this build does not \
             know (it reads version {BATCH_MEMBERS_VERSION})"
        )));
    }
    let total = usize::try_from(
        declaration
            .get("total")
            .and_then(Value::as_u64)
            .ok_or_else(|| malformed("membership declaration carries no numeric `total`"))?,
    )
    .map_err(|error| {
        malformed(format!(
            "membership `total` does not fit this platform: {error}"
        ))
    })?;
    let truncated = declaration
        .get("truncated")
        .and_then(Value::as_bool)
        .ok_or_else(|| malformed("membership declaration carries no boolean `truncated`"))?;
    let members_blake3 = declaration
        .get("members_blake3")
        .and_then(Value::as_str)
        .ok_or_else(|| malformed("membership declaration carries no `members_blake3` digest"))?
        .to_owned();
    let listed = declaration
        .get("cx_id")
        .and_then(Value::as_array)
        .ok_or_else(|| malformed("membership declaration carries no `cx_id` array"))?;
    let mut members = BTreeSet::new();
    for value in listed {
        let member = value.as_str().ok_or_else(|| {
            malformed(format!(
                "membership `cx_id` array holds a {} where a constellation id was declared",
                json_type_name(value)
            ))
        })?;
        members.insert(member.to_owned());
    }
    if members.len() > total {
        return Err(malformed(format!(
            "membership declaration lists {} distinct members but declares total={total}",
            members.len()
        )));
    }
    if truncated {
        let authority = declaration
            .get("authority")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                malformed(
                    "membership declaration is truncated but names no `authority` for the \
                     complete set",
                )
            })?
            .to_owned();
        return Ok(BatchMembers::Truncated {
            total,
            members,
            members_blake3,
            authority,
        });
    }
    if members.len() != total {
        return Err(malformed(format!(
            "membership declaration is not truncated but lists {} of {total} members",
            members.len()
        )));
    }
    Ok(BatchMembers::Complete {
        total,
        members,
        members_blake3,
    })
}

/// Digest over the canonical (sorted, newline-separated) complete member list.
fn members_digest(sorted: &BTreeSet<String>) -> String {
    let mut hasher = blake3::Hasher::new();
    for member in sorted {
        hasher.update(member.as_bytes());
        hasher.update(b"\n");
    }
    hasher.finalize().to_hex().to_string()
}

fn payload_object(payload: &[u8]) -> Result<Map<String, Value>> {
    if payload.is_empty() {
        return Ok(Map::new());
    }
    match serde_json::from_slice::<Value>(payload) {
        Ok(Value::Object(object)) => Ok(object),
        Ok(other) => Err(malformed(format!(
            "batch ledger payload is a JSON {} where an object was required so membership could \
             be declared",
            json_type_name(&other)
        ))),
        Err(error) => Err(malformed(format!(
            "batch ledger payload is not JSON, so membership cannot be declared in it: {error}"
        ))),
    }
}

fn malformed(message: impl Into<String>) -> CalyxError {
    CalyxError {
        code: CALYX_LEDGER_BATCH_MEMBERS_MALFORMED,
        message: message.into(),
        remediation: MALFORMED_REMEDIATION,
    }
}

fn json_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}
