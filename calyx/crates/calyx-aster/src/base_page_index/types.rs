use serde::{Deserialize, Serialize};

pub const BASE_PAGE_INDEX_DIR: &str = "base_page_index_v1";
pub const BASE_PAGE_INDEX_MANIFEST: &str = "manifest.json";
pub const BASE_PAGE_INDEX_GENERATIONS_DIR: &str = "generations";
pub const DEFAULT_BASE_PAGE_INDEX_PAGE_SIZE: usize = 1024;

pub(super) const INDEX_MAGIC: &str = "calyx.base_page_index";
pub(super) const LEGACY_INDEX_VERSION: u32 = 1;
pub(super) const GENERATION_INDEX_VERSION: u32 = 2;
/// Version 3 bound read-time freshness to the global ledger head, so any
/// Answer/Guard/Assay-only append falsely staled a byte-identical Base index.
/// It remains structurally decodable but is rejected at read time until the
/// index is rebuilt into the current version.
pub(super) const PRIOR_INDEX_VERSION: u32 = 3;
/// Version 4 binds freshness to a content-derived Base-source digest over the
/// ordered Base SST identity set plus the Base WAL-row tail.
pub(super) const INDEX_VERSION: u32 = 4;
pub(super) const MISSING_CODE: &str = "CALYX_BASE_PAGE_INDEX_MISSING";
pub(super) const STALE_CODE: &str = "CALYX_BASE_PAGE_INDEX_STALE";
pub(super) const CORRUPT_CODE: &str = "CALYX_BASE_PAGE_INDEX_CORRUPT";
pub(super) const REMEDIATION: &str = "run `calyx readback cx-list --vault <dir> --limit <n> --rebuild-base-page-index` to rebuild the checked Base page index";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BasePageIndexManifest {
    pub magic: String,
    pub version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation: Option<String>,
    /// SHA-256 over the ordered Base SST identity set plus the Base WAL-row
    /// tail after `base_durable_seq`. Pre-v4 manifests decode with an empty
    /// value and are rejected as stale before this field is consulted.
    #[serde(default)]
    pub base_source_digest_hex: String,
    /// Durable sequence floor used to select the WAL tail. This is retained
    /// for diagnostics; freshness is gated by `base_source_digest_hex`.
    #[serde(default)]
    pub base_durable_seq: u64,
    /// Informational build-time ledger head. Version 4 does not use the global
    /// ledger head as its freshness boundary.
    pub ledger_head_height: u64,
    pub ledger_head_tip_hash_hex: String,
    pub page_size: usize,
    pub total_entries: usize,
    pub live_entries: usize,
    pub tombstone_entries: usize,
    pub base_sst_files: usize,
    pub wal_records: usize,
    pub built_at_unix_ms: u128,
    pub pages: Vec<BasePageIndexPageRef>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BasePageIndexPageRef {
    pub path: String,
    pub first_key_hex: String,
    pub last_key_hex: String,
    pub entry_count: usize,
    pub live_entry_count: usize,
    pub sha256_hex: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BasePageIndexPage {
    pub entries: Vec<BasePageIndexEntry>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BasePageIndexEntry {
    pub key_hex: String,
    pub value_sha256_hex: String,
    pub tombstoned: bool,
    pub source: BasePageIndexSource,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BasePageIndexSource {
    Sst {
        path: String,
        /// Seq-domain epoch of the order key (issue #1138): 0 for legacy
        /// flush-ordinal names, 1 for commit-domain names. Manifests written
        /// before this field existed default to 0 (their order fields were
        /// computed in the pre-epoch single domain).
        #[serde(default)]
        order_epoch: u8,
        order_seq: u64,
        order_class_rank: u8,
        order_index: usize,
        /// Exact SST record start. Version 3 readers require this so a Base
        /// point read does not checksum and decode the whole SST body.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        record_offset: Option<u64>,
    },
    Wal {
        path: String,
        seq: u64,
        start_offset: u64,
        end_offset: u64,
        /// Exact CF-tag offset inside the encoded WAL write-batch payload.
        /// Version 3 readers require this to skip unrelated slot payloads.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        row_offset: Option<u64>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BasePageIndexBuildProgress {
    ScanStarted {
        sst_files: usize,
        ledger_head_height: u64,
    },
    SstScanned {
        scanned_sst_files: usize,
        total_sst_files: usize,
        current_rows: usize,
    },
    WalScanned {
        wal_records: usize,
        current_rows: usize,
    },
    PageWritten {
        page_index: usize,
        entry_count: usize,
        live_entry_count: usize,
    },
    Complete {
        total_entries: usize,
        live_entries: usize,
        pages: usize,
    },
}
