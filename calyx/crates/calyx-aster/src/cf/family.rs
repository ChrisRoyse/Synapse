//! Column-family identity and on-disk names.

use calyx_core::SlotId;

/// Per-slot column family flavor.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SlotFamilyKind {
    /// Quantized, active slot vector column.
    Quantized,
    /// Raw f32 sidecar used for cold-tier rescore/re-quantization.
    Raw,
}

/// Aster column families from PRD 04 section 4.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ColumnFamily {
    /// `CxId -> ConstellationHeader`.
    Base,
    /// `b"coll\0" || collection_name -> Collection metadata`.
    Collections,
    /// `0x01 || collection_id || pk_len || pk -> row`.
    Relational,
    /// `0x02 || collection_id || doc_id || path_segments -> leaf/tombstone`.
    Document,
    /// `0x03 || collection_id || ns || key_len || user_key -> version || expires_at || payload`.
    Kv,
    /// `0x04 || tag || collection_id || series || ts|window -> point f64 / rollup tuple`.
    TimeSeries,
    /// `0x05 || tag || collection_id || blob_id || chunk_idx -> chunk / manifest`.
    Blob,
    /// Per-slot vector column, either quantized or raw sidecar.
    Slot { slot: SlotId, kind: SlotFamilyKind },
    /// `(CxId, a, b, kind) -> cross-term value`.
    XTerm,
    /// `(CxId_a, CxId_b) -> temporal cross-term value`.
    TemporalXTerm,
    /// `(ScalarId, CxId) -> f64`.
    Scalars,
    /// `(CxId, AnchorKind) -> AnchorValue + source + ts`.
    Anchors,
    /// `(panel_version, corpus_shard, subject) -> AssayRow`.
    Assay,
    /// `seq -> hash-chained provenance entry`.
    Ledger,
    /// Persisted Lodestar grounding-kernel reports and indexes.
    Kernel,
    /// Persisted Ward guard calibration profiles.
    Guard,
    /// Leapable sidecar metadata owned by the stdio engine.
    Leapable,
    /// Versioned multi-panel and frozen-lens registry catalog.
    Registry,
    /// `(CxId, OccurrenceId) -> recurrence occurrence or summary`.
    Recurrence,
    /// Plain collection graph rows: nodes, typed edges, reverse index, CSR projection.
    Graph,
    /// Typed online/adaptation state.
    Online,
    /// Durable reactive trigger audit/fired rows.
    Reactive,
    /// Anneal rollback snapshots and live artifact pointers.
    AnnealRollback,
    /// Anneal component health snapshot.
    AnnealHealth,
    /// Anneal base-shard checksum and restore metadata.
    AnnealChecksums,
    /// Anneal online mistake-closure log.
    AnnealMistakes,
    /// Anneal surprise-prioritized replay buffer snapshot.
    AnnealReplay,
    /// Anneal online head parameters and Fisher diagonals.
    AnnealHeads,
    /// Anneal per-shape autotune bandit state.
    AnnealBandit,
    /// Anneal long-run soak metric samples and reports.
    AnnealSoak,
    /// Anneal intelligence report snapshots.
    AnnealReport,
    /// Anneal J-over-time growth curve samples.
    AnnealGrowth,
    /// Anneal learned-operator proposal records.
    AnnealOperators,
    /// Time-travel index: `big_endian_u64(millis_utc) || big_endian_u64(seqno)` -> sentinel.
    TimeIndex,
    /// Btree secondary index (PH54): `0x10 || collection_id || index_id ||
    /// field_val_encoded || pk -> ∅`. Existence is the signal; values are empty.
    IndexBtree,
    /// Inverted secondary index (PH54): `0x11 || collection_id || index_id ||
    /// term_hash || pk -> f32_be`. A reserved all-ones term hash stores avgdl stats.
    IndexInverted,
    /// Append-only raw-batch commitments: `seq_be -> version || seq || row_count || SHA-256`.
    ///
    /// This is an internal provenance surface. Callers cannot write it
    /// directly; Aster adds one row atomically to every commit that carries no
    /// Ledger row, and checkpoint maintenance seals ordered ranges of these
    /// rows into the Ledger chain.
    RawCommitment,
}

