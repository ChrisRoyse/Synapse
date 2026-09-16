use std::sync::{Once, OnceLock};

pub use ::metrics::{
    Unit, counter, describe_counter, describe_gauge, describe_histogram, gauge, histogram,
};
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};
use thiserror::Error;

pub const CARDINALITY_LIMIT: u16 = 1_000;

pub const EVENTS_DROPPED_FOR_SUBSCRIBER: &str = "events_dropped_for_subscriber";
pub const EVENTS_PUBLISHED_TOTAL: &str = "events_published_total";
pub const REFLEX_FIRES_TOTAL: &str = "reflex_fires_total";
pub const REFLEX_TICK_JITTER_US: &str = "reflex_tick_jitter_us";
pub const REFLEX_RECURSION_CLAMPS_TOTAL: &str = "reflex_recursion_clamps_total";
pub const REFLEX_STARVED_TOTAL: &str = "reflex_starved_total";
pub const CACHE_EVICTIONS_TOTAL: &str = "cache_evictions_total";
pub const STORAGE_DISK_PRESSURE_LEVEL: &str = "storage_disk_pressure_level";
pub const STORAGE_CF_BYTES: &str = "storage_cf_bytes";
pub const STORAGE_WRITE_BATCH_FLUSHES_TOTAL: &str = "storage_write_batch_flushes_total";
pub const PROFILES_ACTIVE: &str = "profiles_active";
pub const PROFILE_RELOADS_TOTAL: &str = "profile_reloads_total";
pub const AUDIO_LOOPBACK_UNDERRUNS_TOTAL: &str = "audio_loopback_underruns_total";
pub const AUDIO_STT_INFERENCES_TOTAL: &str = "audio_stt_inferences_total";
pub const AUDIO_STT_LATENCY_MS: &str = "audio_stt_latency_ms";
pub const HTTP_REQUESTS_TOTAL: &str = "http_requests_total";
pub const HTTP_ACTIVE_SESSIONS: &str = "http_active_sessions";
pub const SSE_ACTIVE_SUBSCRIBERS: &str = "sse_active_subscribers";
pub const SSE_BUFFER_OVERFLOWS_TOTAL: &str = "sse_buffer_overflows_total";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MetricKind {
    Counter,
    Gauge,
    Histogram,
}

impl MetricKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Counter => "counter",
            Self::Gauge => "gauge",
            Self::Histogram => "histogram",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MetricSpec {
    pub name: &'static str,
    pub kind: MetricKind,
    pub unit: Option<Unit>,
    pub labels: &'static [&'static str],
    pub max_label_combinations: u16,
    pub label_policy: &'static str,
    pub description: &'static str,
}

impl MetricSpec {
    #[must_use]
    pub const fn has_bounded_cardinality(self) -> bool {
        self.max_label_combinations < CARDINALITY_LIMIT
    }

    #[must_use]
    pub fn unit_name(self) -> &'static str {
        self.unit.map_or("none", |unit| unit.as_str())
    }
}

