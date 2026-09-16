use std::time::{SystemTime, UNIX_EPOCH};

use rocksdb::{CompactionDecision, Options};
use synapse_core::retention::{DEFAULTS, RetentionTtl};

const NANOS_PER_SECOND: u64 = 1_000_000_000;
const SECONDS_PER_HOUR: u64 = 60 * 60;
const HOURS_PER_DAY: u64 = 24;
const TS_NS_FIELD: &[u8] = br#""ts_ns""#;

pub(crate) fn install_ttl_filter(options: &mut Options, cf_name: &'static str) {
    let Some(ttl_ns) = ttl_ns_for_cf(cf_name) else {
        return;
    };
    let filter_name = format!("synapse_ttl_{cf_name}");
    options.set_compaction_filter(&filter_name, move |_level, _key, value| {
        ttl_decision(ttl_ns, current_time_ns(), value)
    });
}

pub(crate) fn ttl_ns_for_cf(cf_name: &str) -> Option<u64> {
    DEFAULTS
        .iter()
        .find(|default| default.cf == cf_name)
        .and_then(|default| ttl_to_ns(default.ttl))
}

pub(crate) fn ttl_decision(ttl_ns: u64, now_ns: u64, value: &[u8]) -> CompactionDecision {
    match extract_ts_ns(value) {
        Some(ts_ns) if now_ns.saturating_sub(ts_ns) > ttl_ns => CompactionDecision::Remove,
        _ => CompactionDecision::Keep,
    }
}

fn ttl_to_ns(ttl: RetentionTtl) -> Option<u64> {
    match ttl {
        RetentionTtl::None | RetentionTtl::LruOnly => None,
        RetentionTtl::Hours(hours) => hours
            .checked_mul(SECONDS_PER_HOUR)?
            .checked_mul(NANOS_PER_SECOND),
        RetentionTtl::Days(days) => days
            .checked_mul(HOURS_PER_DAY)?
            .checked_mul(SECONDS_PER_HOUR)?
            .checked_mul(NANOS_PER_SECOND),
    }
}

fn extract_ts_ns(value: &[u8]) -> Option<u64> {
    let field_start = value
        .windows(TS_NS_FIELD.len())
        .position(|window| window == TS_NS_FIELD)?;
    let mut index = field_start + TS_NS_FIELD.len();
    index = skip_json_ws(value, index);
    if value.get(index) != Some(&b':') {
        return None;
    }
    index = skip_json_ws(value, index + 1);
    let digits_start = index;
    while value.get(index).is_some_and(u8::is_ascii_digit) {
        index += 1;
    }
    if digits_start == index {
        return None;
    }
    std::str::from_utf8(&value[digits_start..index])
        .ok()?
        .parse()
        .ok()
}

fn skip_json_ws(value: &[u8], mut index: usize) -> usize {
    while value
        .get(index)
        .is_some_and(|byte| matches!(byte, b' ' | b'\n' | b'\r' | b'\t'))
    {
        index += 1;
    }
    index
}

fn current_time_ns() -> u64 {
    #[cfg(test)]
    {
        let fixed = TEST_NOW_NS.load(std::sync::atomic::Ordering::SeqCst);
        if fixed != 0 {
            return fixed;
        }
    }

    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
        })
}

#[cfg(test)]
static TEST_NOW_NS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

// The fixed test clock is process-global because RocksDB runs the TTL compaction
// filter on background threads that read `current_time_ns()`. Without serialization,
// parallel `cargo test` threads race on it (one guard's drop resets the clock to 0
// mid-compaction in another test), producing flaky TTL eviction. This lock is held
// for the lifetime of each guard so only one test drives the clock at a time.
#[cfg(test)]
static TEST_CLOCK_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
pub(crate) struct TestClockGuard {
    previous: u64,
    _lock: std::sync::MutexGuard<'static, ()>,
}

#[cfg(test)]
impl Drop for TestClockGuard {
    fn drop(&mut self) {
        TEST_NOW_NS.store(self.previous, std::sync::atomic::Ordering::SeqCst);
    }
}

#[cfg(test)]
pub(crate) fn set_test_now_ns(now_ns: u64) -> TestClockGuard {
    let lock = match TEST_CLOCK_LOCK.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    let previous = TEST_NOW_NS.swap(now_ns, std::sync::atomic::Ordering::SeqCst);
    TestClockGuard {
        previous,
        _lock: lock,
    }
}
