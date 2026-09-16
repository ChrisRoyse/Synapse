use std::time::Duration;

use synapse_action::{ActionError, ResolvedBackend, TokenBucket};
use synapse_core::error_codes;
use tokio::time;

#[tokio::test(start_paused = true)]
async fn software_5100_events_in_one_window_limits_exactly_100() {
    let bucket = TokenBucket::for_backend(ResolvedBackend::Software);
    align_bucket_clock(&bucket).await;
    let before = bucket.snapshot();

    let (accepted, limited_errors) = submit_events(&bucket, ResolvedBackend::Software, 5_100);
    let after_batch = bucket.snapshot();
    time::advance(Duration::from_secs(1)).await;
    bucket.refill();
    let after_refill = bucket.snapshot();

    assert_eq!(before.tokens, 5_000);
    assert_eq!(accepted, 5_000);
    assert_eq!(limited_errors.len(), 100);
    assert_eq!(after_batch.tokens, 0);
    assert_eq!(after_refill.tokens, 5_000);
    assert_all_rate_limited(&limited_errors, 1);
    println!(
        "readback=token_bucket edge=overshoot before=tokens:{} after=tokens:{} accepted={} limited={} first_code={} first_retry_after_ms={:?} after_refill=tokens:{}",
        before.tokens,
        after_batch.tokens,
        accepted,
        limited_errors.len(),
        limited_errors[0].code(),
        limited_errors[0].retry_after_ms(),
        after_refill.tokens
    );
}

#[tokio::test(start_paused = true)]
async fn software_exact_capacity_has_no_rate_limited_errors() {
    let bucket = TokenBucket::for_backend(ResolvedBackend::Software);
    align_bucket_clock(&bucket).await;
    let before = bucket.snapshot();

    let (accepted, limited_errors) = submit_events(&bucket, ResolvedBackend::Software, 5_000);
    let after = bucket.snapshot();

    assert_eq!(before.tokens, 5_000);
    assert_eq!(accepted, 5_000);
    assert!(limited_errors.is_empty());
    assert_eq!(after.tokens, 0);
    println!(
        "readback=token_bucket edge=exact_capacity before=tokens:{} after=tokens:{} accepted={} limited={}",
        before.tokens,
        after.tokens,
        accepted,
        limited_errors.len()
    );
}

#[tokio::test(start_paused = true)]
async fn empty_software_bucket_refills_after_one_second() {
    let bucket = TokenBucket::for_backend(ResolvedBackend::Software);
    align_bucket_clock(&bucket).await;
    let before = bucket.snapshot();

    let (accepted, limited_errors) = submit_events(&bucket, ResolvedBackend::Software, 5_000);
    let after_empty = bucket.snapshot();
    time::advance(Duration::from_secs(1)).await;
    bucket.refill();
    let after_refill = bucket.snapshot();

    assert_eq!(accepted, 5_000);
    assert!(limited_errors.is_empty());
    assert_eq!(after_empty.tokens, 0);
    assert_eq!(after_refill.tokens, 5_000);
    println!(
        "readback=token_bucket edge=one_second_refill before={before:?} after_empty={after_empty:?} after_refill={after_refill:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn vigem_1100_events_limits_exactly_100() {
    let bucket = TokenBucket::for_backend(ResolvedBackend::Vigem);
    align_bucket_clock(&bucket).await;
    let before = bucket.snapshot();

    let (accepted, limited_errors) = submit_events(&bucket, ResolvedBackend::Vigem, 1_100);
    let after = bucket.snapshot();

    assert_eq!(before.tokens, 1_000);
    assert_eq!(accepted, 1_000);
    assert_eq!(limited_errors.len(), 100);
    assert_eq!(after.tokens, 0);
    assert_all_rate_limited(&limited_errors, 1);
    println!(
        "readback=token_bucket edge=vigem_overshoot before=tokens:{} after=tokens:{} accepted={} limited={} first_code={} first_retry_after_ms={:?}",
        before.tokens,
        after.tokens,
        accepted,
        limited_errors.len(),
        limited_errors[0].code(),
        limited_errors[0].retry_after_ms()
    );
}

fn submit_events(
    bucket: &TokenBucket,
    backend: ResolvedBackend,
    count: usize,
) -> (usize, Vec<ActionError>) {
    let mut accepted = 0;
    let mut limited_errors = Vec::new();
    for _event_index in 0..count {
        if bucket.try_consume(1) {
            accepted += 1;
        } else {
            limited_errors.push(rate_limited_error(bucket, backend));
        }
    }
    (accepted, limited_errors)
}

async fn align_bucket_clock(bucket: &TokenBucket) {
    time::advance(Duration::from_millis(1)).await;
    bucket.refill();
}

fn rate_limited_error(bucket: &TokenBucket, backend: ResolvedBackend) -> ActionError {
    let snapshot = bucket.snapshot();
    let retry_after_ms = bucket.retry_after_ms(1);
    ActionError::RateLimited {
        detail: format!(
            "backend={} retry_after_ms={} requested_tokens=1 available_tokens={} refill_rate_per_s={}",
            backend.as_str(),
            retry_after_ms,
            snapshot.tokens,
            snapshot.refill_rate_per_s
        ),
        retry_after_ms,
    }
}

fn assert_all_rate_limited(errors: &[ActionError], expected_retry_after_ms: u64) {
    for error in errors {
        assert_eq!(error.code(), error_codes::ACTION_RATE_LIMITED);
        assert_eq!(error.retry_after_ms(), Some(expected_retry_after_ms));
        assert!(error.detail().contains("retry_after_ms="));
    }
}
