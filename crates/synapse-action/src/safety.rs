use std::{panic, sync::OnceLock, time::Duration};

use synapse_core::error_codes;

use crate::RELEASE_ALL_HANDLE;

const PANIC_RELEASE_ALL_TIMEOUT_MS: u64 = 10;

static PANIC_HOOK_INSTALLED: OnceLock<()> = OnceLock::new();

#[tracing::instrument(skip_all)]
pub fn install_panic_hook() {
    PANIC_HOOK_INSTALLED.get_or_init(|| {
        let previous_hook = panic::take_hook();
        panic::set_hook(Box::new(move |info| {
            if let Some(handle) = RELEASE_ALL_HANDLE.get() {
                let timeout = Duration::from_millis(PANIC_RELEASE_ALL_TIMEOUT_MS);
                match handle.fire_release_all_blocking_with_timeout(timeout) {
                    Ok(()) => {
                        tracing::warn!(
                            code = error_codes::SAFETY_RELEASE_ALL_FIRED,
                            reason = "panic",
                            timeout_ms = PANIC_RELEASE_ALL_TIMEOUT_MS,
                            result = "ok",
                            "panic hook fired release_all before previous panic hook"
                        );
                    }
                    Err(error) => {
                        tracing::warn!(
                            code = error_codes::SAFETY_RELEASE_ALL_FIRED,
                            reason = "panic",
                            timeout_ms = PANIC_RELEASE_ALL_TIMEOUT_MS,
                            result = "error",
                            error_code = error.code(),
                            detail = error.detail(),
                            "panic hook attempted release_all before previous panic hook"
                        );
                    }
                }
            }

            previous_hook(info);
        }));
    });
}
