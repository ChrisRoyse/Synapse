/// Default retention and size budget for one storage column family.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RetentionDefault {
    pub cf: &'static str,
    pub ttl: RetentionTtl,
    pub soft_cap_mb: u64,
    pub hard_cap_mb: u64,
    pub cap_eviction: RetentionCapEviction,
}

/// Whether byte-cap GC may delete rows from one column family independently.
///
/// A source and its strict secondary index are one logical relation. Their
/// writes and TTL boundary are coupled, so generic per-CF LRU eviction must not
/// delete either side independently. The peer is declared on both entries so
/// storage startup can fail closed on an asymmetric or TTL-mismatched policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RetentionCapEviction {
    Independent,
    LockstepWith(&'static str),
}

/// Default TTL policy for a storage column family.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RetentionTtl {
    None,
    Hours(u64),
    Days(u64),
    LruOnly,
}

/// PRD §4/§6 storage retention defaults.
pub const DEFAULTS: [RetentionDefault; 20] = [
    RetentionDefault {
        cf: "CF_EVENTS",
        ttl: RetentionTtl::Days(3),
        soft_cap_mb: 256,
        hard_cap_mb: 512,
        cap_eviction: RetentionCapEviction::Independent,
    },
    RetentionDefault {
        cf: "CF_OBSERVATIONS",
        ttl: RetentionTtl::Hours(24),
        soft_cap_mb: 128,
        hard_cap_mb: 256,
        cap_eviction: RetentionCapEviction::Independent,
    },
    RetentionDefault {
        cf: "CF_PROFILES",
        ttl: RetentionTtl::None,
        soft_cap_mb: 20,
        hard_cap_mb: 50,
        cap_eviction: RetentionCapEviction::Independent,
    },
    RetentionDefault {
        cf: "CF_MODEL_CACHE",
        ttl: RetentionTtl::LruOnly,
        soft_cap_mb: 256,
        hard_cap_mb: 512,
        cap_eviction: RetentionCapEviction::Independent,
    },
    RetentionDefault {
        cf: "CF_SESSIONS",
        ttl: RetentionTtl::Days(30),
        soft_cap_mb: 50,
        hard_cap_mb: 100,
        cap_eviction: RetentionCapEviction::Independent,
    },
    RetentionDefault {
        cf: "CF_REFLEX_AUDIT",
        ttl: RetentionTtl::Days(14),
        soft_cap_mb: 64,
        hard_cap_mb: 128,
        cap_eviction: RetentionCapEviction::LockstepWith("CF_REFLEX_AUDIT_ORDER"),
    },
    RetentionDefault {
        cf: "CF_OCR_CACHE",
        ttl: RetentionTtl::Hours(1),
        soft_cap_mb: 50,
        hard_cap_mb: 100,
        cap_eviction: RetentionCapEviction::Independent,
    },
    RetentionDefault {
        cf: "CF_TELEMETRY",
        ttl: RetentionTtl::Days(3),
        soft_cap_mb: 64,
        hard_cap_mb: 128,
        cap_eviction: RetentionCapEviction::Independent,
    },
    RetentionDefault {
        cf: "CF_ACTION_LOG",
        ttl: RetentionTtl::Days(14),
        soft_cap_mb: 64,
        hard_cap_mb: 128,
        cap_eviction: RetentionCapEviction::Independent,
    },
    RetentionDefault {
        cf: "CF_PROCESS_HISTORY",
        ttl: RetentionTtl::Hours(24),
        soft_cap_mb: 32,
        hard_cap_mb: 64,
        cap_eviction: RetentionCapEviction::Independent,
    },
    RetentionDefault {
        cf: "CF_KV",
        ttl: RetentionTtl::None,
        soft_cap_mb: 10,
        hard_cap_mb: 50,
        cap_eviction: RetentionCapEviction::Independent,
    },
    // Summarized operator activity remains useful for a month while detailed
    // evidence expires much sooner.
    RetentionDefault {
        cf: "CF_TIMELINE",
        ttl: RetentionTtl::Days(30),
        soft_cap_mb: 256,
        hard_cap_mb: 512,
        cap_eviction: RetentionCapEviction::Independent,
    },
    // Derived episodes (#846): same retention horizon as their source
    // timeline rows, far smaller footprint (one row per focused span, not
    // per event). Rebuildable at any time by re-segmentation.
    RetentionDefault {
        cf: "CF_EPISODES",
        ttl: RetentionTtl::Days(30),
        soft_cap_mb: 64,
        hard_cap_mb: 128,
        cap_eviction: RetentionCapEviction::Independent,
    },
    // Derived routines (#848): a few hundred small rows replaced wholesale
    // on every mining run; the mining window (episode retention) bounds the
    // content, so no TTL — stale rows cannot outlive a re-mine.
    RetentionDefault {
        cf: "CF_ROUTINES",
        ttl: RetentionTtl::None,
        soft_cap_mb: 16,
        hard_cap_mb: 64,
        cap_eviction: RetentionCapEviction::Independent,
    },
    // Operator routine lifecycle state (#849): confirmations, disables,
    // labels, transition audit trails. Operator decisions must never
    // silently expire, so no TTL; the store is bounded by the routine id
    // space (a few hundred rows) and per-row history caps.
    RetentionDefault {
        cf: "CF_ROUTINE_STATE",
        ttl: RetentionTtl::None,
        soft_cap_mb: 16,
        hard_cap_mb: 64,
        cap_eviction: RetentionCapEviction::Independent,
    },
    // Durable agent-event journal: 30 days covers operational audit and
    // dashboard reconciliation without competing with 90-day summaries.
    RetentionDefault {
        cf: "CF_AGENT_EVENTS",
        ttl: RetentionTtl::Days(14),
        soft_cap_mb: 128,
        hard_cap_mb: 256,
        cap_eviction: RetentionCapEviction::LockstepWith("CF_AGENT_EVENT_SPAWN_INDEX"),
    },
    // Normalized spawned-agent transcripts: raw/high-volume evidence gets a
    // 14-day and 1-GiB envelope; durable summaries and event audit outlive it.
    RetentionDefault {
        cf: "CF_AGENT_TRANSCRIPTS",
        ttl: RetentionTtl::Days(7),
        soft_cap_mb: 256,
        hard_cap_mb: 512,
        cap_eviction: RetentionCapEviction::LockstepWith("CF_AGENT_TRANSCRIPT_ORDER"),
    },
    // Exact timestamp-order pointer index for transcript health/dashboard
    // reads (#2189). Its retention must remain identical to the source CF so
    // source and index disappear at the same logical boundary.
    RetentionDefault {
        cf: "CF_AGENT_TRANSCRIPT_ORDER",
        ttl: RetentionTtl::Days(7),
        soft_cap_mb: 32,
        hard_cap_mb: 64,
        cap_eviction: RetentionCapEviction::LockstepWith("CF_AGENT_TRANSCRIPTS"),
    },
    // Exact timestamp-order pointer index for global reflex history (#2190).
    // Keep this in lockstep with CF_REFLEX_AUDIT's 14-day contract.
    RetentionDefault {
        cf: "CF_REFLEX_AUDIT_ORDER",
        ttl: RetentionTtl::Days(14),
        soft_cap_mb: 16,
        hard_cap_mb: 32,
        cap_eviction: RetentionCapEviction::LockstepWith("CF_REFLEX_AUDIT"),
    },
    // Exact spawn-scoped pointer index for the agent-event journal (#2140).
    // It must expire at the same logical boundary as its source; placing this
    // in non-expiring CF_KV would manufacture dangling index rows after day 30.
    RetentionDefault {
        cf: "CF_AGENT_EVENT_SPAWN_INDEX",
        ttl: RetentionTtl::Days(14),
        soft_cap_mb: 32,
        hard_cap_mb: 64,
        cap_eviction: RetentionCapEviction::LockstepWith("CF_AGENT_EVENTS"),
    },
];
