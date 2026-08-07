use std::{panic, sync::OnceLock, time::Duration};

use synapse_core::error_codes;

use crate::RELEASE_ALL_HANDLE;

const PANIC_RELEASE_ALL_TIMEOUT_MS: u64 = 10;

static PANIC_HOOK_INSTALLED: OnceLock<()> = OnceLock::new();

/// Installs the action-layer panic hook.
///
/// # Ordering is the whole point (#2082)
///
/// `SendInput` key and button state is global to the OS input queue and is not
/// owned by the process that synthesized it. A panic between a key-down and its
/// key-up strands that key **system-wide**, and process death does not clear it:
/// the human is left unable to type or click. So the very first thing this hook
/// does — before recording anything, before chaining to the previous hook, and
/// without consulting any state the panicking thread might already own — is a
/// raw, allocation-free, lock-free `SendInput` release sweep
/// ([`crate::synthetic_input::release_all_synthetic_input_on_panic`]).
///
/// The pre-existing `RELEASE_ALL_HANDLE` round trip runs *after* that, and is
/// now explicitly the second line of defence rather than the first: it is a
/// bounded channel send into a tokio actor, so it is a best effort that fails
/// exactly when it is needed most (a panicking runtime, a wedged emitter, or a
/// panic raised on the emitter task itself). It is kept because it also drains
/// `EmitState`, the recovery ledger and the `ViGEm` pads, which the raw sweep
/// deliberately does not touch.
#[tracing::instrument(skip_all)]
pub fn install_panic_hook() {
    PANIC_HOOK_INSTALLED.get_or_init(|| {
        let previous_hook = panic::take_hook();
        panic::set_hook(Box::new(move |info| {
            // 1. Release, unconditionally, with nothing but raw SendInput.
            let _report = crate::synthetic_input::release_all_synthetic_input_on_panic();

            // 2. Then the richer, fallible drain of actor-held state.
            if let Some(handle) = RELEASE_ALL_HANDLE.get() {
                let timeout = Duration::from_millis(PANIC_RELEASE_ALL_TIMEOUT_MS);
                match handle.fire_release_all_blocking_with_timeout(timeout) {
                    Ok(()) => {
                        tracing::warn!(
                            code = error_codes::SAFETY_RELEASE_ALL_FIRED,
                            reason = "panic",
                            timeout_ms = PANIC_RELEASE_ALL_TIMEOUT_MS,
                            result = "ok",
                            "panic hook fired release_all after the raw synthetic-input sweep"
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
                            "panic hook release_all round trip failed; the raw synthetic-input sweep already released key and button state"
                        );
                    }
                }
            }

            previous_hook(info);
        }));
    });
}
