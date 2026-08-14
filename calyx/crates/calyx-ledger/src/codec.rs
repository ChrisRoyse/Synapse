//! Deterministic binary codec for ledger entries.

use calyx_core::{CalyxError, CxId, LensId, Result};

use crate::entry::{
    ActorId, HASH_BYTES, LedgerEntry, SubjectId, TAG_AGENT, TAG_CX, TAG_GUARD, TAG_KERNEL,
    TAG_LENS, TAG_QUERY, TAG_SERVICE, TAG_SYSTEM,
};
use crate::kind::EntryKind;

const SEQ_LEN: usize = 8;
const KIND_OFFSET: usize = SEQ_LEN + HASH_BYTES;
const HEADER_LEN: usize = KIND_OFFSET;

/// Borrowed, fully validated view of one encoded ledger entry.
///
/// Whole-chain verification needs every field for canonical hashing but never
/// needs to own any of them. Keeping the slices tied to the caller's row buffer
/// prevents a second payload-sized allocation for every historical entry.
pub struct LedgerEntryRef<'a> {
    pub(crate) seq: u64,
    pub(crate) prev_hash: [u8; HASH_BYTES],
    pub(crate) kind: EntryKind,
    pub(crate) subject_tag: u8,
    pub(crate) subject_bytes: &'a [u8],
    pub(crate) payload: &'a [u8],
    pub(crate) actor_tag: u8,
    pub(crate) actor_bytes: &'a [u8],
    pub(crate) ts: u64,
    pub(crate) entry_hash: [u8; HASH_BYTES],
}

impl LedgerEntryRef<'_> {
    pub const fn seq(&self) -> u64 {
        self.seq
    }

    pub const fn kind(&self) -> EntryKind {
        self.kind
    }

    pub fn subject_is_query(&self, expected: &[u8]) -> bool {
        self.subject_tag == TAG_QUERY && self.subject_bytes == expected
    }

    pub const fn payload(&self) -> &[u8] {
        self.payload
    }

    pub fn verify(&self) -> bool {
        self.entry_hash
            == crate::entry::compute_entry_hash_slices(
                self.seq,
                &self.prev_hash,
                self.kind,
                self.subject_tag,
                self.subject_bytes,
                self.payload,
                self.actor_tag,
                self.actor_bytes,
                self.ts,
            )
    }
}

/// Encodes a ledger entry with a stable, padding-free binary layout.
pub fn encode(entry: &LedgerEntry) -> Vec<u8> {
    let subject = entry.subject.wire_bytes();
    let actor = entry.actor.wire_bytes();
    assert!(subject.len() <= u16::MAX as usize, "subject id too long");
    assert!(actor.len() <= u16::MAX as usize, "actor id too long");
    assert!(entry.payload.len() <= u32::MAX as usize, "payload too long");

    let mut out = Vec::with_capacity(encoded_len(entry, subject.len(), actor.len()));
    out.extend_from_slice(&entry.seq.to_be_bytes());
    out.extend_from_slice(&entry.prev_hash);
    out.push(entry.kind.wire_code());
    out.push(entry.subject.wire_tag());
    out.extend_from_slice(&(subject.len() as u16).to_be_bytes());
    out.extend_from_slice(&subject);
    out.extend_from_slice(&(entry.payload.len() as u32).to_be_bytes());
    out.extend_from_slice(&entry.payload);
    out.push(entry.actor.wire_tag());
    out.extend_from_slice(&(actor.len() as u16).to_be_bytes());
    out.extend_from_slice(actor);
    out.extend_from_slice(&entry.ts.to_be_bytes());
    out.extend_from_slice(&entry.entry_hash);
    out
}

/// Decodes a ledger entry and verifies its embedded hash.
pub fn decode(bytes: &[u8]) -> Result<LedgerEntry> {
    let entry = decode_unchecked(bytes)?;
    if entry.verify() {
        Ok(entry)
    } else {
        Err(corrupt(format!(
            "ledger entry seq {} hash mismatch",
            entry.seq
        )))
    }
}

