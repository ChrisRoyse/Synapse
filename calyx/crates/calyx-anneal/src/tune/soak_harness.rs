mod storage;
mod types;

use std::time::Instant;

use calyx_aster::vault::AsterVault;
use calyx_core::{CalyxError, Clock, LensId, PanelSlotId, Result, SlotId};
use calyx_forge::AutotuneCache;

pub use storage::{
    AsterSoakStorage, NoopSoakStorage, SoakStorage, decode_soak_reports, decode_soak_row,
    encode_soak_row, soak_report_key, soak_sample_key,
};
pub use types::{
    CALYX_ANNEAL_SOAK_INVALID_CONFIG, CALYX_ANNEAL_SOAK_INVALID_ROW,
    CALYX_ANNEAL_SOAK_LIVE_TRAFFIC_UNAVAILABLE, CALYX_ANNEAL_SOAK_TIME_BUDGET_EXHAUSTED,
    DEFAULT_SOAK_OSCILLATION_WINDOW, DEFAULT_SOAK_P99_TARGET_REDUCTION, DEFAULT_SOAK_QUERIES,
    DEFAULT_SOAK_SAMPLE_INTERVAL, DEFAULT_SOAK_SEED, MetricSample, SeededSoakProfile, SoakConfig,
    SoakMetrics, SoakMode, SoakReport, SoakRowKind, SoakStoredRow,
};

use crate::{
    ABLedgerWriter, ABPromotionConfig, ABResult, ABRunner, ABTrialBudget, ABVerdict, BanditPolicy,
    ChangeId, ConfigBandit, DType, ForgeConfig, ForgeScopeTuner, IndexScopeTuner, LoomScopeTuner,
    MatPlanConfig, NoopABBudget, NoopABLedgerWriter, QueryLog, QueryObservation, ShapeKey,
};

const INCUMBENT_ARM: usize = 0;
const CANDIDATE_ARM: usize = 1;
const SOAK_SLOT: PanelSlotId = PanelSlotId::new(0, SlotId::new(0));

pub struct SoakHarness<S = NoopSoakStorage, W = NoopABLedgerWriter, B = NoopABBudget>
where
    S: SoakStorage,
    W: ABLedgerWriter,
    B: ABTrialBudget,
{
    pub config: SoakConfig,
    pub forge_tuner: ForgeScopeTuner,
    pub index_tuner: IndexScopeTuner,
    pub loom_tuner: LoomScopeTuner,
    pub ab_runner: ABRunner<W, B>,
    pub metrics: SoakMetrics,
    pub storage: S,
    profile: SeededSoakProfile,
    last_report: Option<SoakReport>,
}

