use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};

use calyx_core::{CalyxError, Clock, Result, Ts};
use serde::{Deserialize, Serialize};

pub const CALYX_ANNEAL_BUDGET_EXHAUSTED: &str = "CALYX_ANNEAL_BUDGET_EXHAUSTED";
pub const CALYX_ANNEAL_BUDGET_INVALID_CONFIG: &str = "CALYX_ANNEAL_BUDGET_INVALID_CONFIG";
pub const CALYX_ANNEAL_BUDGET_NVML_UNAVAILABLE: &str = "CALYX_ANNEAL_BUDGET_NVML_UNAVAILABLE";
pub const CALYX_ANNEAL_BUDGET_CPU_UNAVAILABLE: &str = "CALYX_ANNEAL_BUDGET_CPU_UNAVAILABLE";
pub const BACKGROUND_NICE: i32 = 10;

const CONFIG_DIR: &str = ".anneal";
const CONFIG_FILE: &str = "budget.toml";
const DEFAULT_CPU_FRACTION: f64 = 0.15;
const DEFAULT_VRAM_BYTES: u64 = 512 * 1024 * 1024;
const DEFAULT_TICK_INTERVAL_MS: u64 = 100;
const EPSILON: f64 = 1e-12;

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct BudgetConfig {
    pub cpu_fraction: f64,
    pub vram_bytes: u64,
    pub tick_interval_ms: u64,
}

impl Default for BudgetConfig {
    fn default() -> Self {
        Self {
            cpu_fraction: DEFAULT_CPU_FRACTION,
            vram_bytes: DEFAULT_VRAM_BYTES,
            tick_interval_ms: DEFAULT_TICK_INTERVAL_MS,
        }
    }
}

impl BudgetConfig {
    pub fn load_from_vault(vault: impl AsRef<Path>) -> Result<Self> {
        let path = budget_config_path(vault.as_ref());
        if path.exists() {
            return read_budget_config(&path);
        }
        let config = Self::default();
        persist_budget_config(&path, config)?;
        Ok(config)
    }

