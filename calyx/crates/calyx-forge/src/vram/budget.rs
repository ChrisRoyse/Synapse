//! The VRAM budgeter: soft-cap config, live free-VRAM admission, and atomic
//! usage accounting with RAII release.
//!
//! See [`crate::vram`] for the design rationale (why the probe is injectable).

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use crate::vram::{VramProbe, VramStats};
use crate::{ForgeError, Result};

/// Default soft cap when `CALYX_FORGE_VRAM_BUDGET` is unset: 12 GiB. Leaves
/// ~20 GiB of the 32 GiB device for the three resident TEI containers.
pub const DEFAULT_SOFT_CAP_BYTES: usize = 12 * 1024 * 1024 * 1024;

/// Headroom reserved below the live free-VRAM figure for driver/runtime
/// overhead and allocator fragmentation: 512 MiB.
pub const RESERVED_HEADROOM_BYTES: usize = 512 * 1024 * 1024;

/// Environment variable that configures the soft cap (bytes, decimal).
pub const VRAM_BUDGET_ENV: &str = "CALYX_FORGE_VRAM_BUDGET";

/// Operator remediation attached to every `CALYX_FORGE_VRAM_BUDGET` error.
pub const VRAM_BUDGET_REMEDIATION: &str = "Forge VRAM budget exceeded; reduce batch size or wait for eviction; set CALYX_FORGE_VRAM_BUDGET env var (bytes)";

/// VRAM reservation owner. Serving is the default path; Anneal is the capped
/// background lane that must not crowd out serving/TEI work.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
pub enum Category {
    Serving,
    Anneal,
}

impl Category {
    fn as_str(self) -> &'static str {
        match self {
            Self::Serving => "serving",
            Self::Anneal => "anneal",
        }
    }
}

/// Enforces a soft cap on Forge's cumulative GPU allocation and consults live
/// device free-VRAM before admitting a dispatch.
pub struct VramBudgeter<P: VramProbe> {
    soft_cap_bytes: usize,
    allocated_bytes: AtomicUsize,
    serving_allocated_bytes: AtomicUsize,
    anneal_allocated_bytes: AtomicUsize,
    admission_splits_total: AtomicU64,
    admission_failed_total: AtomicU64,
    oom_intercepts_total: AtomicU64,
    oom_batch_reductions_total: AtomicU64,
    oom_final_failures_total: AtomicU64,
    anneal_throttle_events_total: AtomicU64,
    anneal_vram_rejections_total: AtomicU64,
    probe: P,
}

impl<P: VramProbe> VramBudgeter<P> {
    /// Construct with an explicit soft cap (bytes) and a probe.
    pub fn with_soft_cap(soft_cap_bytes: usize, probe: P) -> Self {
        Self {
            soft_cap_bytes,
            allocated_bytes: AtomicUsize::new(0),
            serving_allocated_bytes: AtomicUsize::new(0),
            anneal_allocated_bytes: AtomicUsize::new(0),
            admission_splits_total: AtomicU64::new(0),
            admission_failed_total: AtomicU64::new(0),
            oom_intercepts_total: AtomicU64::new(0),
            oom_batch_reductions_total: AtomicU64::new(0),
            oom_final_failures_total: AtomicU64::new(0),
            anneal_throttle_events_total: AtomicU64::new(0),
            anneal_vram_rejections_total: AtomicU64::new(0),
            probe,
        }
    }

    /// Construct from the environment.
    ///
    /// `CALYX_FORGE_VRAM_BUDGET` unset -> [`DEFAULT_SOFT_CAP_BYTES`] (12 GiB).
    /// Set to a decimal byte count -> that value. Non-integer -> fail closed
    /// with `CALYX_FORGE_VRAM_BUDGET`.
    pub fn from_env(probe: P) -> Result<Self> {
        let raw = std::env::var(VRAM_BUDGET_ENV).ok();
        let soft_cap_bytes = parse_soft_cap_strict(raw.as_deref())?;
        tracing::info!(
            target: "calyx_forge::vram",
            soft_cap_bytes,
            source = if raw.is_some() { "env" } else { "default" },
            "VRAM budgeter configured"
        );
        Ok(Self::with_soft_cap(soft_cap_bytes, probe))
    }

    /// The configured soft cap in bytes.
    pub fn soft_cap_bytes(&self) -> usize {
        self.soft_cap_bytes
    }

    /// Forge's currently reserved total in bytes (sum of live guards).
    pub fn allocated_bytes(&self) -> usize {
        self.allocated_bytes.load(Ordering::Acquire)
    }

    /// Forge's currently reserved bytes for one owner category.
    pub fn allocated_bytes_for(&self, category: Category) -> usize {
        self.category_counter(category).load(Ordering::Acquire)
    }

    /// Check, without reserving, whether `bytes` could be allocated now.
    pub fn can_allocate(&self, bytes: usize) -> Result<()> {
        if bytes == 0 {
            return Ok(());
        }
        let current = self.allocated_bytes.load(Ordering::Acquire);
        self.check_soft_cap(current, bytes)?;
        self.check_device_headroom_with_free(bytes, self.device_free_vram()?)?;
        Ok(())
    }

