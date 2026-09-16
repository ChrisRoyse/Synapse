//! Token/cost accounting domain types and integer-exact pricing math (#901).
//!
//! These types close the "spawned agents are cost-opaque" gap. Token usage is
//! extracted from the authoritative `CF_AGENT_TRANSCRIPTS` rows ingested in
//! #900, normalized onto a single canonical [`BillableUsage`] shape, and
//! priced against an operator-editable [`ModelPrice`] table. No external
//! pricing API is ever consulted: an unpriced model yields
//! [`CostOutcome::Unpriced`] with the model id surfaced — never a guessed
//! number (the honesty rule from #901).
//!
//! # Provider cache semantics differ — normalization makes pricing uniform
//!
//! The two CLI providers report cache tokens with *different* relationships to
//! `input_tokens`, verified against real CLI output and provider docs:
//!
//! - **Anthropic / Claude** — `input_tokens`, `cache_read_input_tokens`, and
//!   `cache_creation_input_tokens` are **disjoint**; total input =
//!   `input + cache_read + cache_creation`. Each is billed at its own rate.
//!   (<https://platform.claude.com/docs/en/api/rate-limits>.)
//! - **`OpenAI` / Codex** — `cached_input_tokens` is a **subset** of
//!   `input_tokens`; the cached portion is billed at the discounted cache rate
//!   and only the remainder at the full input rate. `reasoning_output_tokens`
//!   are a subset of `output_tokens` and are *not* priced separately.
//!   (<https://developers.openai.com/api/docs/guides/prompt-caching>.)
//!
//! To keep the pricing function uniform and auditable, the source-specific
//! arithmetic lives in the *extractor* ([`BillableUsage::from_claude`] /
//! [`BillableUsage::from_codex_cumulative`]), which produces a canonical
//! [`BillableUsage`] whose four dimensions are always disjoint. The pricing
//! function then multiplies each disjoint dimension by its own rate. This is
//! why a Codex row's `cached` count is moved into `cache_read_tokens` and
//! subtracted from `input_tokens` at extraction time.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Schema version for [`ModelPrice`] rows stored in the operator price table.
pub const MODEL_PRICE_VERSION: u32 = 1;

/// Upper bound on a model id length accepted into the price table. Generous
/// for provider/version/ARN-style ids while still bounding the key space.
pub const MODEL_PRICE_MAX_ID_CHARS: usize = 256;

/// One operator-defined price row: per-million-token rates for a single model.
///
/// Rates are stored as **micro-USD per million tokens** (`micro_usd_per_mtok`)
/// so the table is integer-exact and matches how providers publish pricing
/// (dollars per million tokens). A rate of `$15.00 / MTok` is stored as
/// `15_000_000`. Storing integers avoids float drift in accumulated fleet
/// totals.
///
/// All four rates are required (use `0` for a genuinely free dimension, e.g.
/// a local model). A model the operator has not priced simply has no row, and
/// the cost engine reports it as [`CostOutcome::Unpriced`] rather than
/// guessing.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ModelPrice {
    pub version: u32,
    /// Canonical model id, matched against the transcript row's `model`
    /// field. Lower-cased and trimmed at write time so lookups are stable.
    pub model_id: String,
    /// Optional human label for the provider (`anthropic`, `openai`, `local`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// Full (uncached) input token rate, micro-USD per million tokens.
    pub input_micro_usd_per_mtok: u64,
    /// Output token rate, micro-USD per million tokens. Reasoning/completion
    /// tokens are billed here (they are a subset of output tokens).
    pub output_micro_usd_per_mtok: u64,
    /// Cache-read (hit) token rate, micro-USD per million tokens. For Claude
    /// this is the 0.1x base read rate; for `OpenAI` the discounted cached rate.
    pub cache_read_micro_usd_per_mtok: u64,
    /// Aggregate cache-creation (write) token rate, micro-USD per million
    /// tokens. Used for cache-write tokens with no reported TTL tier (and as
    /// the per-tier rate when the tier-specific rate below is unset). For
    /// `OpenAI` this is `0` — `OpenAI` does not bill cache writes.
    pub cache_creation_micro_usd_per_mtok: u64,
    /// Anthropic 5-minute-TTL cache-write rate (1.25x base input), micro-USD
    /// per million tokens. `None` falls back to the aggregate rate above, so a
    /// run that mixes TTLs is priced exactly once both tiers are set (#949).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_creation_5m_micro_usd_per_mtok: Option<u64>,
    /// Anthropic 1-hour-TTL cache-write rate (2x base input), micro-USD per
    /// million tokens. `None` falls back to the aggregate rate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_creation_1h_micro_usd_per_mtok: Option<u64>,
    /// When this row was last written (unix nanoseconds).
    pub updated_ts_ns: u64,
}