/// Returns a borrowed, structurally and cryptographically validated entry.
///
/// The returned view owns no subject, payload, or actor allocation. It is the
/// read path for whole-history classifiers that need trusted fields but do not
/// need to retain the entry after advancing their storage cursor.
pub fn decode_ref(bytes: &[u8]) -> Result<LedgerEntryRef<'_>> {
    let entry = decode_ref_unchecked(bytes)?;
    if entry.verify() {
        Ok(entry)
    } else {
        Err(corrupt(format!(
            "ledger entry seq {} hash mismatch",
            entry.seq
        )))
    }
}

pub(crate) fn decode_unchecked(bytes: &[u8]) -> Result<LedgerEntry> {
    let entry = decode_ref_unchecked(bytes)?;
    let subject = decode_subject(entry.subject_tag, entry.subject_bytes)?;
    let actor = decode_actor(entry.actor_tag, entry.actor_bytes)?;
    Ok(LedgerEntry {
        seq: entry.seq,
        prev_hash: entry.prev_hash,
        kind: entry.kind,
        subject,
        payload: entry.payload.to_vec(),
        actor,
        ts: entry.ts,
        entry_hash: entry.entry_hash,
    })
}

pub(crate) fn decode_ref_unchecked(bytes: &[u8]) -> Result<LedgerEntryRef<'_>> {
    let mut cursor = Cursor::new(bytes);
    let seq = cursor.u64("seq")?;
    let prev_hash = cursor.hash("prev_hash")?;
    let kind_code = cursor.u8("kind")?;
    let kind = EntryKind::from_wire_code(kind_code)
        .ok_or_else(|| corrupt(format!("invalid ledger kind code {kind_code}")))?;
    let subject_tag = cursor.u8("subject_tag")?;
    let subject_len = cursor.u16("subject_len")? as usize;
    let subject_bytes = cursor.bytes(subject_len, "subject_bytes")?;
    validate_subject(subject_tag, subject_bytes)?;
    let payload_len = cursor.u32("payload_len")? as usize;
    let payload = cursor.bytes(payload_len, "payload")?;
    let actor_tag = cursor.u8("actor_tag")?;
    let actor_len = cursor.u16("actor_len")? as usize;
    let actor_bytes = cursor.bytes(actor_len, "actor_bytes")?;
    validate_actor(actor_tag, actor_bytes)?;
    let ts = cursor.u64("ts")?;
    let entry_hash = cursor.hash("entry_hash")?;
    cursor.finish()?;

    Ok(LedgerEntryRef {
        seq,
        prev_hash,
        kind,
        subject_tag,
        subject_bytes,
        payload,
        actor_tag,
        actor_bytes,
        ts,
        entry_hash,
    })
}

/// Decodes only `seq` and `prev_hash` for fast chain-link checks.
pub fn decode_header(bytes: &[u8]) -> Result<(u64, [u8; HASH_BYTES])> {
    if bytes.len() < HEADER_LEN {
        return Err(corrupt(format!(
            "ledger header requires {HEADER_LEN} bytes, got {}",
            bytes.len()
        )));
    }
    let seq = u64::from_be_bytes(bytes[0..SEQ_LEN].try_into().expect("seq length checked"));
    let prev_hash = bytes[SEQ_LEN..HEADER_LEN]
        .try_into()
        .expect("hash length checked");
    Ok((seq, prev_hash))
}

fn encoded_len(entry: &LedgerEntry, subject_len: usize, actor_len: usize) -> usize {
    SEQ_LEN
        + HASH_BYTES
        + 1
        + 1
        + 2
        + subject_len
        + 4
        + entry.payload.len()
        + 1
        + 2
        + actor_len
        + 8
        + HASH_BYTES
}

fn decode_subject(tag: u8, bytes: &[u8]) -> Result<SubjectId> {
    match tag {
        TAG_CX => Ok(SubjectId::Cx(CxId::from_bytes(copy_16(bytes, "cx")?))),
        TAG_LENS => Ok(SubjectId::Lens(LensId::from_bytes(copy_16(bytes, "lens")?))),
        TAG_KERNEL => Ok(SubjectId::Kernel(bytes.to_vec())),
        TAG_GUARD => Ok(SubjectId::Guard(bytes.to_vec())),
        TAG_QUERY => Ok(SubjectId::Query(bytes.to_vec())),
        _ => Err(corrupt(format!("invalid subject tag {tag}"))),
    }
}

