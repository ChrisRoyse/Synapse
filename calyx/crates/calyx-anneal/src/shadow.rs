use std::collections::HashMap;
use std::sync::Arc;

use calyx_core::{CalyxError, Clock, CxId, Result, Ts};
use rand::SeedableRng;
use rand::seq::SliceRandom;
use rand_chacha::ChaCha8Rng;
use serde::{Deserialize, Serialize};

use crate::{
    ArtifactKey, ArtifactPtr, BudgetHandle, TripwireMetric, TripwireRegistry, TripwireResult,
};

pub const CALYX_ANNEAL_SHADOW_MEASUREMENT_MISSING: &str = "CALYX_ANNEAL_SHADOW_MEASUREMENT_MISSING";

pub const ALL_SHADOW_METRICS: [TripwireMetric; 5] = [
    TripwireMetric::RecallAtK,
    TripwireMetric::GuardFAR,
    TripwireMetric::GuardFRR,
    TripwireMetric::SearchP99,
    TripwireMetric::IngestP95,
];

const COMPARE_EPSILON: f64 = 1e-12;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReplayAnchor {
    pub cx_id: CxId,
    pub similarity: f32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReplayQuery {
    pub query_id: u64,
    pub query_vector: Vec<f32>,
    pub expected_top_k: Vec<ReplayAnchor>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HeldOutReplay {
    pub queries: Vec<ReplayQuery>,
    pub seed: u64,
}

impl HeldOutReplay {
    pub fn sample(mut queries: Vec<ReplayQuery>, n: usize, seed: u64) -> Self {
        let mut rng = ChaCha8Rng::seed_from_u64(seed);
        queries.shuffle(&mut rng);
        queries.truncate(n.min(queries.len()));
        Self { queries, seed }
    }
}

pub trait ReplaySource {
    fn replay_queries(&self) -> Result<Vec<ReplayQuery>>;
}

pub fn build_replay<S>(source: &S, n: usize, seed: u64) -> Result<HeldOutReplay>
where
    S: ReplaySource + ?Sized,
{
    Ok(HeldOutReplay::sample(source.replay_queries()?, n, seed))
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ActionMetricSnapshot {
    pub values: Vec<ActionMetricValue>,
}

impl ActionMetricSnapshot {
    pub fn from_values(values: impl IntoIterator<Item = (TripwireMetric, f64)>) -> Self {
        Self {
            values: values
                .into_iter()
                .map(|(metric, value)| ActionMetricValue { metric, value })
                .collect(),
        }
    }

    fn value(&self, metric: TripwireMetric) -> Option<f64> {
        self.values
            .iter()
            .find(|value| value.metric == metric)
            .map(|value| value.value)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct ActionMetricValue {
    pub metric: TripwireMetric,
    pub value: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct MetricComparison {
    pub metric: TripwireMetric,
    pub candidate_value: f64,
    pub incumbent_value: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MetricSnapshot {
    pub evaluated_at: Ts,
    pub query_count: usize,
    pub metrics: Vec<MetricComparison>,
}

impl MetricSnapshot {
    pub fn empty(evaluated_at: Ts) -> Self {
        Self {
            evaluated_at,
            query_count: 0,
            metrics: Vec::new(),
        }
    }
}

pub trait AnnealAction: Send + Sync {
    fn apply_shadow(&self, query: &ReplayQuery) -> Result<ActionMetricSnapshot>;
}

pub trait ArtifactReplayMeasurer: Send + Sync {
    fn measure(
        &self,
        key: &ArtifactKey,
        artifact: &ArtifactPtr,
        query: &ReplayQuery,
    ) -> Result<ActionMetricSnapshot>;
}

#[derive(Clone)]
pub(crate) struct ArtifactShadowAction {
    key: ArtifactKey,
    artifact: ArtifactPtr,
    measurer: Option<Arc<dyn ArtifactReplayMeasurer>>,
}

impl ArtifactShadowAction {
    pub(crate) fn new(
        key: ArtifactKey,
        artifact: ArtifactPtr,
        measurer: Option<Arc<dyn ArtifactReplayMeasurer>>,
    ) -> Self {
        Self {
            key,
            artifact,
            measurer,
        }
    }
}

impl AnnealAction for ArtifactShadowAction {
    fn apply_shadow(&self, query: &ReplayQuery) -> Result<ActionMetricSnapshot> {
        let measurer = self
            .measurer
            .as_ref()
            .ok_or_else(|| missing_artifact_measurement(&self.key, &self.artifact))?;
        measurer.measure(&self.key, &self.artifact, query)
    }
}

pub struct ShadowExecutor<'a> {
    pub registry: TripwireRegistry,
    pub replay: HeldOutReplay,
    pub budget: BudgetHandle,
    required_metrics: Vec<TripwireMetric>,
    clock: &'a dyn Clock,
}

impl<'a> ShadowExecutor<'a> {
    pub fn new(
        registry: TripwireRegistry,
        replay: HeldOutReplay,
        budget: BudgetHandle,
        clock: &'a dyn Clock,
    ) -> Self {
        Self {
            registry,
            replay,
            budget,
            required_metrics: ALL_SHADOW_METRICS.to_vec(),
            clock,
        }
    }

    pub fn with_required_metrics(mut self, required_metrics: Vec<TripwireMetric>) -> Self {
        self.required_metrics = required_metrics;
        self
    }

    pub fn run_shadow<C, I>(&mut self, candidate: &C, incumbent: &I) -> ShadowVerdict
    where
        C: AnnealAction,
        I: AnnealAction,
    {
        let evaluated_at = self.clock.now();
        if self.replay.queries.is_empty() {
            return ShadowVerdict::Revert {
                reason: ShadowRevertReason::InsufficientReplay,
                metrics: MetricSnapshot::empty(evaluated_at),
            };
        }

        if self.required_metrics.is_empty() {
            return ShadowVerdict::Revert {
                reason: ShadowRevertReason::NoRequiredMetrics,
                metrics: MetricSnapshot::empty(evaluated_at),
            };
        }
        let mut unique = Vec::with_capacity(self.required_metrics.len());
        for metric in &self.required_metrics {
            if unique.contains(metric) {
                return ShadowVerdict::Revert {
                    reason: ShadowRevertReason::DuplicateRequiredMetric(*metric),
                    metrics: MetricSnapshot::empty(evaluated_at),
                };
            }
            unique.push(*metric);
        }

        let mut accumulator = MetricAccumulator::new(unique);
        for (query_index, query) in self.replay.queries.iter().enumerate() {
            if !self.budget.try_consume() {
                return ShadowVerdict::Revert {
                    reason: ShadowRevertReason::BudgetExhausted {
                        evaluated_queries: query_index,
                        replay_queries: self.replay.queries.len(),
                    },
                    metrics: accumulator.snapshot(evaluated_at),
                };
            }
            let candidate_snapshot = match candidate.apply_shadow(query) {
                Ok(snapshot) => snapshot,
                Err(error) => {
                    return ShadowVerdict::Revert {
                        reason: ShadowRevertReason::MeasurementFailed {
                            side: MetricSide::Candidate,
                            code: error.code.to_string(),
                        },
                        metrics: accumulator.snapshot(evaluated_at),
                    };
                }
            };
            let incumbent_snapshot = match incumbent.apply_shadow(query) {
                Ok(snapshot) => snapshot,
                Err(error) => {
                    return ShadowVerdict::Revert {
                        reason: ShadowRevertReason::MeasurementFailed {
                            side: MetricSide::Incumbent,
                            code: error.code.to_string(),
                        },
                        metrics: accumulator.snapshot(evaluated_at),
                    };
                }
            };
            if let Some(reason) = accumulator.add_query(&candidate_snapshot, &incumbent_snapshot) {
                return ShadowVerdict::Revert {
                    reason,
                    metrics: accumulator.snapshot(evaluated_at),
                };
            }
        }

        let metrics = accumulator.snapshot(evaluated_at);
        for comparison in &metrics.metrics {
            match self
                .registry
                .check(comparison.metric, comparison.candidate_value)
            {
                Ok(TripwireResult::Ok) => {}
                Ok(TripwireResult::Crossed { metric, .. }) => {
                    return ShadowVerdict::Revert {
                        reason: ShadowRevertReason::TripwireCrossed(metric),
                        metrics,
                    };
                }
                Err(error) => {
                    return ShadowVerdict::Revert {
                        reason: ShadowRevertReason::TripwireError {
                            metric: comparison.metric,
                            code: error.code.to_string(),
                        },
                        metrics,
                    };
                }
            }
            if regressed(*comparison) {
                return ShadowVerdict::Revert {
                    reason: ShadowRevertReason::MetricRegression(comparison.metric),
                    metrics,
                };
            }
        }

        ShadowVerdict::Promote { metrics }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum ShadowVerdict {
    Promote {
        metrics: MetricSnapshot,
    },
    Revert {
        reason: ShadowRevertReason,
        metrics: MetricSnapshot,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reason", content = "details", rename_all = "snake_case")]
pub enum ShadowRevertReason {
    TripwireCrossed(TripwireMetric),
    MetricRegression(TripwireMetric),
    BudgetExhausted {
        evaluated_queries: usize,
        replay_queries: usize,
    },
    InsufficientReplay,
    NoRequiredMetrics,
    DuplicateRequiredMetric(TripwireMetric),
    MissingMetric {
        metric: TripwireMetric,
        side: MetricSide,
    },
    InvalidMetric {
        metric: TripwireMetric,
        side: MetricSide,
    },
    MeasurementFailed {
        side: MetricSide,
        code: String,
    },
    TripwireError {
        metric: TripwireMetric,
        code: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MetricSide {
    Candidate,
    Incumbent,
}

struct MetricAccumulator {
    required_metrics: Vec<TripwireMetric>,
    totals: HashMap<TripwireMetric, MetricTotals>,
    query_count: usize,
}

impl MetricAccumulator {
    fn new(required_metrics: Vec<TripwireMetric>) -> Self {
        Self {
            required_metrics,
            totals: HashMap::new(),
            query_count: 0,
        }
    }

    fn add_query(
        &mut self,
        candidate: &ActionMetricSnapshot,
        incumbent: &ActionMetricSnapshot,
    ) -> Option<ShadowRevertReason> {
        let mut pairs = Vec::with_capacity(self.required_metrics.len());
        for metric in self.required_metrics.iter().copied() {
            let candidate_value = match metric_value(candidate, metric, MetricSide::Candidate) {
                Ok(value) => value,
                Err(reason) => return Some(reason),
            };
            let incumbent_value = match metric_value(incumbent, metric, MetricSide::Incumbent) {
                Ok(value) => value,
                Err(reason) => return Some(reason),
            };
            if !candidate_value.is_finite() {
                return Some(ShadowRevertReason::InvalidMetric {
                    metric,
                    side: MetricSide::Candidate,
                });
            }
            if !incumbent_value.is_finite() {
                return Some(ShadowRevertReason::InvalidMetric {
                    metric,
                    side: MetricSide::Incumbent,
                });
            }
            pairs.push((metric, candidate_value, incumbent_value));
        }
        for (metric, candidate_value, incumbent_value) in pairs {
            self.totals
                .entry(metric)
                .or_default()
                .add(candidate_value, incumbent_value);
        }
        self.query_count += 1;
        None
    }

    fn snapshot(&self, evaluated_at: Ts) -> MetricSnapshot {
        let metrics = self
            .required_metrics
            .iter()
            .filter_map(|metric| self.totals.get(metric).map(|totals| (*metric, totals)))
            .map(|(metric, totals)| MetricComparison {
                metric,
                candidate_value: totals.candidate_total / totals.count as f64,
                incumbent_value: totals.incumbent_total / totals.count as f64,
            })
            .collect();
        MetricSnapshot {
            evaluated_at,
            query_count: self.query_count,
            metrics,
        }
    }
}

fn metric_value(
    snapshot: &ActionMetricSnapshot,
    metric: TripwireMetric,
    side: MetricSide,
) -> std::result::Result<f64, ShadowRevertReason> {
    snapshot
        .value(metric)
        .ok_or(ShadowRevertReason::MissingMetric { metric, side })
}

#[derive(Default)]
struct MetricTotals {
    candidate_total: f64,
    incumbent_total: f64,
    count: usize,
}

impl MetricTotals {
    fn add(&mut self, candidate_value: f64, incumbent_value: f64) {
        self.candidate_total += candidate_value;
        self.incumbent_total += incumbent_value;
        self.count += 1;
    }
}

fn regressed(comparison: MetricComparison) -> bool {
    match comparison.metric {
        TripwireMetric::RecallAtK => {
            comparison.candidate_value + COMPARE_EPSILON < comparison.incumbent_value
        }
        TripwireMetric::GuardFAR
        | TripwireMetric::GuardFRR
        | TripwireMetric::SearchP99
        | TripwireMetric::IngestP95 => {
            comparison.candidate_value > comparison.incumbent_value + COMPARE_EPSILON
        }
    }
}

fn missing_artifact_measurement(key: &ArtifactKey, artifact: &ArtifactPtr) -> CalyxError {
    CalyxError {
        code: CALYX_ANNEAL_SHADOW_MEASUREMENT_MISSING,
        message: format!(
            "no replay measurer configured for artifact key {key:?} pointer {artifact:?}"
        ),
        remediation: "measure candidate and incumbent on the held-out replay before proposing the artifact",
    }
}