impl ModelPrice {
    /// Normalizes a raw model id for storage and lookup: trimmed, lower-cased.
    #[must_use]
    pub fn normalize_id(raw: &str) -> String {
        raw.trim().to_ascii_lowercase()
    }

    /// Validates a price row before it is persisted. Violations are operator
    /// input errors surfaced loudly, never silently repaired.
    ///
    /// # Errors
    ///
    /// Returns a structured detail string naming the first violated
    /// constraint.
    pub fn validate(&self) -> Result<(), String> {
        if self.version != MODEL_PRICE_VERSION {
            return Err(format!(
                "MODEL_PRICE_INVALID: version {} != {MODEL_PRICE_VERSION}",
                self.version
            ));
        }
        if self.model_id.is_empty() {
            return Err("MODEL_PRICE_INVALID: model_id must not be empty".to_owned());
        }
        if self.model_id.chars().count() > MODEL_PRICE_MAX_ID_CHARS {
            return Err(format!(
                "MODEL_PRICE_INVALID: model_id exceeds {MODEL_PRICE_MAX_ID_CHARS} chars"
            ));
        }
        if self.model_id != Self::normalize_id(&self.model_id) {
            return Err(
                "MODEL_PRICE_INVALID: model_id must be stored normalized (trimmed, lower-cased)"
                    .to_owned(),
            );
        }
        if self.updated_ts_ns == 0 {
            return Err(
                "MODEL_PRICE_INVALID: updated_ts_ns must be a positive timestamp".to_owned(),
            );
        }
        Ok(())
    }

    /// Prices a canonical [`BillableUsage`] against this row, integer-exact.
    ///
    /// Each disjoint token dimension is multiplied by its own per-million rate
    /// in `u128` to avoid overflow, then floor-divided back to micro-USD. The
    /// four contributions are summed. Overflow of the final `u64` micro-USD
    /// total is reported loudly rather than wrapping.
    ///
    /// # Errors
    ///
    /// Returns a detail string if the accumulated micro-USD total exceeds
    /// `u64::MAX` (a fleet total above ~$18.4 trillion — only reachable by a
    /// corrupt usage row, which must surface, not wrap).
    pub fn cost_micro_usd(&self, usage: &BillableUsage) -> Result<CostBreakdown, String> {
        let line = |tokens: u64, rate: u64| -> u128 {
            // tokens * rate / 1_000_000, floor. u128 cannot overflow here:
            // u64::MAX * u64::MAX < u128::MAX.
            (u128::from(tokens) * u128::from(rate)) / 1_000_000
        };
        let input = line(usage.input_tokens, self.input_micro_usd_per_mtok);
        let output = line(usage.output_tokens, self.output_micro_usd_per_mtok);
        let cache_read = line(usage.cache_read_tokens, self.cache_read_micro_usd_per_mtok);
        // Cache writes are billed per TTL tier (1.25x vs 2x base input). When
        // the transcript reported a tier split, each tier is priced at its own
        // rate; any untagged remainder (and the whole amount when no split was
        // reported) is priced at the aggregate rate. A tier rate left unset on
        // the price row falls back to the aggregate rate so a partially-priced
        // table never silently drops a dimension.
        let tagged = usage
            .cache_creation_5m_tokens
            .saturating_add(usage.cache_creation_1h_tokens);
        if tagged > usage.cache_creation_tokens {
            return Err(format!(
                "MODEL_PRICE_USAGE_INVALID: tagged cache-creation tokens {tagged} (5m {} + 1h {}) \
                 exceed the aggregate {} — the TTL split must be a subset",
                usage.cache_creation_5m_tokens,
                usage.cache_creation_1h_tokens,
                usage.cache_creation_tokens
            ));
        }
        let untagged = usage.cache_creation_tokens - tagged;
        let five_minute_cache_write_rate = self
            .cache_creation_5m_micro_usd_per_mtok
            .unwrap_or(self.cache_creation_micro_usd_per_mtok);
        let one_hour_cache_write_rate = self
            .cache_creation_1h_micro_usd_per_mtok
            .unwrap_or(self.cache_creation_micro_usd_per_mtok);
        let cache_creation = line(usage.cache_creation_5m_tokens, five_minute_cache_write_rate)
            + line(usage.cache_creation_1h_tokens, one_hour_cache_write_rate)
            + line(untagged, self.cache_creation_micro_usd_per_mtok);
        let total = input + output + cache_read + cache_creation;
        let to_u64 = |value: u128, label: &str| -> Result<u64, String> {
            u64::try_from(value).map_err(|_e| {
                format!("MODEL_PRICE_COST_OVERFLOW: {label} micro-USD {value} exceeds u64::MAX")
            })
        };
        Ok(CostBreakdown {
            input_micro_usd: to_u64(input, "input")?,
            output_micro_usd: to_u64(output, "output")?,
            cache_read_micro_usd: to_u64(cache_read, "cache_read")?,
            cache_creation_micro_usd: to_u64(cache_creation, "cache_creation")?,
            total_micro_usd: to_u64(total, "total")?,
        })
    }
}