    pub fn validate(self) -> Result<Self> {
        if !self.cpu_fraction.is_finite() || !(0.0..=1.0).contains(&self.cpu_fraction) {
            return Err(invalid_config("cpu_fraction must be finite in 0.0..=1.0"));
        }
        if self.tick_interval_ms == 0 {
            return Err(invalid_config("tick_interval_ms must be positive"));
        }
        Ok(self)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BudgetStatus {
    pub cpu_used_fraction: f64,
    pub vram_used_bytes: u64,
    pub handles_active: usize,
    pub last_tick_at: Ts,
    pub low_priority_nice: i32,
    pub warning_code: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BudgetConfigReadback {
    pub config_path: PathBuf,
    pub config: BudgetConfig,
}

pub trait BudgetProbe: Send + Sync {
    fn sample(&self) -> BudgetProbeSample;
}

#[derive(Clone, Debug, PartialEq)]
pub struct BudgetProbeSample {
    pub cpu_used_fraction: f64,
    pub vram_used_bytes: u64,
    pub nvml_available: bool,
    pub warning_code: Option<String>,
}

pub struct BudgetEnforcer<'a, P = ProcStatBudgetProbe>
where
    P: BudgetProbe,
{
    config: BudgetConfig,
    clock: &'a dyn Clock,
    probe: P,
    state: Arc<Mutex<BudgetState>>,
}

#[derive(Clone, Debug)]
struct BudgetState {
    sampled_cpu_fraction: f64,
    sampled_vram_bytes: u64,
    reserved_cpu_weight: f64,
    reserved_vram_bytes: u64,
    handles_active: usize,
    last_tick_at: Ts,
    warning_code: Option<String>,
}

pub struct BudgetHandle {
    remaining_ticks: usize,
    max_ticks: usize,
    release: Option<BudgetRelease>,
}

struct BudgetRelease {
    state: Weak<Mutex<BudgetState>>,
    cpu_weight: f64,
    vram_bytes: u64,
}

impl BudgetHandle {
    /// Creates an unreserved cooperative tick handle for tests and shadow replay.
    ///
    /// Production background work should use `BudgetEnforcer::acquire`, which
    /// returns a handle wired to the RAII release path.
    pub const fn new(ticks: usize) -> Self {
        Self {
            remaining_ticks: ticks,
            max_ticks: ticks,
            release: None,
        }
    }

    pub const fn remaining_ticks(&self) -> usize {
        self.remaining_ticks
    }

    pub(crate) fn try_consume(&mut self) -> bool {
        if self.remaining_ticks == 0 {
            return false;
        }
        self.remaining_ticks -= 1;
        true
    }

    pub(crate) fn replenish(&mut self) {
        self.remaining_ticks = self.max_ticks;
    }
}

impl Drop for BudgetHandle {
    fn drop(&mut self) {
        let Some(release) = self.release.take() else {
            return;
        };
        let Some(state) = release.state.upgrade() else {
            return;
        };
        if let Ok(mut state) = state.lock() {
            state.reserved_cpu_weight = (state.reserved_cpu_weight - release.cpu_weight).max(0.0);
            state.reserved_vram_bytes =
                state.reserved_vram_bytes.saturating_sub(release.vram_bytes);
            state.handles_active = state.handles_active.saturating_sub(1);
        }
    }
}

impl<'a> BudgetEnforcer<'a, ProcStatBudgetProbe> {
    pub fn new(config: BudgetConfig, clock: &'a dyn Clock) -> Result<Self> {
        Self::with_probe(config, clock, ProcStatBudgetProbe::default())
    }
}

impl<'a, P> BudgetEnforcer<'a, P>
where
    P: BudgetProbe,
{
    pub fn with_probe(config: BudgetConfig, clock: &'a dyn Clock, probe: P) -> Result<Self> {
        Ok(Self {
            config: config.validate()?,
            clock,
            probe,
            state: Arc::new(Mutex::new(BudgetState::new(clock.now()))),
        })
    }

    pub const fn config(&self) -> BudgetConfig {
        self.config
    }

    pub fn tick(&self) -> Result<BudgetStatus> {
        let sample = self.probe.sample();
        let mut state = self.lock_state()?;
        state.sampled_cpu_fraction = sample.cpu_used_fraction.clamp(0.0, 1.0);
        state.sampled_vram_bytes = sample.vram_used_bytes;
        state.last_tick_at = self.clock.now();
        state.warning_code = sample.warning_code.or_else(|| {
            (!sample.nvml_available).then(|| CALYX_ANNEAL_BUDGET_NVML_UNAVAILABLE.to_string())
        });
        Ok(state.status())
    }

    pub fn acquire(&self, cpu_weight: f64, vram_bytes: u64) -> Result<BudgetHandle> {
        self.acquire_with_ticks(cpu_weight, vram_bytes, handle_ticks(self.config))
    }

    /// Admits a bounded unit of work against the live resource ceiling and
    /// gives it the exact cooperative work quota declared by its caller.
    pub fn acquire_with_ticks(
        &self,
        cpu_weight: f64,
        vram_bytes: u64,
        ticks: usize,
    ) -> Result<BudgetHandle> {
        validate_request(cpu_weight)?;
        if ticks == 0 {
            return Err(invalid_config("cooperative tick quota must be positive"));
        }
        self.tick()?;
        let mut state = self.lock_state()?;
        if self.config.cpu_fraction <= EPSILON || self.config.vram_bytes == 0 {
            return Err(exhausted("budget is configured with zero capacity"));
        }
        let projected_cpu = state.sampled_cpu_fraction + state.reserved_cpu_weight + cpu_weight;
        let projected_vram = state
            .sampled_vram_bytes
            .saturating_add(state.reserved_vram_bytes)
            .saturating_add(vram_bytes);
        if projected_cpu > self.config.cpu_fraction + EPSILON {
            return Err(exhausted("cpu budget exhausted"));
        }
        if projected_vram > self.config.vram_bytes {
            return Err(exhausted("vram budget exhausted"));
        }
        state.reserved_cpu_weight += cpu_weight;
        state.reserved_vram_bytes = state.reserved_vram_bytes.saturating_add(vram_bytes);
        state.handles_active += 1;
        Ok(BudgetHandle {
            remaining_ticks: ticks,
            max_ticks: ticks,
            release: Some(BudgetRelease {
                state: Arc::downgrade(&self.state),
                cpu_weight,
                vram_bytes,
            }),
        })
    }

    pub fn status(&self) -> Result<BudgetStatus> {
        Ok(self.lock_state()?.status())
    }

    fn lock_state(&self) -> Result<std::sync::MutexGuard<'_, BudgetState>> {
        self.state
            .lock()
            .map_err(|_| CalyxError::backpressure("budget state lock poisoned"))
    }
}

impl BudgetState {
    const fn new(ts: Ts) -> Self {
        Self {
            sampled_cpu_fraction: 0.0,
            sampled_vram_bytes: 0,
            reserved_cpu_weight: 0.0,
            reserved_vram_bytes: 0,
            handles_active: 0,
            last_tick_at: ts,
            warning_code: None,
        }
    }

    fn status(&self) -> BudgetStatus {
        BudgetStatus {
            cpu_used_fraction: self.sampled_cpu_fraction + self.reserved_cpu_weight,
            vram_used_bytes: self
                .sampled_vram_bytes
                .saturating_add(self.reserved_vram_bytes),
            handles_active: self.handles_active,
            last_tick_at: self.last_tick_at,
            low_priority_nice: BACKGROUND_NICE,
            warning_code: self.warning_code.clone(),
        }
    }
}

#[derive(Default)]
pub struct ProcStatBudgetProbe {
    previous: Mutex<Option<ProcStat>>,
}

impl BudgetProbe for ProcStatBudgetProbe {
    fn sample(&self) -> BudgetProbeSample {
        let Some(current) = read_proc_stat() else {
            return cpu_unavailable_sample();
        };
        let Ok(mut previous) = self.previous.lock() else {
            return cpu_unavailable_sample();
        };
        let cpu_used_fraction = previous
            .as_ref()
            .and_then(|prev| current.usage_since(*prev))
            .unwrap_or(0.0);
        *previous = Some(current);
        BudgetProbeSample {
            cpu_used_fraction,
            vram_used_bytes: 0,
            nvml_available: false,
            warning_code: None,
        }
    }
}

fn cpu_unavailable_sample() -> BudgetProbeSample {
    BudgetProbeSample {
        cpu_used_fraction: 1.0,
        vram_used_bytes: 0,
        nvml_available: false,
        warning_code: Some(CALYX_ANNEAL_BUDGET_CPU_UNAVAILABLE.to_string()),
    }
}

#[derive(Clone, Copy)]
struct ProcStat {
    idle: u64,
    total: u64,
}

impl ProcStat {
    fn usage_since(self, previous: Self) -> Option<f64> {
        let idle = self.idle.checked_sub(previous.idle)?;
        let total = self.total.checked_sub(previous.total)?;
        (total > 0).then_some(1.0 - idle as f64 / total as f64)
    }
}

pub fn budget_config_path(vault: &Path) -> PathBuf {
    vault.join(CONFIG_DIR).join(CONFIG_FILE)
}

pub fn read_budget_config_from_vault(vault: impl AsRef<Path>) -> Result<BudgetConfigReadback> {
    let config_path = budget_config_path(vault.as_ref());
    let config = read_budget_config(&config_path)?;
    Ok(BudgetConfigReadback {
        config_path,
        config,
    })
}

fn read_budget_config(path: &Path) -> Result<BudgetConfig> {
    let bytes = fs::read(path)
        .map_err(|error| invalid_config(format!("read {}: {error}", path.display())))?;
    let text = std::str::from_utf8(&bytes)
        .map_err(|error| invalid_config(format!("{} is not UTF-8: {error}", path.display())))?;
    toml::from_str::<BudgetConfig>(text)
        .map_err(|error| invalid_config(format!("parse {}: {error}", path.display())))?
        .validate()
}

fn persist_budget_config(path: &Path, config: BudgetConfig) -> Result<()> {
    let config = config.validate()?;
    let text = toml::to_string_pretty(&config)
        .map_err(|error| invalid_config(format!("serialize budget config: {error}")))?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| invalid_config(format!("create {}: {error}", parent.display())))?;
    }
    atomic_write_text(path, &text)
}

