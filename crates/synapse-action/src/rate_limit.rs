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

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::time;

    use super::{
        SOFTWARE_RATE_LIMIT_PER_S, TokenBucket, VIGEM_RATE_LIMIT_PER_S, rate_limit_per_s,
        retry_after_ms_for_snapshot,
    };
    use crate::ResolvedBackend;

    #[test]
    fn software_bucket_consumes_100_and_refills_100_after_20ms() {
        let bucket =
            TokenBucket::new_at_ns(SOFTWARE_RATE_LIMIT_PER_S, SOFTWARE_RATE_LIMIT_PER_S, 0);
        let before = bucket.snapshot();

        let consumed = (0..100).filter(|_| bucket.try_consume_at_ns(1, 0)).count();
        let after_consume = bucket.snapshot();
        bucket.refill_at_ns(20_000_000);
        let after_refill = bucket.snapshot();

        assert_eq!(consumed, 100);
        assert_eq!(after_consume.tokens, SOFTWARE_RATE_LIMIT_PER_S - 100);
        assert_eq!(after_refill.tokens, SOFTWARE_RATE_LIMIT_PER_S);
        println!(
            "readback=token_bucket edge=software_100_refill before={before:?} after_consume={after_consume:?} after_refill={after_refill:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn software_bucket_refills_with_paused_tokio_time() {
        let bucket = TokenBucket::new(SOFTWARE_RATE_LIMIT_PER_S, SOFTWARE_RATE_LIMIT_PER_S);
        let before = bucket.snapshot();

        assert!(bucket.try_consume(100));
        let after_consume = bucket.snapshot();
        time::advance(Duration::from_millis(20)).await;
        bucket.refill();
        let after_refill = bucket.snapshot();

        assert_eq!(after_consume.tokens, SOFTWARE_RATE_LIMIT_PER_S - 100);
        assert_eq!(after_refill.tokens, SOFTWARE_RATE_LIMIT_PER_S);
        println!(
            "readback=token_bucket edge=tokio_paused_20ms before={before:?} after_consume={after_consume:?} after_refill={after_refill:?}"
        );
    }

    #[test]
    fn empty_bucket_denies_without_mutating_negative() {
        let bucket = TokenBucket::new_at_ns(3, 3, 0);
        let before = bucket.snapshot();

        assert!(bucket.try_consume_at_ns(3, 0));
        let after_empty = bucket.snapshot();
        assert!(!bucket.try_consume_at_ns(1, 0));
        let after_denied = bucket.snapshot();

        assert_eq!(after_empty.tokens, 0);
        assert_eq!(after_denied.tokens, 0);
        println!(
            "readback=token_bucket edge=empty_denied before={before:?} after_empty={after_empty:?} after_denied={after_denied:?}"
        );
    }

    #[test]
    fn refill_clamps_to_capacity() {
        let bucket = TokenBucket::new_at_ns(10, 10, 0);
        let before = bucket.snapshot();

        assert!(bucket.try_consume_at_ns(5, 0));
        let after_consume = bucket.snapshot();
        bucket.refill_at_ns(10_000_000_000);
        let after_refill = bucket.snapshot();

        assert_eq!(after_consume.tokens, 5);
        assert_eq!(after_refill.tokens, 10);
        println!(
            "readback=token_bucket edge=clamp before={before:?} after_consume={after_consume:?} after_refill={after_refill:?}"
        );
    }

    #[test]
    fn sub_token_elapsed_time_is_not_lost() {
        let bucket = TokenBucket::new_at_ns(10, SOFTWARE_RATE_LIMIT_PER_S, 0);
        assert!(bucket.try_consume_at_ns(10, 0));
        let before = bucket.snapshot();

        bucket.refill_at_ns(100_000);
        let after_half_token = bucket.snapshot();
        bucket.refill_at_ns(200_000);
        let after_full_token = bucket.snapshot();

        assert_eq!(after_half_token.tokens, 0);
        assert_eq!(after_half_token.last_refill_ns, 0);
        assert_eq!(after_full_token.tokens, 1);
        println!(
            "readback=token_bucket edge=sub_token_elapsed before={before:?} after_half={after_half_token:?} after_full={after_full_token:?}"
        );
    }

    #[test]
    fn fractional_refill_remainder_is_not_lost_after_whole_token() {
        let bucket = TokenBucket::new_at_ns(10, SOFTWARE_RATE_LIMIT_PER_S, 0);
        assert!(bucket.try_consume_at_ns(10, 0));
        let before = bucket.snapshot();

        bucket.refill_at_ns(300_000);
        let after_one_and_half_tokens = bucket.snapshot();
        bucket.refill_at_ns(400_000);
        let after_two_tokens = bucket.snapshot();

        assert_eq!(after_one_and_half_tokens.tokens, 1);
        assert_eq!(after_one_and_half_tokens.last_refill_ns, 200_000);
        assert_eq!(after_two_tokens.tokens, 2);
        assert_eq!(after_two_tokens.last_refill_ns, 400_000);
        println!(
            "readback=token_bucket edge=fractional_remainder before={before:?} after_one_and_half={after_one_and_half_tokens:?} after_two={after_two_tokens:?}"
        );
    }

    #[test]
    fn zero_refill_rate_never_refills_or_divides_by_zero() {
        let bucket = TokenBucket::new_at_ns(2, 0, 0);
        let before = bucket.snapshot();

        assert!(bucket.try_consume_at_ns(2, 0));
        let after_empty = bucket.snapshot();
        bucket.refill_at_ns(10_000_000_000);
        let after_refill_attempt = bucket.snapshot();

        assert_eq!(after_empty.tokens, 0);
        assert_eq!(after_refill_attempt.tokens, 0);
        assert_eq!(after_refill_attempt.last_refill_ns, 0);
        println!(
            "readback=token_bucket edge=zero_rate before={before:?} after_empty={after_empty:?} after_refill_attempt={after_refill_attempt:?}"
        );
    }

    #[test]
    fn backend_defaults_match_m2_spec() {
        for (backend, expected) in [
            (ResolvedBackend::Software, SOFTWARE_RATE_LIMIT_PER_S),
            (ResolvedBackend::Vigem, VIGEM_RATE_LIMIT_PER_S),
            (ResolvedBackend::Hardware, SOFTWARE_RATE_LIMIT_PER_S),
        ] {
            let bucket = TokenBucket::for_backend(backend);
            let after = bucket.snapshot();
            assert_eq!(rate_limit_per_s(backend), expected);
            assert_eq!(after.capacity, expected);
            assert_eq!(after.tokens, expected);
            assert_eq!(after.refill_rate_per_s, expected);
            println!(
                "readback=token_bucket edge=backend_default backend={} after={after:?}",
                backend.as_str()
            );
        }
    }

    #[test]
    fn retry_after_hint_rounds_up_to_one_millisecond() {
        let bucket =
            TokenBucket::new_at_ns(SOFTWARE_RATE_LIMIT_PER_S, SOFTWARE_RATE_LIMIT_PER_S, 0);
        assert!(bucket.try_consume_at_ns(SOFTWARE_RATE_LIMIT_PER_S, 0));
        let before = bucket.snapshot();

        let after = retry_after_ms_for_snapshot(before, 1);

        assert_eq!(after, 1);
        println!(
            "readback=token_bucket edge=retry_after_hint before={before:?} after_retry_after_ms={after}"
        );
    }
}