/// Canonical, provider-agnostic token usage with **disjoint** dimensions.
///
/// "Disjoint" means no token is counted in more than one field, so the four
/// fields can each be priced independently and summed. Extractors convert the
/// provider-native shape into this canonical form (see module docs).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BillableUsage {
    /// Full-rate input tokens (already excludes any cached portion).
    pub input_tokens: u64,
    /// Output (completion) tokens. Includes reasoning tokens, which providers
    /// bill at the output rate.
    pub output_tokens: u64,
    /// Cache-read (hit) tokens, billed at the cache-read rate.
    pub cache_read_tokens: u64,
    /// Cache-creation (write) tokens, billed at the cache-write rate. This is
    /// the aggregate; the two tier fields below are subsets of it.
    pub cache_creation_tokens: u64,
    /// 5-minute-TTL portion of `cache_creation_tokens` (1.25x base input).
    /// `0` when the source reported no tier split.
    #[serde(default)]
    pub cache_creation_5m_tokens: u64,
    /// 1-hour-TTL portion of `cache_creation_tokens` (2x base input).
    #[serde(default)]
    pub cache_creation_1h_tokens: u64,
}

impl BillableUsage {
    /// Total tokens across all four disjoint dimensions.
    #[must_use]
    pub const fn total_tokens(&self) -> u64 {
        self.input_tokens
            .saturating_add(self.output_tokens)
            .saturating_add(self.cache_read_tokens)
            .saturating_add(self.cache_creation_tokens)
    }

    /// True when no tokens are recorded in any dimension.
    #[must_use]
    pub const fn is_zero(&self) -> bool {
        self.input_tokens == 0
            && self.output_tokens == 0
            && self.cache_read_tokens == 0
            && self.cache_creation_tokens == 0
    }

    /// Builds canonical usage from a **Claude** usage block, whose dimensions
    /// are already disjoint. The four counts map straight across; the
    /// cache-creation TTL tiers are left unsplit (priced at the aggregate
    /// rate). Use [`Self::from_claude_with_ttl`] when the tier split is known.
    #[must_use]
    pub const fn from_claude(
        input_tokens: u64,
        output_tokens: u64,
        cache_read_input_tokens: u64,
        cache_creation_input_tokens: u64,
    ) -> Self {
        Self {
            input_tokens,
            output_tokens,
            cache_read_tokens: cache_read_input_tokens,
            cache_creation_tokens: cache_creation_input_tokens,
            cache_creation_5m_tokens: 0,
            cache_creation_1h_tokens: 0,
        }
    }

    /// Builds canonical Claude usage carrying the cache-creation TTL split, so
    /// the cost engine can price 5m and 1h writes at their distinct rates
    /// (#949). The two tier counts must each be `<= cache_creation_input_tokens`
    /// and their sum must not exceed it; the pricing function enforces the
    /// subset invariant and surfaces a violation rather than clamping.
    #[must_use]
    pub const fn from_claude_with_ttl(
        input_tokens: u64,
        output_tokens: u64,
        cache_read_input_tokens: u64,
        cache_creation_input_tokens: u64,
        cache_creation_short_ttl_tokens: u64,
        cache_creation_long_ttl_tokens: u64,
    ) -> Self {
        Self {
            input_tokens,
            output_tokens,
            cache_read_tokens: cache_read_input_tokens,
            cache_creation_tokens: cache_creation_input_tokens,
            cache_creation_5m_tokens: cache_creation_short_ttl_tokens,
            cache_creation_1h_tokens: cache_creation_long_ttl_tokens,
        }
    }

