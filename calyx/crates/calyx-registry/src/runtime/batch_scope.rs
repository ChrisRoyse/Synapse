use std::cell::Cell;

use calyx_core::{CalyxError, Result};

thread_local! {
    static SCOPED_RUNTIME_BATCH_LIMIT: Cell<Option<usize>> = const { Cell::new(None) };
}

pub(crate) fn with_runtime_batch_limit<T>(
    limit: Option<usize>,
    run: impl FnOnce() -> Result<T>,
) -> Result<T> {
    if limit == Some(0) {
        return Err(CalyxError::lens_unreachable(
            "ONNX runtime batch limit must be > 0 when supplied",
        ));
    }
    SCOPED_RUNTIME_BATCH_LIMIT.with(|slot| {
        let previous = slot.replace(limit);
        let result = run();
        slot.set(previous);
        result
    })
}

#[cfg(feature = "embedding-runtimes")]
pub(crate) fn scoped_max_batch(spec_max: Option<usize>) -> Result<Option<usize>> {
    if spec_max == Some(0) {
        return Err(CalyxError {
            code: "CALYX_LENS_CONFIG_INVALID",
            message: "LensSpec max_batch must be > 0".to_string(),
            remediation: "fix persisted LensSpec runtime fields or re-register the lens",
        });
    }
    let scoped = SCOPED_RUNTIME_BATCH_LIMIT.with(Cell::get);
    let out = match (spec_max, scoped) {
        (Some(spec), Some(limit)) => Some(spec.min(limit)),
        (Some(spec), None) => Some(spec),
        (None, Some(limit)) => Some(limit),
        (None, None) => None,
    };
    Ok(out)
}