impl ColumnFamily {
    /// Static non-slot families in manifest order.
    pub const STATIC: [Self; 36] = [
        Self::Base,
        Self::Collections,
        Self::Relational,
        Self::XTerm,
        Self::TemporalXTerm,
        Self::Scalars,
        Self::Anchors,
        Self::Assay,
        Self::Ledger,
        Self::Recurrence,
        Self::Graph,
        Self::Online,
        Self::Reactive,
        Self::AnnealRollback,
        Self::AnnealHealth,
        Self::AnnealChecksums,
        Self::AnnealMistakes,
        Self::AnnealReplay,
        Self::AnnealHeads,
        Self::AnnealBandit,
        Self::AnnealSoak,
        Self::AnnealReport,
        Self::AnnealGrowth,
        Self::TimeIndex,
        Self::Document,
        Self::Kv,
        Self::TimeSeries,
        Self::Blob,
        Self::IndexBtree,
        Self::IndexInverted,
        Self::AnnealOperators,
        Self::Kernel,
        Self::Guard,
        Self::Leapable,
        // Append only: these positions are durable keyspace tags.
        Self::Registry,
        Self::RawCommitment,
    ];

    /// Creates a quantized slot column family such as `slot_00`.
    pub const fn slot(slot: SlotId) -> Self {
        Self::Slot {
            slot,
            kind: SlotFamilyKind::Quantized,
        }
    }

    /// Creates a raw sidecar slot column family such as `slot_00.raw`.
    pub const fn slot_raw(slot: SlotId) -> Self {
        Self::Slot {
            slot,
            kind: SlotFamilyKind::Raw,
        }
    }

    /// Returns the stable directory name under `vault/cf/`.
    pub fn name(&self) -> String {
        match self {
            Self::Base => "base".to_string(),
            Self::Collections => "collections".to_string(),
            Self::Relational => "relational".to_string(),
            Self::Document => "document".to_string(),
            Self::Kv => "kv".to_string(),
            Self::TimeSeries => "timeseries".to_string(),
            Self::Blob => "blob".to_string(),
            Self::Slot {
                slot,
                kind: SlotFamilyKind::Quantized,
            } => format!("slot_{:02}", slot.get()),
            Self::Slot {
                slot,
                kind: SlotFamilyKind::Raw,
            } => format!("slot_{:02}.raw", slot.get()),
            Self::XTerm => "xterm".to_string(),
            Self::TemporalXTerm => "temporal_xterm".to_string(),
            Self::Scalars => "scalars".to_string(),
            Self::Anchors => "anchors".to_string(),
            Self::Assay => "assay".to_string(),
            Self::Ledger => "ledger".to_string(),
            Self::Kernel => "kernel".to_string(),
            Self::Guard => "guard".to_string(),
            Self::Leapable => "leapable".to_string(),
            Self::Registry => "registry".to_string(),
            Self::Recurrence => "recurrence".to_string(),
            Self::Graph => "graph".to_string(),
            Self::Online => "online".to_string(),
            Self::Reactive => "reactive".to_string(),
            Self::AnnealRollback => "anneal_rollback".to_string(),
            Self::AnnealHealth => "anneal_health".to_string(),
            Self::AnnealChecksums => "anneal_checksums".to_string(),
            Self::AnnealMistakes => "anneal_mistakes".to_string(),
            Self::AnnealReplay => "anneal_replay".to_string(),
            Self::AnnealHeads => "anneal_heads".to_string(),
            Self::AnnealBandit => "anneal_bandit".to_string(),
            Self::AnnealSoak => "anneal_soak".to_string(),
            Self::AnnealReport => "anneal_report".to_string(),
            Self::AnnealGrowth => "anneal_growth".to_string(),
            Self::AnnealOperators => "anneal_operators".to_string(),
            Self::TimeIndex => "time_index".to_string(),
            Self::IndexBtree => "index_btree".to_string(),
            Self::IndexInverted => "index_inverted".to_string(),
            Self::RawCommitment => "raw_commitment".to_string(),
        }
    }

    /// Parses a stable `vault/cf/<name>` directory name back to a column family.
    pub fn from_name(name: &str) -> Option<Self> {
        if let Some(cf) = Self::STATIC.iter().copied().find(|cf| cf.name() == name) {
            return Some(cf);
        }
        let (slot_name, kind) = match name.strip_suffix(".raw") {
            Some(slot_name) => (slot_name, SlotFamilyKind::Raw),
            None => (name, SlotFamilyKind::Quantized),
        };
        let slot = slot_name.strip_prefix("slot_")?.parse::<u16>().ok()?;
        Some(Self::Slot {
            slot: SlotId::new(slot),
            kind,
        })
    }