    /// Builds canonical usage from a **Codex / `OpenAI`** cumulative turn usage
    /// block, where `cached_input_tokens` is a subset of `input_tokens`. The
    /// cached portion is moved into `cache_read_tokens` and subtracted from
    /// the full-rate input. `OpenAI` does not report cache writes, so
    /// `cache_creation_tokens` is always `0`.
    ///
    /// # Errors
    ///
    /// Returns a detail string when `cached_input_tokens > input_tokens`,
    /// which would mean the source row is malformed — surfaced, never clamped.
    pub fn from_codex_cumulative(
        input_tokens: u64,
        output_tokens: u64,
        cached_input_tokens: u64,
    ) -> Result<Self, String> {
        if cached_input_tokens > input_tokens {
            return Err(format!(
                "CODEX_USAGE_INVALID: cached_input_tokens {cached_input_tokens} exceeds \
                 input_tokens {input_tokens} (cached must be a subset of input)"
            ));
        }
        Ok(Self {
            input_tokens: input_tokens - cached_input_tokens,
            output_tokens,
            cache_read_tokens: cached_input_tokens,
            cache_creation_tokens: 0,
            cache_creation_5m_tokens: 0,
            cache_creation_1h_tokens: 0,
        })
    }
}

/// Per-dimension cost contributions, all in micro-USD, summing to `total`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CostBreakdown {
    pub input_micro_usd: u64,
    pub output_micro_usd: u64,
    pub cache_read_micro_usd: u64,
    pub cache_creation_micro_usd: u64,
    pub total_micro_usd: u64,
}