fn atomic_write_text(path: &Path, text: &str) -> Result<()> {
    let tmp = temp_path(path)?;
    fs::write(&tmp, text)
        .map_err(|error| invalid_config(format!("write {}: {error}", tmp.display())))?;
    fs::rename(&tmp, path).map_err(|error| {
        let _ = fs::remove_file(&tmp);
        invalid_config(format!(
            "rename {} -> {}: {error}",
            tmp.display(),
            path.display()
        ))
    })
}

fn temp_path(path: &Path) -> Result<PathBuf> {
    let file_name = path
        .file_name()
        .ok_or_else(|| invalid_config("budget config path must include a file name"))?;
    let mut tmp_name = file_name.to_os_string();
    tmp_name.push(format!(".tmp-{}", std::process::id()));
    Ok(path.with_file_name(tmp_name))
}

#[cfg(target_os = "linux")]
fn read_proc_stat() -> Option<ProcStat> {
    let text = fs::read_to_string("/proc/stat").ok()?;
    let fields = text.lines().next()?.split_whitespace().collect::<Vec<_>>();
    if fields.first().copied()? != "cpu" {
        return None;
    }
    let mut values = fields[1..]
        .iter()
        .filter_map(|field| field.parse::<u64>().ok());
    let user = values.next()?;
    let nice = values.next()?;
    let system = values.next()?;
    let idle = values.next()?;
    let iowait = values.next().unwrap_or(0);
    let irq = values.next().unwrap_or(0);
    let softirq = values.next().unwrap_or(0);
    let steal = values.next().unwrap_or(0);
    let idle_all = idle.saturating_add(iowait);
    let total = user
        .saturating_add(nice)
        .saturating_add(system)
        .saturating_add(idle)
        .saturating_add(iowait)
        .saturating_add(irq)
        .saturating_add(softirq)
        .saturating_add(steal);
    Some(ProcStat {
        idle: idle_all,
        total,
    })
}