pub const M3_METRICS: &[MetricSpec] = &[
    MetricSpec {
        name: EVENTS_DROPPED_FOR_SUBSCRIBER,
        kind: MetricKind::Counter,
        unit: Some(Unit::Count),
        labels: &["subscription_id"],
        max_label_combinations: 64,
        label_policy: "bounded subscriber slot, not raw UUID",
        description: "Events dropped by a bounded per-subscriber event queue.",
    },
    MetricSpec {
        name: EVENTS_PUBLISHED_TOTAL,
        kind: MetricKind::Counter,
        unit: Some(Unit::Count),
        labels: &["source", "kind"],
        max_label_combinations: 832,
        label_policy: "EventSource enum by normalized M3 event kind bucket.",
        description: "Events published onto the M3 event bus.",
    },
    MetricSpec {
        name: REFLEX_FIRES_TOTAL,
        kind: MetricKind::Counter,
        unit: Some(Unit::Count),
        labels: &["kind", "reflex_id"],
        max_label_combinations: 64,
        label_policy: "reflex kind by bounded active reflex slot, not raw reflex UUID.",
        description: "Reflex fire outcomes accepted by the scheduler.",
    },
    MetricSpec {
        name: REFLEX_TICK_JITTER_US,
        kind: MetricKind::Histogram,
        unit: Some(Unit::Microseconds),
        labels: &[],
        max_label_combinations: 1,
        label_policy: "unlabeled scheduler distribution.",
        description: "Reflex scheduler tick jitter in microseconds.",
    },
    MetricSpec {
        name: REFLEX_RECURSION_CLAMPS_TOTAL,
        kind: MetricKind::Counter,
        unit: Some(Unit::Count),
        labels: &[],
        max_label_combinations: 1,
        label_policy: "unlabeled recursion-guard clamp counter.",
        description: "Times the on-event recursion guard clamped reflex firing.",
    },
    MetricSpec {
        name: REFLEX_STARVED_TOTAL,
        kind: MetricKind::Counter,
        unit: Some(Unit::Count),
        labels: &["reflex_id"],
        max_label_combinations: 32,
        label_policy: "bounded active reflex slot, not raw reflex UUID.",
        description: "Reflexes marked starved by conflict resolution.",
    },
    MetricSpec {
        name: CACHE_EVICTIONS_TOTAL,
        kind: MetricKind::Counter,
        unit: Some(Unit::Count),
        labels: &["cf", "reason"],
        max_label_combinations: 64,
        label_policy: "column-family closed set by eviction reason closed set.",
        description: "Rows evicted from storage caches or column-family retention.",
    },
    MetricSpec {
        name: STORAGE_DISK_PRESSURE_LEVEL,
        kind: MetricKind::Gauge,
        unit: Some(Unit::Count),
        labels: &[],
        max_label_combinations: 1,
        label_policy: "unlabeled gauge; value is 0..4.",
        description: "Current storage disk pressure level.",
    },
    MetricSpec {
        name: STORAGE_CF_BYTES,
        kind: MetricKind::Gauge,
        unit: Some(Unit::Bytes),
        labels: &["cf"],
        max_label_combinations: 16,
        label_policy: "column-family closed set.",
        description: "Estimated live bytes per storage column family.",
    },
    MetricSpec {
        name: STORAGE_WRITE_BATCH_FLUSHES_TOTAL,
        kind: MetricKind::Counter,
        unit: Some(Unit::Count),
        labels: &["trigger"],
        max_label_combinations: 8,
        label_policy: "flush trigger closed set.",
        description: "Storage write batch flushes by trigger.",
    },
    MetricSpec {
        name: PROFILES_ACTIVE,
        kind: MetricKind::Gauge,
        unit: Some(Unit::Count),
        labels: &["profile_id"],
        max_label_combinations: 128,
        label_policy: "bundled/operator profile ID set capped by loaded profile count.",
        description: "Active profile marker, 1 for active and 0 for inactive.",
    },
    MetricSpec {
        name: PROFILE_RELOADS_TOTAL,
        kind: MetricKind::Counter,
        unit: Some(Unit::Count),
        labels: &["profile_id", "outcome"],
        max_label_combinations: 256,
        label_policy: "loaded profile ID cap by reload outcome closed set.",
        description: "Profile reload attempts by profile and outcome.",
    },
    MetricSpec {
        name: AUDIO_LOOPBACK_UNDERRUNS_TOTAL,
        kind: MetricKind::Counter,
        unit: Some(Unit::Count),
        labels: &[],
        max_label_combinations: 1,
        label_policy: "unlabeled loopback underrun counter.",
        description: "Audio loopback underruns observed while reading the ring.",
    },
    MetricSpec {
        name: AUDIO_STT_INFERENCES_TOTAL,
        kind: MetricKind::Counter,
        unit: Some(Unit::Count),
        labels: &["outcome"],
        max_label_combinations: 8,
        label_policy: "STT outcome closed set.",
        description: "Speech-to-text inference attempts by outcome.",
    },
    MetricSpec {
        name: AUDIO_STT_LATENCY_MS,
        kind: MetricKind::Histogram,
        unit: Some(Unit::Milliseconds),
        labels: &[],
        max_label_combinations: 1,
        label_policy: "unlabeled STT latency distribution.",
        description: "Speech-to-text inference latency in milliseconds.",
    },
    MetricSpec {
        name: HTTP_REQUESTS_TOTAL,
        kind: MetricKind::Counter,
        unit: Some(Unit::Count),
        labels: &["path", "status"],
        max_label_combinations: 64,
        label_policy: "normalized route path by status-code class/closed status set.",
        description: "HTTP transport requests by normalized path and status.",
    },
    MetricSpec {
        name: HTTP_ACTIVE_SESSIONS,
        kind: MetricKind::Gauge,
        unit: Some(Unit::Count),
        labels: &[],
        max_label_combinations: 1,
        label_policy: "unlabeled active HTTP session gauge.",
        description: "Currently active streamable HTTP MCP sessions.",
    },
    MetricSpec {
        name: SSE_ACTIVE_SUBSCRIBERS,
        kind: MetricKind::Gauge,
        unit: Some(Unit::Count),
        labels: &[],
        max_label_combinations: 1,
        label_policy: "unlabeled active SSE subscriber gauge.",
        description: "Currently active SSE event subscribers.",
    },
    MetricSpec {
        name: SSE_BUFFER_OVERFLOWS_TOTAL,
        kind: MetricKind::Counter,
        unit: Some(Unit::Count),
        labels: &[],
        max_label_combinations: 1,
        label_policy: "unlabeled SSE ring overflow counter.",
        description: "SSE ring buffer overflows.",
    },
];