fn validate_subject(tag: u8, bytes: &[u8]) -> Result<()> {
    match tag {
        TAG_CX => copy_16(bytes, "cx").map(|_| ()),
        TAG_LENS => copy_16(bytes, "lens").map(|_| ()),
        TAG_KERNEL | TAG_GUARD | TAG_QUERY => Ok(()),
        _ => Err(corrupt(format!("invalid subject tag {tag}"))),
    }
}

fn decode_actor(tag: u8, bytes: &[u8]) -> Result<ActorId> {
    let value = String::from_utf8(bytes.to_vec())
        .map_err(|_| corrupt(format!("actor tag {tag} is not utf8")))?;
    match tag {
        TAG_AGENT => Ok(ActorId::Agent(value)),
        TAG_SERVICE => Ok(ActorId::Service(value)),
        TAG_SYSTEM if bytes.is_empty() => Ok(ActorId::System),
        TAG_SYSTEM => Err(corrupt("system actor must have zero actor bytes")),
        _ => Err(corrupt(format!("invalid actor tag {tag}"))),
    }
}

fn validate_actor(tag: u8, bytes: &[u8]) -> Result<()> {
    match tag {
        TAG_AGENT | TAG_SERVICE => std::str::from_utf8(bytes)
            .map(|_| ())
            .map_err(|_| corrupt(format!("actor tag {tag} is not utf8"))),
        TAG_SYSTEM if bytes.is_empty() => Ok(()),
        TAG_SYSTEM => Err(corrupt("system actor must have zero actor bytes")),
        _ => Err(corrupt(format!("invalid actor tag {tag}"))),
    }
}

fn copy_16(bytes: &[u8], label: &str) -> Result<[u8; 16]> {
    bytes.try_into().map_err(|_| {
        corrupt(format!(
            "{label} subject requires 16 bytes, got {}",
            bytes.len()
        ))
    })
}

fn corrupt(message: impl Into<String>) -> CalyxError {
    CalyxError::ledger_corrupt(message)
}

struct Cursor<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Cursor<'a> {
    const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn u8(&mut self, label: &str) -> Result<u8> {
        let bytes = self.bytes(1, label)?;
        Ok(bytes[0])
    }

    fn u16(&mut self, label: &str) -> Result<u16> {
        let bytes = self.bytes(2, label)?;
        Ok(u16::from_be_bytes(
            bytes.try_into().expect("u16 length checked"),
        ))
    }

    fn u32(&mut self, label: &str) -> Result<u32> {
        let bytes = self.bytes(4, label)?;
        Ok(u32::from_be_bytes(
            bytes.try_into().expect("u32 length checked"),
        ))
    }

    fn u64(&mut self, label: &str) -> Result<u64> {
        let bytes = self.bytes(8, label)?;
        Ok(u64::from_be_bytes(
            bytes.try_into().expect("u64 length checked"),
        ))
    }

    fn hash(&mut self, label: &str) -> Result<[u8; HASH_BYTES]> {
        let bytes = self.bytes(HASH_BYTES, label)?;
        Ok(bytes.try_into().expect("hash length checked"))
    }

    fn bytes(&mut self, len: usize, label: &str) -> Result<&'a [u8]> {
        let end = self
            .offset
            .checked_add(len)
            .ok_or_else(|| corrupt(format!("{label} length overflows")))?;
        if end > self.bytes.len() {
            return Err(corrupt(format!(
                "{label} extends past buffer at {}..{} of {}",
                self.offset,
                end,
                self.bytes.len()
            )));
        }
        let out = &self.bytes[self.offset..end];
        self.offset = end;
        Ok(out)
    }

    fn finish(&self) -> Result<()> {
        if self.offset == self.bytes.len() {
            Ok(())
        } else {
            Err(corrupt(format!(
                "ledger entry has {} trailing bytes",
                self.bytes.len() - self.offset
            )))
        }
    }
}
