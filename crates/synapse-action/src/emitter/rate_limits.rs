use std::sync::{Arc, RwLock};

use crate::{
    ActionError, ActionResult, ResolvedBackend, TokenBucket, TokenBucketSnapshot,
    rate_limit::retry_after_ms_for_snapshot,
};

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct BackendRateLimitSnapshot {
    pub software: TokenBucketSnapshot,
    pub vigem: TokenBucketSnapshot,
    pub hardware: TokenBucketSnapshot,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct BackendRateLimitOverrideReadback {
    pub backend: ResolvedBackend,
    pub before: TokenBucketSnapshot,
    pub after: TokenBucketSnapshot,
}

#[derive(Clone)]
pub struct BackendRateLimitControl {
    inner: Arc<RwLock<BackendRateLimits>>,
}

pub(super) struct BackendRateLimits {
    software: TokenBucket,
    vigem: TokenBucket,
    hardware: TokenBucket,
}

impl BackendRateLimitControl {
    pub(super) fn new(rate_limits: BackendRateLimits) -> Self {
        Self {
            inner: Arc::new(RwLock::new(rate_limits)),
        }
    }

    #[cfg(test)]
    #[must_use]
    /// # Panics
    ///
    /// Panics if the backend rate-limit state lock is poisoned in a test.
    pub fn snapshot(&self) -> BackendRateLimitSnapshot {
        match self.try_snapshot() {
            Ok(snapshot) => snapshot,
            Err(error) => panic!("backend rate limit snapshot failed: {error}"),
        }
    }

    /// # Errors
    ///
    /// Returns [`ActionError::BackendUnavailable`] if the backend rate-limit
    /// state lock is poisoned.
    pub fn try_snapshot(&self) -> ActionResult<BackendRateLimitSnapshot> {
        self.inner
            .read()
            .map(|rate_limits| rate_limits.snapshot())
            .map_err(|_err| ActionError::BackendUnavailable {
                detail: "backend rate limit lock poisoned".to_owned(),
            })
    }

    /// # Errors
    ///
    /// Returns [`ActionError::BackendUnavailable`] if the backend rate-limit
    /// state lock is poisoned.
    pub fn override_backend(
        &self,
        backend: ResolvedBackend,
        capacity: u32,
        refill_rate_per_s: u32,
    ) -> ActionResult<BackendRateLimitOverrideReadback> {
        let mut rate_limits =
            self.inner
                .write()
                .map_err(|_err| ActionError::BackendUnavailable {
                    detail: "backend rate limit lock poisoned".to_owned(),
                })?;
        Ok(rate_limits.replace_backend(backend, TokenBucket::new(capacity, refill_rate_per_s)))
    }

    /// # Errors
    ///
    /// Returns [`ActionError::BackendUnavailable`] if the backend rate-limit
    /// state lock is poisoned.
    pub fn reset_backend(
        &self,
        backend: ResolvedBackend,
    ) -> ActionResult<BackendRateLimitOverrideReadback> {
        let mut rate_limits =
            self.inner
                .write()
                .map_err(|_err| ActionError::BackendUnavailable {
                    detail: "backend rate limit lock poisoned".to_owned(),
                })?;
        Ok(rate_limits.replace_backend(backend, TokenBucket::for_backend(backend)))
    }

    pub(super) fn consume(&self, backend: ResolvedBackend) -> ActionResult<()> {
        let Some(snapshot) = (match self
            .inner
            .read()
            .map_err(|_err| ActionError::BackendUnavailable {
                detail: "backend rate limit lock poisoned".to_owned(),
            })?
            .bucket(backend)
        {
            bucket if bucket.try_consume(1) => None,
            bucket => Some(bucket.snapshot()),
        }) else {
            return Ok(());
        };
        let retry_after_ms = retry_after_ms_for_snapshot(snapshot, 1);
        Err(ActionError::RateLimited {
            detail: format!(
                "backend={} retry_after_ms={} requested_tokens=1 available_tokens={} refill_rate_per_s={}",
                backend.as_str(),
                retry_after_ms,
                snapshot.tokens,
                snapshot.refill_rate_per_s
            ),
            retry_after_ms,
        })
    }
}

impl BackendRateLimits {
    pub(super) fn new() -> Self {
        Self {
            software: TokenBucket::for_backend(ResolvedBackend::Software),
            vigem: TokenBucket::for_backend(ResolvedBackend::Vigem),
            hardware: TokenBucket::for_backend(ResolvedBackend::Hardware),
        }
    }

    #[cfg(test)]
    pub(super) const fn with_buckets(
        software: TokenBucket,
        vigem: TokenBucket,
        hardware: TokenBucket,
    ) -> Self {
        Self {
            software,
            vigem,
            hardware,
        }
    }

    pub(super) const fn bucket(&self, backend: ResolvedBackend) -> &TokenBucket {
        match backend {
            ResolvedBackend::Software => &self.software,
            ResolvedBackend::Vigem => &self.vigem,
            ResolvedBackend::Hardware => &self.hardware,
        }
    }

    const fn bucket_mut(&mut self, backend: ResolvedBackend) -> &mut TokenBucket {
        match backend {
            ResolvedBackend::Software => &mut self.software,
            ResolvedBackend::Vigem => &mut self.vigem,
            ResolvedBackend::Hardware => &mut self.hardware,
        }
    }

    fn replace_backend(
        &mut self,
        backend: ResolvedBackend,
        bucket: TokenBucket,
    ) -> BackendRateLimitOverrideReadback {
        let target = self.bucket_mut(backend);
        let before = target.snapshot();
        *target = bucket;
        let after = target.snapshot();
        BackendRateLimitOverrideReadback {
            backend,
            before,
            after,
        }
    }

    fn snapshot(&self) -> BackendRateLimitSnapshot {
        BackendRateLimitSnapshot {
            software: self.software.snapshot(),
            vigem: self.vigem.snapshot(),
            hardware: self.hardware.snapshot(),
        }
    }
}
