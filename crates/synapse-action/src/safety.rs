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
/// # Why the panic sweep stays bounded (#2082 finding A1)
///
/// The startup sweep was widened to scan the whole virtual-key space, because at
/// boot the process-local strand mirror is empty and `GetAsyncKeyState` is the
/// only evidence that can exist. The panic hook is the opposite case and keeps
/// the bounded sweep, for two structural reasons:
///
/// * it runs **inside the process that owns the mirror**, which is authoritative
///   for every key this process pressed — including non-modifiers, which is
///   exactly why `key_down` hands its entry to the mirror. A full scan would add
///   only keys this process never pressed, i.e. keys the **operator** is
///   physically holding, and this hook is the one most likely to run while a
///   human is mid-keystroke;
/// * it may run on a thread that panicked *because* it exhausted its stack. The
///   bounded sweep's frame is one fixed `[INPUT; 82]` and a handful of
///   `GetAsyncKeyState` calls; a full-space release would add ~250 syscalls and
///   a second staging buffer to a path whose entire contract is "the cheapest
///   thing that is still complete". A panic hook that itself faults releases
///   nothing at all.
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