impl<S, W, B> SoakHarness<S, W, B>
where
    S: SoakStorage,
    W: ABLedgerWriter,
    B: ABTrialBudget,
{
    pub fn seeded(
        config: SoakConfig,
        cache: AutotuneCache,
        ab_runner: ABRunner<W, B>,
        storage: S,
    ) -> Self {
        let forge_tuner = ForgeScopeTuner::new(cache.clone());
        let index_tuner = IndexScopeTuner::new(cache.clone());
        let loom_tuner = LoomScopeTuner::new(cache, MatPlanConfig::default());
        Self::with_parts(
            config,
            forge_tuner,
            index_tuner,
            loom_tuner,
            ab_runner,
            storage,
            SeededSoakProfile::default(),
        )
    }

    pub fn live_traffic(
        mut config: SoakConfig,
        forge_tuner: ForgeScopeTuner,
        index_tuner: IndexScopeTuner,
        loom_tuner: LoomScopeTuner,
        ab_runner: ABRunner<W, B>,
        storage: S,
    ) -> Self {
        config.mode = SoakMode::LiveTraffic;
        Self::with_parts(
            config,
            forge_tuner,
            index_tuner,
            loom_tuner,
            ab_runner,
            storage,
            SeededSoakProfile::default(),
        )
    }

    pub fn with_parts(
        config: SoakConfig,
        forge_tuner: ForgeScopeTuner,
        index_tuner: IndexScopeTuner,
        loom_tuner: LoomScopeTuner,
        ab_runner: ABRunner<W, B>,
        storage: S,
        profile: SeededSoakProfile,
    ) -> Self {
        Self {
            config,
            forge_tuner,
            index_tuner,
            loom_tuner,
            ab_runner,
            metrics: SoakMetrics::default(),
            storage,
            profile,
            last_report: None,
        }
    }

    pub fn with_seeded_profile(mut self, profile: SeededSoakProfile) -> Self {
        self.profile = profile;
        self
    }

    pub fn run<C>(&mut self, _vault: &AsterVault<C>) -> Result<SoakReport>
    where
        C: Clock,
    {
        validate_config(&self.config)?;
        if self.config.mode == SoakMode::LiveTraffic {
            return Err(live_traffic_unavailable());
        }
        validate_profile(self.profile)?;
        self.metrics.samples.clear();
        let run_id = self.run_id();
        let mut promotions = Vec::new();
        if self.config.n_queries == 0 {
            let report = self.report_for(0, promotions, false);
            self.storage.save_report(run_id, &report)?;
            self.last_report = Some(report.clone());
            return Ok(report);
        }

        let shape_key = soak_shape_key(self.config.seed);
        let mut bandit = soak_ab_bandit(self.config.seed);
        self.start_ab_trial(shape_key.clone(), &mut bandit)?;
        let mut promoted = false;
        let mut query_log = soak_query_log();
        let started_at = Instant::now();

        for query_count in 1..=self.config.n_queries {
            let incumbent = self.incumbent_result(query_count);
            let candidate = self.candidate_result(query_count);
            self.forge_tuner
                .on_op(shape_key.clone(), incumbent.latency_ns, incumbent.recall_k)?;
            self.index_tuner.on_search(
                SOAK_SLOT,
                incumbent.latency_ns,
                incumbent.recall_k,
                incumbent.bits_per_anchor,
            )?;
            match self
                .ab_runner
                .record_query(&shape_key, incumbent, candidate, &mut bandit)?
            {
                Some(ABVerdict::Promoted(record)) if !promoted => {
                    promotions.push(record.change_id);
                    promoted = true;
                }
                _ => {}
            }
            if self.should_sample(query_count) {
                let _ = self.loom_tuner.on_query_tick(&query_log)?;
                let sample = self.sample_at(query_count);
                self.metrics.samples.push(sample);
                if let Err(error) = self.storage.save_sample(run_id, &sample) {
                    let partial = self.report_for(query_count, promotions, false);
                    self.storage.save_report(run_id, &partial)?;
                    self.last_report = Some(partial);
                    return Err(error);
                }
                if self.runtime_exhausted(started_at) {
                    let partial = self.report_for(query_count, promotions, false);
                    self.storage.save_report(run_id, &partial)?;
                    self.last_report = Some(partial);
                    return Err(time_budget_exhausted(query_count));
                }
                query_log = soak_query_log();
            } else {
                query_log.push(soak_observation(query_count));
            }
        }

        let report = self.report_for(self.config.n_queries, promotions, true);
        self.storage.save_report(run_id, &report)?;
        self.last_report = Some(report.clone());
        Ok(report)
    }

    pub fn last_report(&self) -> Option<&SoakReport> {
        self.last_report.as_ref()
    }

    pub fn run_id(&self) -> [u8; 32] {
        let mut hasher = blake3::Hasher::new();
        hasher.update(&self.config.n_queries.to_be_bytes());
        hasher.update(&self.config.seed.to_be_bytes());
        hasher.update(format!("{:?}", self.config.mode).as_bytes());
        hasher.update(&self.profile.baseline_p99_ns.to_be_bytes());
        hasher.update(&self.profile.final_p99_ns.to_be_bytes());
        hasher.update(&self.profile.recall_baseline.to_bits().to_be_bytes());
        hasher.update(&self.profile.recall_final.to_bits().to_be_bytes());
        *hasher.finalize().as_bytes()
    }

    fn runtime_exhausted(&self, started_at: Instant) -> bool {
        self.config
            .max_runtime_ms
            .is_some_and(|budget| started_at.elapsed().as_millis() >= budget as u128)
    }

    fn start_ab_trial(&mut self, shape_key: ShapeKey, bandit: &mut ConfigBandit) -> Result<()> {
        let min_samples = self.config.n_queries.min(100) as usize;
        let candidate_config = ForgeConfig {
            tile_m: 128,
            tile_n: 128,
            tile_k: 64,
            dtype: DType::Fp32,
            batch_size: 2,
        };
        let promotion = ABPromotionConfig {
            key: shape_key.autotune_key(0.99),
            config: candidate_config.to_best_config(&shape_key),
        };
        self.ab_runner.start_trial_with_config(
            shape_key,
            CANDIDATE_ARM,
            INCUMBENT_ARM,
            min_samples.max(1),
            Some(promotion),
        )?;
        bandit.validate()
    }

    fn incumbent_result(&self, query_count: u64) -> ABResult {
        ABResult {
            arm_idx: INCUMBENT_ARM,
            latency_ns: self.profile.baseline_p99_ns,
            recall_k: self.profile.recall_baseline,
            bits_per_anchor: self.profile.bits_per_anchor,
            ts: query_count,
        }
    }

    fn candidate_result(&self, query_count: u64) -> ABResult {
        ABResult {
            arm_idx: CANDIDATE_ARM,
            latency_ns: self.profile.final_p99_ns,
            recall_k: self.profile.recall_final,
            bits_per_anchor: self.profile.bits_per_anchor,
            ts: query_count,
        }
    }

    fn should_sample(&self, query_count: u64) -> bool {
        query_count == self.config.n_queries
            || query_count.is_multiple_of(self.config.sample_interval)
    }

    fn sample_at(&self, query_count: u64) -> MetricSample {
        let total = self.config.n_queries.max(1);
        let progress = if query_count >= total {
            1.0
        } else {
            query_count as f64 / total as f64
        };
        MetricSample {
            p99_ns: interpolate_u64(
                self.profile.baseline_p99_ns,
                self.profile.final_p99_ns,
                progress,
            ),
            recall_10: interpolate_f64(
                self.profile.recall_baseline,
                self.profile.recall_final,
                progress,
            ),
            query_count,
        }
    }

    fn report_for(
        &self,
        total_queries: u64,
        promotions: Vec<ChangeId>,
        include_final: bool,
    ) -> SoakReport {
        let mut samples = self.metrics.samples.clone();
        if include_final
            && samples
                .last()
                .is_none_or(|sample| sample.query_count != total_queries)
        {
            samples.push(self.sample_at(total_queries));
        }
        let baseline = if total_queries == 0 {
            0
        } else {
            self.profile.baseline_p99_ns
        };
        let final_p99 = samples.last().map(|sample| sample.p99_ns).unwrap_or(0);
        let recall_baseline = if total_queries == 0 {
            0.0
        } else {
            self.profile.recall_baseline
        };
        let recall_final = samples.last().map(|sample| sample.recall_10).unwrap_or(0.0);
        let p99_reduction = p99_reduction(baseline, final_p99);
        let oscillation_detected = check_oscillation(&samples, self.config.oscillation_window);
        let min_recall = self.config.min_recall.max(recall_baseline);
        let gate_passed = p99_reduction + f64::EPSILON >= self.config.p99_target_reduction
            && recall_final + f64::EPSILON >= min_recall
            && !oscillation_detected;
        SoakReport {
            baseline_p99_ns: baseline,
            final_p99_ns: final_p99,
            p99_reduction,
            recall_baseline,
            recall_final,
            oscillation_detected,
            promotions,
            total_queries,
            samples,
            gate_passed,
            ts: total_queries,
        }
    }
}