    pub(crate) fn can_allocate_with_device_free(
        &self,
        bytes: usize,
        device_free_bytes: usize,
    ) -> Result<()> {
        if bytes == 0 {
            return Ok(());
        }
        let current = self.allocated_bytes.load(Ordering::Acquire);
        self.check_soft_cap(current, bytes)?;
        self.check_device_headroom_with_free(bytes, device_free_bytes)
    }

    pub(crate) fn device_free_vram(&self) -> Result<usize> {
        self.probe.free_device_vram().map_err(|err| {
            budget_err(format!(
                "device free-VRAM query failed; treating unknown device state as over-budget: {err}"
            ))
        })
    }

    /// Reserve serving/default VRAM, returning an RAII guard.
    pub fn reserve(&self, bytes: usize) -> Result<VramGuard<'_, P>> {
        self.reserve_category(bytes, Category::Serving)
    }

    /// Reserve `bytes` for a specific owner category.
    pub fn reserve_category(&self, bytes: usize, category: Category) -> Result<VramGuard<'_, P>> {
        self.reserve_category_inner(bytes, category, None)
    }

    pub(crate) fn reserve_category_with_cap(
        &self,
        bytes: usize,
        category: Category,
        category_cap_bytes: usize,
    ) -> Result<VramGuard<'_, P>> {
        self.reserve_category_inner(bytes, category, Some(category_cap_bytes))
    }

    fn reserve_category_inner(
        &self,
        bytes: usize,
        category: Category,
        category_cap_bytes: Option<usize>,
    ) -> Result<VramGuard<'_, P>> {
        if bytes == 0 {
            return Ok(VramGuard {
                budgeter: self,
                bytes: 0,
                category,
            });
        }

        self.check_device_headroom(bytes)?;
        self.reserve_total(bytes)?;
        if let Err(err) = self.reserve_category_counter(category, bytes, category_cap_bytes) {
            self.allocated_bytes.fetch_sub(bytes, Ordering::AcqRel);
            return Err(err);
        }

        Ok(VramGuard {
            budgeter: self,
            bytes,
            category,
        })
    }

    /// Snapshot accounting + live device free VRAM.
    pub fn stats(&self) -> VramStats {
        let device_free_bytes = match self.probe.free_device_vram() {
            Ok(free) => free,
            Err(err) => {
                tracing::warn!(
                    target: "calyx_forge::vram",
                    error = %err,
                    "free-VRAM probe failed during stats(); reporting device_free_bytes=0"
                );
                0
            }
        };
        self.stats_with_device_free(device_free_bytes)
    }

    /// Snapshot accounting with a fail-closed live device read.
    ///
    /// Unlike [`Self::stats`], this method never substitutes a sentinel for an
    /// unknown physical state. Runtime health/admission gates should use this
    /// strict form whenever the readback itself is part of the verdict.
    pub fn stats_strict(&self) -> Result<VramStats> {
        let device_free_bytes = self.device_free_vram()?;
        Ok(self.stats_with_device_free(device_free_bytes))
    }

    fn stats_with_device_free(&self, device_free_bytes: usize) -> VramStats {
        VramStats {
            soft_cap_bytes: self.soft_cap_bytes,
            allocated_bytes: self.allocated_bytes.load(Ordering::Acquire),
            serving_allocated_bytes: self.allocated_bytes_for(Category::Serving),
            anneal_allocated_bytes: self.allocated_bytes_for(Category::Anneal),
            device_free_bytes,
            splits_total: self.admission_splits_total.load(Ordering::Acquire),
            queued_total: 0,
            failed_total: self.admission_failed_total.load(Ordering::Acquire),
            oom_guard: crate::vram::OomGuardStats {
                oom_intercepts: self.oom_intercepts_total.load(Ordering::Acquire),
                batch_reductions: self.oom_batch_reductions_total.load(Ordering::Acquire),
                final_failures: self.oom_final_failures_total.load(Ordering::Acquire),
            },
            yield_stats: crate::vram::YieldStats {
                anneal_throttle_events: self.anneal_throttle_events_total.load(Ordering::Acquire),
                anneal_vram_rejections: self.anneal_vram_rejections_total.load(Ordering::Acquire),
            },
        }
    }

    pub(crate) fn record_admission_split(&self) {
        self.admission_splits_total.fetch_add(1, Ordering::AcqRel);
    }

    pub(crate) fn record_admission_failed(&self) {
        self.admission_failed_total.fetch_add(1, Ordering::AcqRel);
    }

    pub(crate) fn record_oom_intercept(&self) {
        self.oom_intercepts_total.fetch_add(1, Ordering::AcqRel);
    }

    pub(crate) fn record_oom_batch_reduction(&self) {
        self.oom_batch_reductions_total
            .fetch_add(1, Ordering::AcqRel);
    }

    pub(crate) fn record_oom_final_failure(&self) {
        self.oom_final_failures_total.fetch_add(1, Ordering::AcqRel);
    }

    pub(crate) fn record_anneal_throttle_event(&self) {
        self.anneal_throttle_events_total
            .fetch_add(1, Ordering::AcqRel);
    }

    pub(crate) fn record_anneal_vram_rejection(&self) {
        self.anneal_vram_rejections_total
            .fetch_add(1, Ordering::AcqRel);
    }

    fn reserve_total(&self, bytes: usize) -> Result<()> {
        let mut current = self.allocated_bytes.load(Ordering::Acquire);
        loop {
            let projected = self.checked_projection(current, bytes)?;
            match self.allocated_bytes.compare_exchange_weak(
                current,
                projected,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Ok(()),
                Err(actual) => current = actual,
            }
        }
    }

    fn reserve_category_counter(
        &self,
        category: Category,
        bytes: usize,
        category_cap_bytes: Option<usize>,
    ) -> Result<()> {
        let counter = self.category_counter(category);
        let mut current = counter.load(Ordering::Acquire);
        loop {
            let projected = current.checked_add(bytes).ok_or_else(|| {
                budget_err(format!(
                    "{} reservation arithmetic overflow: allocated={current} + requested={bytes}",
                    category.as_str()
                ))
            })?;
            if let Some(cap) = category_cap_bytes
                && projected > cap
            {
                return Err(budget_err(format!(
                    "{} category cap exceeded: allocated={current} + requested={bytes} = {projected} > cap={cap}",
                    category.as_str()
                )));
            }
            match counter.compare_exchange_weak(
                current,
                projected,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Ok(()),
                Err(actual) => current = actual,
            }
        }
    }

    fn check_soft_cap(&self, current: usize, bytes: usize) -> Result<()> {
        self.checked_projection(current, bytes).map(|_| ())
    }

    fn checked_projection(&self, current: usize, bytes: usize) -> Result<usize> {
        let projected = current.checked_add(bytes).ok_or_else(|| {
            budget_err(format!(
                "reservation arithmetic overflow: allocated={current} + requested={bytes}"
            ))
        })?;
        if projected > self.soft_cap_bytes {
            return Err(budget_err(format!(
                "soft cap exceeded: allocated={current} + requested={bytes} = {projected} > soft_cap={}",
                self.soft_cap_bytes
            )));
        }
        Ok(projected)
    }

    fn check_device_headroom(&self, bytes: usize) -> Result<()> {
        self.check_device_headroom_with_free(bytes, self.device_free_vram()?)
    }

    fn check_device_headroom_with_free(&self, bytes: usize, free: usize) -> Result<()> {
        let usable = free.saturating_sub(RESERVED_HEADROOM_BYTES);
        if bytes > usable {
            return Err(budget_err(format!(
                "insufficient device VRAM: requested={bytes} > usable={usable} (free={free} - headroom={RESERVED_HEADROOM_BYTES})"
            )));
        }
        Ok(())
    }

    fn category_counter(&self, category: Category) -> &AtomicUsize {
        match category {
            Category::Serving => &self.serving_allocated_bytes,
            Category::Anneal => &self.anneal_allocated_bytes,
        }
    }
}