static REGISTER_M3_METRICS: Once = Once::new();
static PROMETHEUS_RECORDER: OnceLock<PrometheusHandle> = OnceLock::new();

pub const PROMETHEUS_RECORDER_SOURCE_OF_TRUTH: &str =
    "process-global metrics recorder + metrics_exporter_prometheus PrometheusHandle::render";

#[derive(Debug, Error)]
pub enum MetricsRecorderError {
    #[error("METRICS_RECORDER_INSTALL_FAILED: {0}")]
    Install(String),
}

#[must_use]
pub fn prometheus_recorder_installed() -> bool {
    PROMETHEUS_RECORDER.get().is_some()
}

/// Installs the process-global metrics recorder once and returns its render handle.
///
/// # Errors
///
/// Returns [`MetricsRecorderError`] when the exporter cannot build or when
/// another global recorder has already been installed outside this module.
pub fn install_prometheus_recorder() -> Result<PrometheusHandle, MetricsRecorderError> {
    if let Some(handle) = PROMETHEUS_RECORDER.get() {
        return Ok(handle.clone());
    }

    let handle = match PrometheusBuilder::new().install_recorder() {
        Ok(handle) => handle,
        Err(error) => {
            if let Some(handle) = PROMETHEUS_RECORDER.get() {
                return Ok(handle.clone());
            }
            return Err(MetricsRecorderError::Install(error.to_string()));
        }
    };
    if PROMETHEUS_RECORDER.set(handle.clone()).is_ok() {
        Ok(handle)
    } else if let Some(handle) = PROMETHEUS_RECORDER.get() {
        Ok(handle.clone())
    } else {
        Err(MetricsRecorderError::Install(
            "prometheus recorder handle could not be published".to_owned(),
        ))
    }
}

#[must_use]
pub fn render_prometheus() -> Option<String> {
    PROMETHEUS_RECORDER
        .get()
        .map(metrics_exporter_prometheus::PrometheusHandle::render)
}

pub fn register_m3_metrics() {
    REGISTER_M3_METRICS.call_once(|| {
        for spec in M3_METRICS {
            describe_metric(*spec);
            tracing::info!(
                code = "M3_METRIC_REGISTERED",
                metric_name = spec.name,
                metric_kind = spec.kind.as_str(),
                unit = spec.unit_name(),
                labels = ?spec.labels,
                max_label_combinations = spec.max_label_combinations,
                label_policy = spec.label_policy,
                "M3 metric registered"
            );
        }
        tracing::info!(
            code = "M3_METRICS_REGISTERED",
            metric_count = M3_METRICS.len(),
            cardinality_limit = CARDINALITY_LIMIT,
            "M3 metric registry ready"
        );
    });
}

#[must_use]
pub const fn m3_metric_specs() -> &'static [MetricSpec] {
    M3_METRICS
}