pub fn check_oscillation(samples: &[MetricSample], window: u64) -> bool {
    let Some(last) = samples.last() else {
        return false;
    };
    let cutoff = last.query_count.saturating_sub(window);
    let mut previous = None;
    for sample in samples.iter().filter(|sample| sample.query_count >= cutoff) {
        if let Some(prev) = previous
            && sample.p99_ns as f64 > prev as f64 * 1.05
        {
            return true;
        }
        previous = Some(sample.p99_ns);
    }
    false
}

fn soak_ab_bandit(seed: u64) -> ConfigBandit {
    let mut bandit =
        ConfigBandit::new(BanditPolicy::EpsilonGreedy { epsilon: 0.0 }, seed).with_hysteresis(1);
    bandit.add_arm(b"soak-incumbent".to_vec());
    bandit.add_arm(b"soak-candidate".to_vec());
    bandit
}

fn soak_shape_key(seed: u64) -> ShapeKey {
    ShapeKey::new(
        format!("soak_gemm_{seed:x}"),
        &[128, 64],
        DType::Fp32,
        "cpu0",
    )
}

fn soak_query_log() -> QueryLog {
    let mut log = QueryLog::with_budgets(4, 4);
    log.push(soak_observation(0));
    log
}

fn soak_observation(query_count: u64) -> QueryObservation {
    let a = LensId::from_bytes([1; 16]);
    let b = LensId::from_bytes([2; 16]);
    QueryObservation::new(a, b, 1_000 + query_count % 7, 700, Some(650), 0.40)
}