/// RAII handle for a live VRAM reservation. Dropping it returns `bytes` to the
/// budgeter's available pool and the owner category counter.
pub struct VramGuard<'b, P: VramProbe> {
    budgeter: &'b VramBudgeter<P>,
    bytes: usize,
    category: Category,
}

impl<P: VramProbe> VramGuard<'_, P> {
    /// The number of bytes this guard holds reserved.
    pub fn bytes(&self) -> usize {
        self.bytes
    }

    /// The owner category this guard accounts against.
    pub fn category(&self) -> Category {
        self.category
    }
}

impl<P: VramProbe> Drop for VramGuard<'_, P> {
    fn drop(&mut self) {
        if self.bytes > 0 {
            self.budgeter
                .allocated_bytes
                .fetch_sub(self.bytes, Ordering::AcqRel);
            self.budgeter
                .category_counter(self.category)
                .fetch_sub(self.bytes, Ordering::AcqRel);
        }
    }
}

fn budget_err(detail: String) -> ForgeError {
    ForgeError::VramBudget {
        detail,
        remediation: VRAM_BUDGET_REMEDIATION.to_string(),
    }
}

/// Parse the soft cap from a raw env value. Fail-loud on a non-integer; default
/// only on absence. Pure (no env access) so it can be tested with known input.
fn parse_soft_cap_strict(raw: Option<&str>) -> Result<usize> {
    match raw {
        None => Ok(DEFAULT_SOFT_CAP_BYTES),
        Some(s) => s.trim().parse::<usize>().map_err(|_| {
            budget_err(format!(
                "{VRAM_BUDGET_ENV}={s:?} is not a valid byte count (expected a non-negative integer number of bytes)"
            ))
        }),
    }
}