fn describe_metric(spec: MetricSpec) {
    match (spec.kind, spec.unit) {
        (MetricKind::Counter, Some(unit)) => describe_counter!(spec.name, unit, spec.description),
        (MetricKind::Counter, None) => describe_counter!(spec.name, spec.description),
        (MetricKind::Gauge, Some(unit)) => describe_gauge!(spec.name, unit, spec.description),
        (MetricKind::Gauge, None) => describe_gauge!(spec.name, spec.description),
        (MetricKind::Histogram, Some(unit)) => {
            describe_histogram!(spec.name, unit, spec.description);
        }
        (MetricKind::Histogram, None) => describe_histogram!(spec.name, spec.description),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::{
        AUDIO_LOOPBACK_UNDERRUNS_TOTAL, AUDIO_STT_INFERENCES_TOTAL, AUDIO_STT_LATENCY_MS,
        CACHE_EVICTIONS_TOTAL, CARDINALITY_LIMIT, EVENTS_DROPPED_FOR_SUBSCRIBER,
        EVENTS_PUBLISHED_TOTAL, HTTP_ACTIVE_SESSIONS, HTTP_REQUESTS_TOTAL, MetricKind,
        PROFILE_RELOADS_TOTAL, PROFILES_ACTIVE, REFLEX_FIRES_TOTAL, REFLEX_RECURSION_CLAMPS_TOTAL,
        REFLEX_STARVED_TOTAL, REFLEX_TICK_JITTER_US, SSE_ACTIVE_SUBSCRIBERS,
        SSE_BUFFER_OVERFLOWS_TOTAL, STORAGE_CF_BYTES, STORAGE_DISK_PRESSURE_LEVEL,
        STORAGE_WRITE_BATCH_FLUSHES_TOTAL, counter, install_prometheus_recorder, m3_metric_specs,
        register_m3_metrics, render_prometheus,
    };

    #[test]
    fn m3_registry_contains_exact_issue_metrics() {
        let expected = BTreeSet::from([
            EVENTS_DROPPED_FOR_SUBSCRIBER,
            EVENTS_PUBLISHED_TOTAL,
            REFLEX_FIRES_TOTAL,
            REFLEX_TICK_JITTER_US,
            REFLEX_RECURSION_CLAMPS_TOTAL,
            REFLEX_STARVED_TOTAL,
            CACHE_EVICTIONS_TOTAL,
            STORAGE_DISK_PRESSURE_LEVEL,
            STORAGE_CF_BYTES,
            STORAGE_WRITE_BATCH_FLUSHES_TOTAL,
            PROFILES_ACTIVE,
            PROFILE_RELOADS_TOTAL,
            AUDIO_LOOPBACK_UNDERRUNS_TOTAL,
            AUDIO_STT_INFERENCES_TOTAL,
            AUDIO_STT_LATENCY_MS,
            HTTP_REQUESTS_TOTAL,
            HTTP_ACTIVE_SESSIONS,
            SSE_ACTIVE_SUBSCRIBERS,
            SSE_BUFFER_OVERFLOWS_TOTAL,
        ]);
        let actual = m3_metric_specs()
            .iter()
            .map(|spec| spec.name)
            .collect::<BTreeSet<_>>();
        assert_eq!(actual, expected);
        assert_eq!(actual.len(), 19);
    }

    #[test]
    fn m3_registry_cardinality_budgets_stay_bounded() {
        for spec in m3_metric_specs() {
            assert!(
                spec.has_bounded_cardinality(),
                "{} permits {} label combinations, expected < {}",
                spec.name,
                spec.max_label_combinations,
                CARDINALITY_LIMIT
            );
            assert!(
                !spec.description.is_empty(),
                "{} must carry a metric description",
                spec.name
            );
            assert!(
                !spec.label_policy.is_empty(),
                "{} must document how labels are bounded",
                spec.name
            );
        }
    }

    #[test]
    fn installed_prometheus_recorder_renders_counter_samples()
    -> Result<(), Box<dyn std::error::Error>> {
        let _handle = install_prometheus_recorder()?;
        register_m3_metrics();
        counter!(
            EVENTS_PUBLISHED_TOTAL,
            "source" => "telemetry_test",
            "kind" => "smoke"
        )
        .increment(1);

        let rendered = render_prometheus().ok_or("prometheus recorder render handle missing")?;
        assert!(rendered.contains(EVENTS_PUBLISHED_TOTAL), "{rendered}");
        assert!(rendered.contains("source=\"telemetry_test\""), "{rendered}");
        assert!(rendered.contains("kind=\"smoke\""), "{rendered}");
        Ok(())
    }

    #[test]
    fn m3_registry_keeps_expected_metric_kinds() {
        let specs = m3_metric_specs();
        assert_eq!(
            specs
                .iter()
                .filter(|spec| matches!(spec.kind, MetricKind::Counter))
                .count(),
            12
        );
        assert_eq!(
            specs
                .iter()
                .filter(|spec| matches!(spec.kind, MetricKind::Gauge))
                .count(),
            5
        );
        assert_eq!(
            specs
                .iter()
                .filter(|spec| matches!(spec.kind, MetricKind::Histogram))
                .count(),
            2
        );
    }
}
