//! Append-only Ledger provenance primitives.

pub mod append;
pub mod audit;
pub mod base_stamp;
pub mod batch_members;
pub mod checkpoint;
pub mod codec;
mod directory_store;
pub mod entry;
pub mod group_commit;
pub mod head_anchor;
pub mod kind;
pub mod merkle;
pub mod redaction;
pub mod reproduce;
pub mod stream_verify;
pub mod tombstone;
pub mod verify;

pub use append::{
    DirectoryLedgerStore, LedgerAppender, LedgerCfStore, LedgerRow, LedgerSnapshot,
    MemoryLedgerStore, PreparedLedgerEntry, reject_delete, reject_tombstone,
};
pub use audit::{
    AnswerTrace, AnswerTraceHop, AuditFilter, QuarantineLookup, QuarantineSet,
    answer_trace_from_entries, audit, entry_cx_mentions, get_answer_trace,
    get_answer_trace_from_snapshot, get_provenance, get_provenance_from_snapshot,
};
pub use base_stamp::{
    CALYX_LEDGER_BASE_STAMP_UNDECLARED, CoverageRule, SubjectShape, coverage_rule,
    require_base_stamp_declared,
};
pub use batch_members::{
    BATCH_MEMBERS_AUTHORITY, BATCH_MEMBERS_FIELD, BATCH_MEMBERS_VERSION, BatchMembers,
    CALYX_LEDGER_BATCH_MEMBERS_MALFORMED, MAX_ENUMERATED_BATCH_MEMBERS, MemberVerdict,
    declare_batch_members, read_batch_members,
};
pub use checkpoint::{
    CHECKPOINT_TAG, CheckpointConfig, CheckpointPayload, CheckpointScheduler,
    DEFAULT_CHECKPOINT_INTERVAL, OverlayLedgerStore,
};
pub use codec::{decode, decode_header, encode};
pub use entry::{ActorId, LedgerEntry, SubjectId, compute_entry_hash};
pub use group_commit::{
    DefaultLedgerHook, LedgerBatchRow, LedgerWriteBatch, StagedLedgerRow, WriteBatch, WriteOp,
    ingest_kind_for, ledger_batch_key,
};
pub use head_anchor::LedgerHeadAnchor;
pub use kind::{EntryKind, WriterStatus};
pub use merkle::{
    MERKLE_EMPTY_ROOT, MERKLE_SIGNING_DOMAIN, MerkleExportBundle, combine_hash, leaf_hash,
    merkle_root, merkle_root_of_hashes, sign_root, verify_signature,
};
pub use redaction::{MAX_UNCLASSIFIED_TOKEN_LEN, PayloadBuilder, RedactedInput, RedactionPolicy};
pub use reproduce::{
    CALYX_REPRODUCE_EVIDENCE_UNAVAILABLE, ForgeBackend, FusionMode, FusionWeights, HitRef,
    InlineInputResolver, QueryId, REPRODUCE_PAYLOAD_TAG, REPRODUCE_TOLERANCE, RecordedSlot,
    RemeasuredSlot, ReproduceContext, ReproduceInputResolver, ReproduceLensRegistry,
    ReproduceResult, SlotWeight, activate_forge_determinism, append_reproduce_entry,
    assert_reproduced, assert_within_tolerance, build_reproduce_context, lookup_frozen_lens,
    remeasure_slots, remeasure_slots_with_input_resolver, reproduce, reproduce_payload_bytes,
    reproduce_verdict, reproduce_verdict_with_input_resolver, reproduce_with_input_resolver,
    rerun_fusion,
};
pub use stream_verify::{AnchorDiscipline, StreamingChainVerifier, StreamingStart};
pub use tombstone::{
    ErasureScope, ErasureTombstone, find_tombstone, is_tombstoned, tombstone_from_entry,
    write_tombstone,
};
pub use verify::{
    DecodedLedgerSnapshot, VerifyResult, verify_chain, verify_decoded_snapshot, verify_snapshot,
};