    /// Returns true when writes to this CF change an input consumed by the
    /// persistent search-index builder and must therefore advance the
    /// vault's derived-content watermark (issues #1100 and #1808).
    ///
    /// The match is exhaustive on purpose: adding a CF forces an explicit
    /// decision here. The builder reads Base metadata and quantized Slot
    /// vectors only (`calyx-search::persisted::rebuild_scan`). All other CFs
    /// are independent databases or live query-time inputs and do not require
    /// regenerating the persistent vector/filter artifacts. Raw Slot
    /// sidecars are cold-tier rescore/re-quantization inputs, not index rows.
    pub const fn feeds_persistent_search_index(&self) -> bool {
        match self {
            Self::Base
            | Self::Slot {
                kind: SlotFamilyKind::Quantized,
                ..
            } => true,
            Self::Collections
            | Self::Relational
            | Self::Document
            | Self::Kv
            | Self::TimeSeries
            | Self::Blob
            | Self::Slot {
                kind: SlotFamilyKind::Raw,
                ..
            }
            | Self::XTerm
            | Self::TemporalXTerm
            | Self::Scalars
            | Self::Anchors
            | Self::Assay
            | Self::Kernel
            | Self::Guard
            | Self::Leapable
            | Self::Registry
            | Self::Recurrence
            | Self::Graph
            | Self::Online
            | Self::Reactive
            | Self::AnnealRollback
            | Self::AnnealHealth
            | Self::AnnealChecksums
            | Self::AnnealMistakes
            | Self::AnnealReplay
            | Self::AnnealHeads
            | Self::AnnealBandit
            | Self::AnnealSoak
            | Self::AnnealReport
            | Self::AnnealGrowth
            | Self::AnnealOperators
            | Self::IndexBtree
            | Self::IndexInverted
            | Self::Ledger
            | Self::TimeIndex
            | Self::RawCommitment => false,
        }
    }

    /// Returns true for slot CFs, including raw sidecars.
    pub const fn is_slot(&self) -> bool {
        matches!(self, Self::Slot { .. })
    }

    /// Returns true for raw f32 sidecar slot CFs.
    pub const fn is_raw_slot(&self) -> bool {
        matches!(
            self,
            Self::Slot {
                kind: SlotFamilyKind::Raw,
                ..
            }
        )
    }

    /// Returns the slot id for slot CFs.
    pub const fn slot_id(&self) -> Option<SlotId> {
        match self {
            Self::Slot { slot, .. } => Some(*slot),
            _ => None,
        }
    }

    /// Stable, reversible byte tag identifying this CF inside a vault-scoped key
    /// (see [`crate::vault::keyspace`]).
    ///
    /// Non-slot CFs encode to a single discriminant byte — their position in
    /// [`Self::STATIC`], which stays in sync automatically if the
    /// manifest order is extended. Slot CFs encode to
    /// `SLOT_TAG ‖ slot_id_be(2) ‖ kind_byte` so the slot index and
    /// quantized/raw flavor round-trip exactly. `STATIC` stays below 0xF0, so no
    /// static discriminant can collide with `SLOT_KEYSPACE_TAG` (`0xF0`).
    pub fn keyspace_tag(&self) -> Vec<u8> {
        match self {
            Self::Slot { slot, kind } => {
                let mut tag = Vec::with_capacity(4);
                tag.push(SLOT_KEYSPACE_TAG);
                tag.extend_from_slice(&slot.get().to_be_bytes());
                tag.push(match kind {
                    SlotFamilyKind::Quantized => 0,
                    SlotFamilyKind::Raw => 1,
                });
                tag
            }
            other => {
                let index = Self::STATIC
                    .iter()
                    .position(|cf| cf == other)
                    .expect("every non-slot ColumnFamily is listed in STATIC");
                vec![index as u8]
            }
        }
    }

    /// Inverse of [`Self::keyspace_tag`]: parses the CF tag off the front of
    /// `raw` and returns the CF plus the remaining (user-key) bytes.
    ///
    /// Returns `None` on any malformed tag (empty, unknown discriminant, or a
    /// truncated slot tag) so the caller can fail closed.
    pub fn parse_keyspace_tag(raw: &[u8]) -> Option<(Self, &[u8])> {
        let (&first, rest) = raw.split_first()?;
        if first == SLOT_KEYSPACE_TAG {
            let slot_bytes = rest.get(0..2)?;
            let kind = match rest.get(2)? {
                0 => SlotFamilyKind::Quantized,
                1 => SlotFamilyKind::Raw,
                _ => return None,
            };
            let slot = SlotId::new(u16::from_be_bytes([slot_bytes[0], slot_bytes[1]]));
            Some((Self::Slot { slot, kind }, &rest[3..]))
        } else {
            let cf = *Self::STATIC.get(first as usize)?;
            Some((cf, rest))
        }
    }
}

/// Discriminant byte that marks a slot CF tag. Distinct from every static-CF
/// discriminant because `STATIC.len()` remains far below this value.
const SLOT_KEYSPACE_TAG: u8 = 0xF0;
