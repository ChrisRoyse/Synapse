use std::sync::{
    OnceLock,
    atomic::{AtomicU32, AtomicU64, Ordering},
};

use tokio::time::Instant;

use crate::ResolvedBackend;

pub const SOFTWARE_RATE_LIMIT_PER_S: u32 = 5_000;
pub const VIGEM_RATE_LIMIT_PER_S: u32 = 1_000;

const NANOS_PER_SECOND: u128 = 1_000_000_000;
const NANOS_PER_MILLISECOND: u128 = 1_000_000;

static PROCESS_START: OnceLock<Instant> = OnceLock::new();

pub struct TokenBucket {
    capacity: u32,
    tokens: AtomicU32,
    refill_rate_per_s: u32,
    last_refill: AtomicU64,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct TokenBucketSnapshot {
    pub capacity: u32,
    pub tokens: u32,
    pub refill_rate_per_s: u32,
    pub last_refill_ns: u64,
}

impl TokenBucket {
    #[must_use]
    pub fn new(capacity: u32, refill_rate_per_s: u32) -> Self {
        Self::new_at_ns(capacity, refill_rate_per_s, monotonic_now_ns())
    }

    #[must_use]
    pub fn for_backend(backend: ResolvedBackend) -> Self {
        let rate = rate_limit_per_s(backend);
        Self::new(rate, rate)
    }

    #[must_use]
    pub fn snapshot(&self) -> TokenBucketSnapshot {
        TokenBucketSnapshot {
            capacity: self.capacity,
            tokens: self.tokens.load(Ordering::Relaxed),
            refill_rate_per_s: self.refill_rate_per_s,
            last_refill_ns: self.last_refill.load(Ordering::Relaxed),
        }
    }

    #[must_use]
    pub fn try_consume(&self, requested: u32) -> bool {
        self.try_consume_at_ns(requested, monotonic_now_ns())
    }

    pub fn refill(&self) {
        self.refill_at_ns(monotonic_now_ns());
    }

    #[must_use]
    pub fn retry_after_ms(&self, requested: u32) -> u64 {
        self.refill();
        let snapshot = self.snapshot();
        retry_after_ms_for_snapshot(snapshot, requested)
    }

    #[must_use]
    const fn new_at_ns(capacity: u32, refill_rate_per_s: u32, now_ns: u64) -> Self {
        Self {
            capacity,
            tokens: AtomicU32::new(capacity),
            refill_rate_per_s,
            last_refill: AtomicU64::new(now_ns),
        }
    }

    #[must_use]
    fn try_consume_at_ns(&self, requested: u32, now_ns: u64) -> bool {
        self.refill_at_ns(now_ns);
        if requested == 0 {
            return true;
        }

        let mut current = self.tokens.load(Ordering::Acquire);
        loop {
            if current < requested {
                return false;
            }
            let next = current - requested;
            match self.tokens.compare_exchange_weak(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_previous) => return true,
                Err(observed) => current = observed,
            }
        }
    }

    fn refill_at_ns(&self, now_ns: u64) {
        if self.refill_rate_per_s == 0 {
            return;
        }

        let mut last = self.last_refill.load(Ordering::Acquire);
        loop {
            if now_ns <= last {
                return;
            }

            let elapsed_ns = now_ns - last;
            let add =
                (u128::from(elapsed_ns) * u128::from(self.refill_rate_per_s)) / NANOS_PER_SECOND;
            if add == 0 {
                return;
            }
            let elapsed_refilled_ns = (add * NANOS_PER_SECOND) / u128::from(self.refill_rate_per_s);
            let next_refill =
                last.saturating_add(u64::try_from(elapsed_refilled_ns).unwrap_or(u64::MAX));

            match self.last_refill.compare_exchange_weak(
                last,
                next_refill,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_previous) => {
                    let add = u32::try_from(add).unwrap_or(u32::MAX);
                    let _update =
                        self.tokens
                            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |tokens| {
                                Some(tokens.saturating_add(add).min(self.capacity))
                            });
                    return;
                }
                Err(observed) => last = observed,
            }
        }
    }
}

#[must_use]
pub fn retry_after_ms_for_snapshot(snapshot: TokenBucketSnapshot, requested: u32) -> u64 {
    if requested == 0 || snapshot.tokens >= requested {
        return 0;
    }
    if snapshot.refill_rate_per_s == 0 {
        return u64::MAX;
    }

    let missing = u128::from(requested - snapshot.tokens);
    let delay_nanos = (missing * NANOS_PER_SECOND).div_ceil(u128::from(snapshot.refill_rate_per_s));
    let retry_millis = delay_nanos.div_ceil(NANOS_PER_MILLISECOND);
    u64::try_from(retry_millis.max(1)).unwrap_or(u64::MAX)
}

#[must_use]
pub const fn rate_limit_per_s(backend: ResolvedBackend) -> u32 {
    match backend {
        ResolvedBackend::Software | ResolvedBackend::Hardware => SOFTWARE_RATE_LIMIT_PER_S,
        ResolvedBackend::Vigem => VIGEM_RATE_LIMIT_PER_S,
    }
}

fn monotonic_now_ns() -> u64 {
    let start = PROCESS_START.get_or_init(Instant::now);
    let elapsed = Instant::now().saturating_duration_since(*start);
    u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX)
}