#[cfg(target_os = "windows")]
fn read_proc_stat() -> Option<ProcStat> {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::Threading::GetSystemTimes;

    let mut idle = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    // SAFETY: all three pointers reference initialized, writable FILETIME
    // values for the duration of the synchronous Windows API call.
    let succeeded = unsafe { GetSystemTimes(&mut idle, &mut kernel, &mut user) };
    if succeeded == 0 {
        return None;
    }
    let idle = filetime_ticks(idle);
    let kernel = filetime_ticks(kernel);
    let user = filetime_ticks(user);
    Some(ProcStat {
        idle,
        // GetSystemTimes defines kernel time as including idle time.
        total: kernel.checked_add(user)?,
    })
}

#[cfg(target_os = "windows")]
fn filetime_ticks(value: windows_sys::Win32::Foundation::FILETIME) -> u64 {
    (u64::from(value.dwHighDateTime) << 32) | u64::from(value.dwLowDateTime)
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
fn read_proc_stat() -> Option<ProcStat> {
    None
}

fn handle_ticks(config: BudgetConfig) -> usize {
    let ticks = 1_000_u64.div_ceil(config.tick_interval_ms);
    usize::try_from(ticks).unwrap_or(usize::MAX).max(1)
}

fn validate_request(cpu_weight: f64) -> Result<()> {
    if !cpu_weight.is_finite() || cpu_weight < 0.0 {
        return Err(invalid_config("cpu_weight must be finite and non-negative"));
    }
    Ok(())
}

fn exhausted(message: impl Into<String>) -> CalyxError {
    CalyxError {
        code: CALYX_ANNEAL_BUDGET_EXHAUSTED,
        message: message.into(),
        remediation: "retry the Anneal task after the background budget replenishes",
    }
}

fn invalid_config(message: impl Into<String>) -> CalyxError {
    CalyxError {
        code: CALYX_ANNEAL_BUDGET_INVALID_CONFIG,
        message: message.into(),
        remediation: "fix vault .anneal/budget.toml before running Anneal background work",
    }
}