/// Outcome of pricing one model's usage: either a computed cost, or an honest
/// "unpriced" marker carrying the model id so the operator knows exactly which
/// row to add to the price table.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum CostOutcome {
    /// The model has a price row; `cost` is integer-exact.
    Priced { cost: CostBreakdown },
    /// No price row exists for `model_id`; cost is deliberately unknown.
    Unpriced { model_id: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn price(input: u64, output: u64, cr: u64, cc: u64) -> ModelPrice {
        ModelPrice {
            version: MODEL_PRICE_VERSION,
            model_id: "test-model".to_owned(),
            provider: None,
            input_micro_usd_per_mtok: input,
            output_micro_usd_per_mtok: output,
            cache_read_micro_usd_per_mtok: cr,
            cache_creation_micro_usd_per_mtok: cc,
            cache_creation_5m_micro_usd_per_mtok: None,
            cache_creation_1h_micro_usd_per_mtok: None,
            updated_ts_ns: 1,
        }
    }

    #[test]
    fn prices_each_disjoint_dimension_independently() {
        // $3/MTok input, $15/MTok output, $0.30/MTok read, $3.75/MTok write.
        let p = price(3_000_000, 15_000_000, 300_000, 3_750_000);
        let usage = BillableUsage::from_claude(1_000_000, 1_000_000, 1_000_000, 1_000_000);
        let cost = p.cost_micro_usd(&usage).expect("prices");
        assert_eq!(cost.input_micro_usd, 3_000_000);
        assert_eq!(cost.output_micro_usd, 15_000_000);
        assert_eq!(cost.cache_read_micro_usd, 300_000);
        assert_eq!(cost.cache_creation_micro_usd, 3_750_000);
        assert_eq!(cost.total_micro_usd, 22_050_000);
    }

    #[test]
    fn cache_creation_ttl_tiers_priced_at_distinct_rates() {
        // Real Claude Fable 5 published rates: $10 base input, $50 output,
        // $1 cache read, $12.50 5m-write (1.25x), $20 1h-write (2x).
        let p = ModelPrice {
            version: MODEL_PRICE_VERSION,
            model_id: "test-model".to_owned(),
            provider: None,
            input_micro_usd_per_mtok: 10_000_000,
            output_micro_usd_per_mtok: 50_000_000,
            cache_read_micro_usd_per_mtok: 1_000_000,
            cache_creation_micro_usd_per_mtok: 12_500_000, // aggregate == 5m
            cache_creation_5m_micro_usd_per_mtok: Some(12_500_000),
            cache_creation_1h_micro_usd_per_mtok: Some(20_000_000),
            updated_ts_ns: 1,
        };
        // The real claude_stream_real.jsonl Fable 5 slice: in=4118, out=1906,
        // cache_read=283040, cache_creation=22189 ALL in the 1h tier.
        let usage = BillableUsage::from_claude_with_ttl(4118, 1906, 283_040, 22_189, 0, 22_189);
        let cost = p.cost_micro_usd(&usage).expect("prices");
        assert_eq!(cost.input_micro_usd, 41_180); // 4118 * 10
        assert_eq!(cost.output_micro_usd, 95_300); // 1906 * 50
        assert_eq!(cost.cache_read_micro_usd, 283_040); // 283040 * 1
        // Priced at the 1h rate ($20), NOT the 5m/aggregate rate ($12.50):
        assert_eq!(cost.cache_creation_micro_usd, 443_780); // 22189 * 20
        // 41180 + 95300 + 283040 + 443780 == 863300 micro == $0.8633, which is
        // exactly the modelUsage costUSD Claude reported for claude-fable-5.
        assert_eq!(cost.total_micro_usd, 863_300);
    }

    #[test]
    fn untagged_cache_creation_uses_aggregate_rate() {
        // No tier split reported (from_claude): the whole aggregate prices at
        // the aggregate rate, preserving pre-#949 behavior.
        let p = price(0, 0, 0, 3_750_000);
        let usage = BillableUsage::from_claude(0, 0, 0, 1000);
        assert_eq!(
            p.cost_micro_usd(&usage)
                .expect("ok")
                .cache_creation_micro_usd,
            3_750
        );
    }

    #[test]
    fn tier_rate_unset_falls_back_to_aggregate() {
        // 1h tokens present but no 1h rate set -> aggregate rate used, never 0.
        let p = price(0, 0, 0, 3_750_000);
        let usage = BillableUsage::from_claude_with_ttl(0, 0, 0, 1000, 0, 1000);
        assert_eq!(
            p.cost_micro_usd(&usage)
                .expect("ok")
                .cache_creation_micro_usd,
            3_750
        );
    }

    #[test]
    fn cache_creation_tier_overflow_is_rejected() {
        let p = price(0, 0, 0, 1);
        // Tagged (600 + 600 = 1200) exceeds the aggregate (1000): corrupt.
        let usage = BillableUsage::from_claude_with_ttl(0, 0, 0, 1000, 600, 600);
        let err = p.cost_micro_usd(&usage).expect_err("must reject");
        assert!(err.contains("MODEL_PRICE_USAGE_INVALID"), "{err}");
    }

    #[test]
    fn small_token_counts_are_integer_exact() {
        // $0.25/MTok input over 1000 tokens = 250 micro-USD = $0.00025.
        let p = price(250_000, 0, 0, 0);
        let usage = BillableUsage::from_claude(1_000, 0, 0, 0);
        let cost = p.cost_micro_usd(&usage).expect("prices");
        assert_eq!(cost.total_micro_usd, 250);
    }

    #[test]
    fn zero_rate_dimension_is_free() {
        let p = price(0, 0, 0, 0);
        let usage = BillableUsage::from_claude(999, 999, 999, 999);
        assert_eq!(p.cost_micro_usd(&usage).expect("prices").total_micro_usd, 0);
    }

    #[test]
    fn codex_subtracts_cached_from_input() {
        // input 144733 includes cached 103296; full-rate input must be 41437.
        let usage = BillableUsage::from_codex_cumulative(144_733, 2_110, 103_296)
            .expect("cached is a subset");
        assert_eq!(usage.input_tokens, 41_437);
        assert_eq!(usage.cache_read_tokens, 103_296);
        assert_eq!(usage.cache_creation_tokens, 0);
        assert_eq!(usage.output_tokens, 2_110);
    }

    #[test]
    fn codex_rejects_cached_exceeding_input() {
        let error = BillableUsage::from_codex_cumulative(100, 0, 101).expect_err("must refuse");
        assert!(error.contains("CODEX_USAGE_INVALID"), "{error}");
    }

    #[test]
    fn overflow_is_reported_not_wrapped() {
        let p = price(u64::MAX, 0, 0, 0);
        let usage = BillableUsage::from_claude(u64::MAX, 0, 0, 0);
        let error = p.cost_micro_usd(&usage).expect_err("must overflow loudly");
        assert!(error.contains("MODEL_PRICE_COST_OVERFLOW"), "{error}");
    }

    #[test]
    fn validate_rejects_unnormalized_id() {
        let mut p = price(1, 1, 1, 1);
        p.model_id = "Claude-Opus".to_owned();
        assert!(p.validate().is_err(), "mixed-case id must be rejected");
    }

    #[test]
    fn validate_accepts_normalized_row() {
        assert!(price(1, 1, 1, 1).validate().is_ok());
    }

    #[test]
    fn normalize_id_trims_and_lowercases() {
        assert_eq!(
            ModelPrice::normalize_id("  Claude-Fable-5 "),
            "claude-fable-5"
        );
    }
}