fn interpolate_u64(start: u64, end: u64, progress: f64) -> u64 {
    interpolate_f64(start as f64, end as f64, progress)
        .round()
        .max(0.0) as u64
}

fn interpolate_f64(start: f64, end: f64, progress: f64) -> f64 {
    start + ((end - start) * progress.clamp(0.0, 1.0))
}

fn p99_reduction(baseline: u64, final_p99: u64) -> f64 {
    if baseline == 0 {
        return 0.0;
    }
    (baseline as f64 - final_p99 as f64) / baseline as f64
}

fn validate_config(config: &SoakConfig) -> Result<()> {
    if !config.p99_target_reduction.is_finite() || config.p99_target_reduction < 0.0 {
        return Err(invalid_config(
            "p99 target reduction must be finite and non-negative",
        ));
    }
    if !config.min_recall.is_finite() || !(0.0..=1.0).contains(&config.min_recall) {
        return Err(invalid_config("min_recall must be finite and within 0..=1"));
    }
    if config.sample_interval == 0 {
        return Err(invalid_config("sample_interval must be positive"));
    }
    if config.oscillation_window == 0 {
        return Err(invalid_config("oscillation_window must be positive"));
    }
    Ok(())
}

fn validate_profile(profile: SeededSoakProfile) -> Result<()> {
    if profile.baseline_p99_ns == 0 {
        return Err(invalid_config("baseline_p99_ns must be positive"));
    }
    if !profile.recall_baseline.is_finite()
        || !profile.recall_final.is_finite()
        || !(0.0..=1.0).contains(&profile.recall_baseline)
        || !(0.0..=1.0).contains(&profile.recall_final)
    {
        return Err(invalid_config(
            "seeded recall values must be finite and within 0..=1",
        ));
    }
    if !profile.bits_per_anchor.is_finite() || profile.bits_per_anchor < 0.0 {
        return Err(invalid_config(
            "seeded bits_per_anchor must be finite and non-negative",
        ));
    }
    Ok(())
}

fn invalid_config(message: impl Into<String>) -> CalyxError {
    CalyxError {
        code: CALYX_ANNEAL_SOAK_INVALID_CONFIG,
        message: message.into(),
        remediation: "repair SoakConfig or seeded soak profile before running the soak",
    }
}

fn live_traffic_unavailable() -> CalyxError {
    CalyxError {
        code: CALYX_ANNEAL_SOAK_LIVE_TRAFFIC_UNAVAILABLE,
        message: "live-traffic soak has no vault-backed replay measurement provider".to_string(),
        remediation: "install an independently measured vault-backed replay provider before selecting SoakMode::LiveTraffic; use SoakMode::Seeded only for explicit simulation",
    }
}

fn time_budget_exhausted(query_count: u64) -> CalyxError {
    CalyxError {
        code: CALYX_ANNEAL_SOAK_TIME_BUDGET_EXHAUSTED,
        message: format!("soak exceeded configured runtime budget after {query_count} queries"),
        remediation: "increase SoakConfig::max_runtime_ms or reduce SoakConfig::n_queries",
    }
}
