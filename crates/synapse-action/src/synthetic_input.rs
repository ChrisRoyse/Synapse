//! Last-resort release of every synthetic input this process can strand
//! **system-wide** (#2082).
//!
//! # Why a second, dumber release path exists
//!
//! `SendInput` key/button state is **global to the OS input queue, not owned by
//! the process that synthesized it**. Microsoft states the consequence directly
//! in the `SendInput` remarks: *"This function does not reset the keyboard's
//! current state. Any keys that are already pressed when the function is called
//! might interfere with the events that this function generates. To avoid this
//! problem, check the keyboard's state with the `GetAsyncKeyState` function and
//! correct as necessary."* Nothing in Win32 walks that state back when the
//! synthesizing process dies. A panic between a key-down and its key-up, or
//! between a mouse press and its release, therefore leaves the **human** unable
//! to type or click, and killing the daemon does not fix it.
//!
//! Synapse already has a release path: `RELEASE_ALL_HANDLE` -> actor task ->
//! `EmitState` -> `enigo` -> `SendInput`, plus the durable [`crate::recovery`]
//! ledger. Every link in that chain is unavailable exactly when it matters most:
//!
//! * the actor is a tokio task — a panicking thread cannot rely on a runtime
//!   that may itself be unwinding, and the round trip is a channel send with a
//!   timeout, i.e. a *best effort*;
//! * `EmitState` lives behind that channel;
//! * the recovery ledger does `read` + `open` + `write` + `sync_data` per event,
//!   which is not something to attempt from a panic hook;
//! * `foreground_fence::guard_emission` takes a process-global `Mutex` that the
//!   panicking thread may be holding.
//!
//! This module is the layer underneath all of that. It holds a lock-free,
//! allocation-free mirror of what the software backend currently has pressed,
//! and can flush a release for all of it with **raw `SendInput` calls and
//! nothing else**: no channels, no tokio, no allocation, no `Mutex`, no file
//! I/O, no fence. It is safe to call from a panic hook, from a watchdog thread,
//! and from daemon startup.
//!
//! # Release-only, never re-press
//!
//! The sweep synthesizes key-ups and never re-presses anything. That mirrors
//! `win-text-inject`'s `modifiers::sanitize` (already cited by
//! [`crate::foreground_fence`] for the target-capture hazard), which documents
//! the rule: released modifiers are *"deliberately **not** restored afterwards:
//! re-pressing a modifier the user has since physically released leaves it
//! stuck down forever, which is a far worse failure than a lost modifier."*
//! `PowerToys`' Keyboard Manager reached the same conclusion after shipping a
//! re-press and then removing it, because injecting the key-up itself flips
//! `GetAsyncKeyState` to "up" and makes any re-press guard dead code.
//!
//! # The lone Alt/Win tap hazard
//!
//! A bare `VK_MENU`/`VK_LWIN` key-up with no intervening key press is read by
//! the shell as a menu-bar / Start-menu activation. `PowerToys`' Keyboard Manager
//! fix for stuck modifiers injects a **dummy key** immediately before releasing
//! a held Alt or Win so the release is inert. This module does the same, in the
//! same `SendInput` batch as the releases (so there is no user-mode window in
//! which the dummy could itself strand), and only when Alt or Win actually
//! reads down.
//!
//! # Fence relationship
//!
//! [`crate::foreground_fence`] already classifies emissions as `Delivery` (must
//! be refused on drift) or `Release` (allowed through a tripped fence, because
//! "stranding a held key or button is worse than one stray release event").
//! Everything this module emits is a `Release` by construction. It does not
//! *call* the fence — a panic hook must not take a lock — but it preserves the
//! semantics exactly: a fence refusal must still release, and now so must a
//! panic.

use std::sync::{
    OnceLock,
    atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
};
use std::time::Instant;

use synapse_core::{Key, KeyCode, MouseButton};

/// How many concurrently held synthetic keys the lock-free mirror can track.
///
/// `act_keymap`/`act_combo` cap far below this; overflow is logged rather than
/// silently dropped, because an untracked press is exactly the defect this
/// module exists to prevent.
const STRAND_SLOTS: usize = 64;

/// Number of distinct mouse buttons ([`MouseButton`] is a closed set of five).
const BUTTON_SLOTS: usize = 5;

/// Empty sentinel for a strand slot. A real token always has a non-zero tag.
const TOKEN_EMPTY: u32 = 0;

const TAG_SHIFT: u32 = 24;
const TAG_VIRTUAL_KEY: u32 = 1 << TAG_SHIFT;
const TAG_SCANCODE: u32 = 2 << TAG_SHIFT;
const TAG_UNICODE: u32 = 3 << TAG_SHIFT;
const VALUE_MASK: u32 = 0x0000_ffff;
const TAG_MASK: u32 = 0xff00_0000;

/// Lock-free mirror of the synthetic keys the software backend has pressed.
static STRAND_TOKENS: [AtomicU32; STRAND_SLOTS] =
    [const { AtomicU32::new(TOKEN_EMPTY) }; STRAND_SLOTS];
/// Monotonic milliseconds (process epoch) at which each strand was pressed.
static STRAND_SINCE_MS: [AtomicU64; STRAND_SLOTS] = [const { AtomicU64::new(0) }; STRAND_SLOTS];
/// Monotonic milliseconds at which each mouse button was pressed; `0` = up.
static BUTTON_SINCE_MS: [AtomicU64; BUTTON_SLOTS] = [const { AtomicU64::new(0) }; BUTTON_SLOTS];
/// Count of presses that could not be mirrored because every slot was taken.
static STRAND_OVERFLOWS: AtomicU64 = AtomicU64::new(0);

static WATCHDOG_STARTED: AtomicBool = AtomicBool::new(false);

fn epoch() -> Instant {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    *EPOCH.get_or_init(Instant::now)
}

/// Monotonic milliseconds since the first use of this module.
///
/// Never `0` for a live strand: `0` is the "not held" sentinel, so a press that
/// lands in the very first millisecond is recorded as `1`.
fn now_ms() -> u64 {
    u64::try_from(epoch().elapsed().as_millis())
        .unwrap_or(u64::MAX)
        .max(1)
}

#[cfg_attr(not(windows), allow(dead_code))]
#[allow(clippy::cast_possible_truncation)]
const fn token_value(token: u32) -> u16 {
    // Masked to 16 bits immediately above, so the truncation is the intent.
    (token & VALUE_MASK) as u16
}

#[cfg_attr(not(windows), allow(dead_code))]
const fn token_tag(token: u32) -> u32 {
    token & TAG_MASK
}

/// Encodes the exact OS form a [`Key`] is emitted in, so the release the sweep
/// synthesizes is the same shape as the press `enigo` emitted.
///
/// Mirrors `backend::software::keyboard::emit_key`/`named_key`: a scancode key
/// releases as a scancode, a symbol or single-character named key releases as a
/// UTF-16 unit with `KEYEVENTF_UNICODE`, and a named modifier/editing key
/// releases as its virtual key.
#[must_use]
pub fn key_strand_token(key: &Key) -> Option<u32> {
    if key.use_scancode {
        let KeyCode::HidCode { value } = &key.code else {
            return None;
        };
        return Some(TAG_SCANCODE | u32::from(*value));
    }
    match &key.code {
        KeyCode::HidCode { .. } => None,
        KeyCode::Symbol { value } => unicode_token(*value),
        KeyCode::Named { value } => named_token(value),
    }
}

fn unicode_token(value: char) -> Option<u32> {
    let mut buffer = [0_u16; 2];
    let units = value.encode_utf16(&mut buffer);
    // A surrogate pair emits two units; the first is the one whose release the
    // OS pairs with the press, and a stranded astral character is not a
    // modifier that can disable the keyboard.
    units.first().map(|unit| TAG_UNICODE | u32::from(*unit))
}

/// Named key -> virtual key, matching what `enigo` emits for the same
/// `EnigoKey` on Windows.
fn named_token(value: &str) -> Option<u32> {
    let lower = value.to_ascii_lowercase();
    let mut chars = lower.chars();
    if let Some(single) = chars.next()
        && chars.next().is_none()
    {
        return unicode_token(single);
    }
    let vkey: u16 = match lower.as_str() {
        "alt" => 0x12,
        "backspace" => 0x08,
        "ctrl" | "control" => 0x11,
        "delete" => 0x2e,
        "down" | "arrowdown" => 0x28,
        "end" => 0x23,
        "enter" | "return" => 0x0d,
        "escape" | "esc" => 0x1b,
        "home" => 0x24,
        "insert" => 0x2d,
        "left" | "arrowleft" => 0x25,
        "meta" | "win" | "windows" | "super" => 0x5b,
        "pagedown" => 0x22,
        "pageup" => 0x21,
        "right" | "arrowright" => 0x27,
        "shift" | "leftshift" | "lshift" | "rightshift" | "rshift" => 0x10,
        "space" => 0x20,
        "tab" => 0x09,
        "up" | "arrowup" => 0x26,
        "f1" => 0x70,
        "f2" => 0x71,
        "f3" => 0x72,
        "f4" => 0x73,
        "f5" => 0x74,
        "f6" => 0x75,
        "f7" => 0x76,
        "f8" => 0x77,
        "f9" => 0x78,
        "f10" => 0x79,
        "f11" => 0x7a,
        "f12" => 0x7b,
        _ => return None,
    };
    Some(TAG_VIRTUAL_KEY | u32::from(vkey))
}

const fn button_index(button: MouseButton) -> usize {
    match button {
        MouseButton::Left => 0,
        MouseButton::Right => 1,
        MouseButton::Middle => 2,
        MouseButton::X1 => 3,
        MouseButton::X2 => 4,
    }
}

const fn button_from_index(index: usize) -> Option<MouseButton> {
    match index {
        0 => Some(MouseButton::Left),
        1 => Some(MouseButton::Right),
        2 => Some(MouseButton::Middle),
        3 => Some(MouseButton::X1),
        4 => Some(MouseButton::X2),
        _ => None,
    }
}

const fn button_label(button: MouseButton) -> &'static str {
    match button {
        MouseButton::Left => "left",
        MouseButton::Right => "right",
        MouseButton::Middle => "middle",
        MouseButton::X1 => "x1",
        MouseButton::X2 => "x2",
    }
}

/// Records that `token` is now physically down.
///
/// Duplicate registrations of the same token take no extra slot: the release is
/// idempotent, so one mirror entry per distinct key is exactly what the sweep
/// needs.
pub fn register_key_strand(token: u32) {
    if token == TOKEN_EMPTY {
        return;
    }
    let pressed_at = now_ms();
    if STRAND_TOKENS
        .iter()
        .any(|cell| cell.load(Ordering::Acquire) == token)
    {
        return;
    }
    for (cell, since) in STRAND_TOKENS.iter().zip(STRAND_SINCE_MS.iter()) {
        if cell
            .compare_exchange(TOKEN_EMPTY, token, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            since.store(pressed_at, Ordering::Release);
            return;
        }
    }
    let overflows = STRAND_OVERFLOWS.fetch_add(1, Ordering::Relaxed) + 1;
    tracing::error!(
        code = "SYNTHETIC_INPUT_STRAND_MIRROR_FULL",
        token,
        slots = STRAND_SLOTS,
        overflows,
        "a synthetic key press could not be mirrored: a panic or watchdog sweep will not know to release it"
    );
}

/// Forgets `token`: its matching release has been emitted.
pub fn clear_key_strand(token: u32) {
    if token == TOKEN_EMPTY {
        return;
    }
    for (cell, since) in STRAND_TOKENS.iter().zip(STRAND_SINCE_MS.iter()) {
        if cell
            .compare_exchange(token, TOKEN_EMPTY, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            since.store(0, Ordering::Release);
        }
    }
}

/// Records that `button` is now physically down.
pub fn register_button_strand(button: MouseButton) {
    let pressed_at = now_ms();
    if let Some(since) = BUTTON_SINCE_MS.get(button_index(button)) {
        since.store(pressed_at, Ordering::Release);
    }
}

/// Forgets `button`: its matching release has been emitted.
pub fn clear_button_strand(button: MouseButton) {
    if let Some(since) = BUTTON_SINCE_MS.get(button_index(button)) {
        since.store(0, Ordering::Release);
    }
}

/// Clears the whole mirror without emitting anything.
///
/// Only for callers that have *already* emitted every matching release
/// (`Action::ReleaseAll`), so the watchdog does not re-release what is now up.
pub fn clear_all_strands() {
    for (cell, since) in STRAND_TOKENS.iter().zip(STRAND_SINCE_MS.iter()) {
        cell.store(TOKEN_EMPTY, Ordering::Release);
        since.store(0, Ordering::Release);
    }
    for since in &BUTTON_SINCE_MS {
        since.store(0, Ordering::Release);
    }
}

/// What a sweep found down and what it managed to release.
///
/// Every field is a plain counter so the report can be built without
/// allocating, which is what makes it usable from a panic hook.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SyntheticReleaseReport {
    /// Bitmask over [`MODIFIER_SWEEP`] of modifiers `GetAsyncKeyState` reported
    /// down at sweep entry. This is the *evidence* an inherited strand existed.
    pub modifiers_found_down: u32,
    /// Bitmask over the [`MouseButton`] index of buttons found down.
    pub buttons_found_down: u32,
    /// Mirrored strands that were still registered and got a release.
    pub tracked_strands_released: usize,
    /// Modifier key-ups emitted (always the full set: the sweep is unconditional).
    pub modifier_releases_emitted: usize,
    /// Mouse button-ups emitted (always the full set).
    pub button_releases_emitted: usize,
    /// `SendInput` batches that did not fully insert even after retry.
    pub emission_failures: usize,
    /// Whether the lone-Alt/Win dummy-key mask was injected.
    pub masked_lone_modifier_tap: bool,
}

impl SyntheticReleaseReport {
    /// True when the OS reported synthetic state still down at sweep entry.
    ///
    /// At daemon startup this is the signature of an **inherited strand** from a
    /// previous unclean generation, and must be logged loudly rather than
    /// silently cleared.
    #[must_use]
    pub const fn found_anything_down(&self) -> bool {
        self.modifiers_found_down != 0 || self.buttons_found_down != 0
    }

    /// Comma-separated names of the modifiers found down.
    #[must_use]
    pub fn modifiers_found_down_labels(&self) -> String {
        let mut out = String::new();
        for (index, (_vkey, label)) in MODIFIER_SWEEP.iter().enumerate() {
            if self.modifiers_found_down & (1 << index) != 0 {
                if !out.is_empty() {
                    out.push(',');
                }
                out.push_str(label);
            }
        }
        out
    }

    /// Comma-separated names of the mouse buttons found down.
    #[must_use]
    pub fn buttons_found_down_labels(&self) -> String {
        let mut out = String::new();
        for index in 0..BUTTON_SLOTS {
            if self.buttons_found_down & (1 << index) != 0
                && let Some(button) = button_from_index(index)
            {
                if !out.is_empty() {
                    out.push(',');
                }
                out.push_str(button_label(button));
            }
        }
        out
    }
}

/// Every modifier whose stranded key-down disables the human's keyboard, both
/// `L`/`R` variants plus the merged virtual key, in release order.
///
/// A stranded `Alt` alone turns every keystroke into a menu accelerator, which
/// is exactly the operator-reported symptom in #2082.
pub const MODIFIER_SWEEP: [(u16, &str); 11] = [
    (0x10, "shift"),
    (0xa0, "lshift"),
    (0xa1, "rshift"),
    (0x11, "ctrl"),
    (0xa2, "lctrl"),
    (0xa3, "rctrl"),
    (0x12, "alt"),
    (0xa4, "lalt"),
    (0xa5, "ralt"),
    (0x5b, "lwin"),
    (0x5c, "rwin"),
];

/// Unassigned virtual key used to absorb a lone Alt/Win release so the shell
/// does not read it as a menu-bar / Start-menu activation (the `PowerToys`
/// Keyboard Manager "dummy key" remedy).
#[cfg_attr(not(windows), allow(dead_code))]
const DUMMY_MASK_VKEY: u16 = 0xff;

#[cfg(windows)]
mod win32 {
    use std::mem;
    use std::sync::atomic::Ordering;

    use windows::Win32::UI::Input::KeyboardAndMouse::{
        GetAsyncKeyState, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBD_EVENT_FLAGS,
        KEYBDINPUT, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, KEYEVENTF_UNICODE, MOUSE_EVENT_FLAGS,
        MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_XUP, MOUSEINPUT,
        SendInput, VIRTUAL_KEY,
    };

    use super::{
        BUTTON_SINCE_MS, BUTTON_SLOTS, DUMMY_MASK_VKEY, MODIFIER_SWEEP, MouseButton,
        STRAND_SINCE_MS, STRAND_SLOTS, STRAND_TOKENS, SyntheticReleaseReport, TAG_SCANCODE,
        TAG_UNICODE, TAG_VIRTUAL_KEY, TOKEN_EMPTY, button_from_index, button_index, token_tag,
        token_value,
    };

    const XBUTTON1_DATA: u32 = 0x0001;
    const XBUTTON2_DATA: u32 = 0x0002;

    /// Head room for: every tracked release, every modifier, every button and
    /// the two-event dummy mask.
    const MAX_BATCH: usize = STRAND_SLOTS + MODIFIER_SWEEP.len() + BUTTON_SLOTS + 2;

    fn input_size() -> i32 {
        i32::try_from(mem::size_of::<INPUT>()).unwrap_or(0)
    }

    const fn key_input(vkey: u16, scan: u16, flags: KEYBD_EVENT_FLAGS) -> INPUT {
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(vkey),
                    wScan: scan,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        }
    }

    const fn mouse_up_input(flags: MOUSE_EVENT_FLAGS, mouse_data: u32) -> INPUT {
        INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx: 0,
                    dy: 0,
                    mouseData: mouse_data,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        }
    }

    /// The release form for one mirrored strand token.
    pub(super) const fn release_input_for_token(token: u32) -> Option<INPUT> {
        let value = token_value(token);
        match token_tag(token) {
            TAG_VIRTUAL_KEY => Some(key_input(value, 0, KEYEVENTF_KEYUP)),
            TAG_SCANCODE => Some(key_input(
                0,
                value,
                KEYBD_EVENT_FLAGS(KEYEVENTF_SCANCODE.0 | KEYEVENTF_KEYUP.0),
            )),
            TAG_UNICODE => Some(key_input(
                0,
                value,
                KEYBD_EVENT_FLAGS(KEYEVENTF_UNICODE.0 | KEYEVENTF_KEYUP.0),
            )),
            _ => None,
        }
    }

    pub(super) const fn release_input_for_button(button: MouseButton) -> INPUT {
        match button {
            MouseButton::Left => mouse_up_input(MOUSEEVENTF_LEFTUP, 0),
            MouseButton::Right => mouse_up_input(MOUSEEVENTF_RIGHTUP, 0),
            MouseButton::Middle => mouse_up_input(MOUSEEVENTF_MIDDLEUP, 0),
            MouseButton::X1 => mouse_up_input(MOUSEEVENTF_XUP, XBUTTON1_DATA),
            MouseButton::X2 => mouse_up_input(MOUSEEVENTF_XUP, XBUTTON2_DATA),
        }
    }

    /// One raw `SendInput`, retried once, with no allocation and no fence.
    ///
    /// Retrying is safe because everything this module emits is a release, and
    /// releases are idempotent: a duplicate key-up for an already-up key is a
    /// no-op in the OS input queue. Returns `true` when every event was
    /// inserted.
    pub(super) fn send_release_batch(inputs: &[INPUT]) -> bool {
        if inputs.is_empty() {
            return true;
        }
        let cb_size = input_size();
        let expected = u32::try_from(inputs.len()).unwrap_or(u32::MAX);
        for _attempt in 0..2 {
            // SAFETY: `inputs` are initialized Windows `INPUT` records that live
            // for the duration of the call and `cb_size` is `size_of::<INPUT>()`.
            let sent = unsafe { SendInput(inputs, cb_size) };
            if sent == expected {
                return true;
            }
        }
        false
    }

    /// `GetAsyncKeyState` sets the high bit while the key is down; for the
    /// returned `i16` that bit is the sign bit.
    fn is_down(vkey: u16) -> bool {
        // SAFETY: `GetAsyncKeyState` takes a plain virtual-key code and touches
        // no caller-owned memory.
        let state = unsafe { GetAsyncKeyState(i32::from(vkey)) };
        state < 0
    }

    const fn button_virtual_key(button: MouseButton) -> u16 {
        match button {
            MouseButton::Left => 0x01,
            MouseButton::Right => 0x02,
            MouseButton::Middle => 0x04,
            MouseButton::X1 => 0x05,
            MouseButton::X2 => 0x06,
        }
    }

    const fn is_alt_or_win(vkey: u16) -> bool {
        matches!(vkey, 0x12 | 0xa4 | 0xa5 | 0x5b | 0x5c)
    }

    /// Releases everything, unconditionally, in one raw `SendInput` batch.
    pub(super) fn sweep() -> SyntheticReleaseReport {
        let mut report = SyntheticReleaseReport::default();

        // 1. Evidence first: what does the OS say is down right now? This is the
        //    only readback, and it is what makes an inherited strand *visible*
        //    rather than silently cleared.
        for (index, (vkey, _label)) in MODIFIER_SWEEP.iter().enumerate() {
            if is_down(*vkey) {
                report.modifiers_found_down |= 1 << index;
            }
        }
        for index in 0..BUTTON_SLOTS {
            if let Some(button) = button_from_index(index)
                && is_down(button_virtual_key(button))
            {
                report.buttons_found_down |= 1 << index;
            }
        }

        let mut batch: [INPUT; MAX_BATCH] = [const { key_input(0, 0, KEYEVENTF_KEYUP) }; MAX_BATCH];
        let mut len = 0_usize;

        // 2. Whatever the action layer tracks as held, in its exact emitted form.
        for (cell, since) in STRAND_TOKENS.iter().zip(STRAND_SINCE_MS.iter()) {
            let token = cell.swap(TOKEN_EMPTY, Ordering::AcqRel);
            since.store(0, Ordering::Release);
            if token == TOKEN_EMPTY {
                continue;
            }
            if let Some(input) = release_input_for_token(token)
                && len < MAX_BATCH
            {
                batch[len] = input;
                len += 1;
                report.tracked_strands_released += 1;
            }
        }

        // 3. The lone Alt/Win tap mask, injected inside the same batch so it can
        //    never itself strand.
        let alt_or_win_down = MODIFIER_SWEEP
            .iter()
            .enumerate()
            .any(|(index, (vkey, _label))| {
                report.modifiers_found_down & (1 << index) != 0 && is_alt_or_win(*vkey)
            });
        if alt_or_win_down && len + 2 <= MAX_BATCH {
            batch[len] = key_input(DUMMY_MASK_VKEY, 0, KEYBD_EVENT_FLAGS(0));
            batch[len + 1] = key_input(DUMMY_MASK_VKEY, 0, KEYEVENTF_KEYUP);
            len += 2;
            report.masked_lone_modifier_tap = true;
        }

        // 4. Every modifier, unconditionally. Release-only; nothing is re-pressed.
        for (vkey, _label) in &MODIFIER_SWEEP {
            if len < MAX_BATCH {
                batch[len] = key_input(*vkey, 0, KEYEVENTF_KEYUP);
                len += 1;
                report.modifier_releases_emitted += 1;
            }
        }

        // 5. Every mouse button, unconditionally.
        for index in 0..BUTTON_SLOTS {
            if let Some(button) = button_from_index(index) {
                if let Some(since) = BUTTON_SINCE_MS.get(button_index(button)) {
                    since.store(0, Ordering::Release);
                }
                if len < MAX_BATCH {
                    batch[len] = release_input_for_button(button);
                    len += 1;
                    report.button_releases_emitted += 1;
                }
            }
        }

        if !send_release_batch(&batch[..len]) {
            report.emission_failures += 1;
        }
        report
    }
}

#[cfg(not(windows))]
mod win32 {
    use super::{MouseButton, SyntheticReleaseReport};

    pub(super) const fn release_token(_token: u32) -> bool {
        true
    }

    pub(super) const fn release_button(_button: MouseButton) -> bool {
        true
    }

    pub(super) fn sweep() -> SyntheticReleaseReport {
        SyntheticReleaseReport::default()
    }
}

#[cfg(windows)]
fn release_token_now(token: u32) -> bool {
    win32::release_input_for_token(token)
        .is_none_or(|input| win32::send_release_batch(std::slice::from_ref(&input)))
}

#[cfg(not(windows))]
fn release_token_now(token: u32) -> bool {
    win32::release_token(token)
}

#[cfg(windows)]
fn release_button_now(button: MouseButton) -> bool {
    let input = win32::release_input_for_button(button);
    win32::send_release_batch(std::slice::from_ref(&input))
}

#[cfg(not(windows))]
fn release_button_now(button: MouseButton) -> bool {
    win32::release_button(button)
}

/// Releases **everything** this process could have stranded, unconditionally,
/// with raw `SendInput` and nothing else.
///
/// Safe to call from a panic hook: no allocation on the emission path, no
/// channels, no tokio, no `Mutex`, no file I/O, no foreground fence. The
/// returned report says what the OS had down at entry, so the caller can log an
/// inherited strand instead of silently clearing it.
#[must_use]
pub fn release_all_synthetic_input() -> SyntheticReleaseReport {
    win32::sweep()
}

/// Startup sweep (#2082 fix 2): release unconditionally on daemon boot and log
/// exactly what was found still down.
///
/// A daemon that starts after any unclean exit inherits whatever synthetic state
/// the previous generation stranded, and `SendInput` state does not die with the
/// process. The evidence is `GetAsyncKeyState` at entry — the exact readback
/// Microsoft's `SendInput` remarks point at for this problem.
pub fn release_all_synthetic_input_on_startup() -> SyntheticReleaseReport {
    let report = release_all_synthetic_input();
    if report.found_anything_down() {
        tracing::warn!(
            code = "SYNTHETIC_INPUT_STARTUP_STRAND_RELEASED",
            modifiers_found_down = %report.modifiers_found_down_labels(),
            buttons_found_down = %report.buttons_found_down_labels(),
            tracked_strands_released = report.tracked_strands_released,
            modifier_releases_emitted = report.modifier_releases_emitted,
            button_releases_emitted = report.button_releases_emitted,
            masked_lone_modifier_tap = report.masked_lone_modifier_tap,
            emission_failures = report.emission_failures,
            source_of_truth = "GetAsyncKeyState read before the startup SendInput release sweep",
            "readback=synthetic_input edge=startup inherited synthetic input was still down and has been released; a previous daemon generation exited without releasing it"
        );
    } else {
        tracing::info!(
            code = "SYNTHETIC_INPUT_STARTUP_SWEEP_CLEAN",
            modifier_releases_emitted = report.modifier_releases_emitted,
            button_releases_emitted = report.button_releases_emitted,
            emission_failures = report.emission_failures,
            source_of_truth = "GetAsyncKeyState read before the startup SendInput release sweep",
            "readback=synthetic_input edge=startup no synthetic input was down; the unconditional release sweep ran anyway"
        );
    }
    if report.emission_failures > 0 {
        tracing::error!(
            code = "SYNTHETIC_INPUT_RELEASE_EMISSION_FAILED",
            phase = "startup",
            emission_failures = report.emission_failures,
            "the startup synthetic-input release sweep could not insert its release batch even after retry; SendInput is being blocked (UIPI or another thread)"
        );
    }
    report
}

/// Panic-path sweep (#2082 fix 1).
///
/// Called from the panic hook **before** anything else: before the panic is
/// recorded, before any previous hook is chained, and without consulting any
/// state a panicking thread might already own. Logging happens after the
/// release, never before it.
pub fn release_all_synthetic_input_on_panic() -> SyntheticReleaseReport {
    let report = release_all_synthetic_input();
    tracing::error!(
        code = "SYNTHETIC_INPUT_PANIC_RELEASED",
        modifiers_found_down = %report.modifiers_found_down_labels(),
        buttons_found_down = %report.buttons_found_down_labels(),
        tracked_strands_released = report.tracked_strands_released,
        modifier_releases_emitted = report.modifier_releases_emitted,
        button_releases_emitted = report.button_releases_emitted,
        masked_lone_modifier_tap = report.masked_lone_modifier_tap,
        emission_failures = report.emission_failures,
        "panic hook released all synthetic input before recording the panic: SendInput key state is global to the OS input queue and does not die with this process"
    );
    report
}

// -----------------------------------------------------------------------------
// RAII guards
// -----------------------------------------------------------------------------

/// RAII guard for one synthetic key that is currently down.
///
/// Armed immediately *before* the press leaves, so a panic in the emission
/// itself still releases (a duplicate key-up is a harmless no-op; an unpaired
/// key-down is not). [`Self::disarm`] is called only once the matching release
/// has provably left the OS; every other exit — `?`, early return, unwind —
/// runs [`Drop`], which emits the release with raw `SendInput` and logs at
/// `ERROR`.
#[derive(Debug)]
pub struct HeldKeyStrand {
    token: u32,
    stage: &'static str,
}

impl HeldKeyStrand {
    /// Arms a guard for `key` and mirrors it, so a panic sweep also sees it.
    ///
    /// A key with no representable OS release form yields an inert guard; such a
    /// key cannot be emitted either, so there is nothing to strand.
    #[must_use]
    pub fn arm(key: &Key, stage: &'static str) -> Self {
        let token = key_strand_token(key).unwrap_or(TOKEN_EMPTY);
        register_key_strand(token);
        Self { token, stage }
    }

    /// Adopts an already-mirrored key so a scoped body is unwind-safe.
    #[must_use]
    pub const fn adopt(token: u32, stage: &'static str) -> Self {
        Self { token, stage }
    }

    /// The matching release has left the OS; stop guarding.
    pub fn disarm(&mut self) {
        if self.token != TOKEN_EMPTY {
            clear_key_strand(self.token);
            self.token = TOKEN_EMPTY;
        }
    }

    /// Stops guarding but **keeps** the mirror entry, because the press is
    /// meant to outlive this scope (`act_key_down` released by a later
    /// `act_key_up`). The panic sweep, startup sweep and watchdog still see it.
    pub const fn disarm_without_clearing_mirror(&mut self) {
        self.token = TOKEN_EMPTY;
    }
}

/// Emits the release for one mirrored token immediately, through the same raw,
/// fence-free, allocation-free path the panic sweep uses.
///
/// The retry that keeps the release path from being the thing that fails: a
/// higher layer whose normal (fenced, `enigo`) release errored calls this and
/// logs at `ERROR` rather than returning early with the key still down.
/// Returns `true` when the release was inserted.
#[must_use]
pub fn force_release_key_token(token: u32) -> bool {
    if token == TOKEN_EMPTY {
        return true;
    }
    let released = release_token_now(token);
    clear_key_strand(token);
    released
}

/// Emits the release for one mouse button immediately, through the raw path.
#[must_use]
pub fn force_release_button(button: MouseButton) -> bool {
    let released = release_button_now(button);
    clear_button_strand(button);
    released
}

impl Drop for HeldKeyStrand {
    fn drop(&mut self) {
        if self.token == TOKEN_EMPTY {
            return;
        }
        let token = self.token;
        let released = release_token_now(token);
        clear_key_strand(token);
        self.token = TOKEN_EMPTY;
        if released {
            tracing::error!(
                code = "SYNTHETIC_INPUT_GUARD_RELEASED",
                stage = self.stage,
                token,
                "a synthetic key press left its scope without a matching release (error or unwind); the RAII guard emitted the key-up"
            );
        } else {
            tracing::error!(
                code = "SYNTHETIC_INPUT_RELEASE_EMISSION_FAILED",
                phase = "key_guard_drop",
                stage = self.stage,
                token,
                "the RAII key-up could not be inserted even after retry; the key may still be down system-wide"
            );
        }
    }
}

/// RAII guard for one synthetic mouse button that is currently down.
///
/// Same contract as [`HeldKeyStrand`]. A stranded left button is the half of the
/// #2082 symptom triple that stops the human clicking anything.
#[derive(Debug)]
pub struct HeldButtonStrand {
    button: Option<MouseButton>,
    stage: &'static str,
}

impl HeldButtonStrand {
    /// Arms a guard for `button` and mirrors it.
    #[must_use]
    pub fn arm(button: MouseButton, stage: &'static str) -> Self {
        register_button_strand(button);
        Self {
            button: Some(button),
            stage,
        }
    }

    /// Adopts an already-mirrored button so a scoped body is unwind-safe.
    #[must_use]
    pub const fn adopt(button: MouseButton, stage: &'static str) -> Self {
        Self {
            button: Some(button),
            stage,
        }
    }

    /// The matching release has left the OS; stop guarding.
    pub fn disarm(&mut self) {
        if let Some(button) = self.button.take() {
            clear_button_strand(button);
        }
    }

    /// Stops guarding but **keeps** the mirror entry, because the press is meant
    /// to outlive this scope (`act_mouse_button action=down` released by a later
    /// `action=up`). The panic sweep, startup sweep and watchdog still see it.
    pub const fn disarm_without_clearing_mirror(&mut self) {
        self.button = None;
    }
}

impl Drop for HeldButtonStrand {
    fn drop(&mut self) {
        let Some(button) = self.button.take() else {
            return;
        };
        let released = release_button_now(button);
        clear_button_strand(button);
        if released {
            tracing::error!(
                code = "SYNTHETIC_INPUT_GUARD_RELEASED",
                stage = self.stage,
                button = button_label(button),
                "a synthetic mouse press left its scope without a matching release (error or unwind); the RAII guard emitted the button-up"
            );
        } else {
            tracing::error!(
                code = "SYNTHETIC_INPUT_RELEASE_EMISSION_FAILED",
                phase = "button_guard_drop",
                stage = self.stage,
                button = button_label(button),
                "the RAII button-up could not be inserted even after retry; the button may still be down system-wide"
            );
        }
    }
}

// -----------------------------------------------------------------------------
// Watchdog
// -----------------------------------------------------------------------------

/// How often the watchdog thread re-reads the strand mirror.
const WATCHDOG_TICK_MS: u64 = 1_000;

/// Releases any strand held longer than the current planned-emission bound.
///
/// The bound comes from [`crate::lease::synthetic_hold_watchdog_bound_ms`]: the
/// #2065/#2071 emission-budget ceiling while an action is provably mid-plan, and
/// the much tighter held-key ceiling when nothing is armed. Returns how many
/// strands were force-released.
pub fn watchdog_tick() -> usize {
    let bound_ms = crate::lease::synthetic_hold_watchdog_bound_ms();
    let now = now_ms();
    let mut released = 0_usize;

    for (cell, since_cell) in STRAND_TOKENS.iter().zip(STRAND_SINCE_MS.iter()) {
        let token = cell.load(Ordering::Acquire);
        let since = since_cell.load(Ordering::Acquire);
        if token == TOKEN_EMPTY || since == 0 {
            continue;
        }
        let held_ms = now.saturating_sub(since);
        if held_ms < bound_ms {
            continue;
        }
        if cell
            .compare_exchange(token, TOKEN_EMPTY, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            continue;
        }
        since_cell.store(0, Ordering::Release);
        let emitted = release_token_now(token);
        released += 1;
        tracing::error!(
            code = "SYNTHETIC_INPUT_WATCHDOG_RELEASED",
            kind = "key",
            token,
            held_ms,
            bound_ms,
            emitted,
            "a synthetic key was held past the planned-emission budget and has been force-released"
        );
        audit_watchdog_release("key", token, None, held_ms, bound_ms, emitted);
        if !emitted {
            tracing::error!(
                code = "SYNTHETIC_INPUT_RELEASE_EMISSION_FAILED",
                phase = "watchdog_key",
                token,
                "the watchdog key-up could not be inserted even after retry; the key may still be down system-wide"
            );
        }
    }

    for index in 0..BUTTON_SLOTS {
        let Some(since_cell) = BUTTON_SINCE_MS.get(index) else {
            continue;
        };
        let since = since_cell.load(Ordering::Acquire);
        if since == 0 {
            continue;
        }
        let held_ms = now.saturating_sub(since);
        if held_ms < bound_ms {
            continue;
        }
        let Some(button) = button_from_index(index) else {
            continue;
        };
        if since_cell
            .compare_exchange(since, 0, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            continue;
        }
        let emitted = release_button_now(button);
        released += 1;
        tracing::error!(
            code = "SYNTHETIC_INPUT_WATCHDOG_RELEASED",
            kind = "button",
            button = button_label(button),
            held_ms,
            bound_ms,
            emitted,
            "a synthetic mouse button was held past the planned-emission budget and has been force-released"
        );
        audit_watchdog_release(
            "button",
            0,
            Some(button_label(button)),
            held_ms,
            bound_ms,
            emitted,
        );
        if !emitted {
            tracing::error!(
                code = "SYNTHETIC_INPUT_RELEASE_EMISSION_FAILED",
                phase = "watchdog_button",
                button = button_label(button),
                "the watchdog button-up could not be inserted even after retry; the button may still be down system-wide"
            );
        }
    }

    released
}

/// Durable audit record for a watchdog force-release.
///
/// Written next to the crash-recovery ledger so a post-hoc investigation of
/// "my keyboard died" has evidence that survives the daemon. The release has
/// already happened by the time this runs; a failed audit write is logged at
/// `ERROR` and never allowed to matter more than the release.
fn audit_watchdog_release(
    kind: &str,
    token: u32,
    button: Option<&str>,
    held_ms: u64,
    bound_ms: u64,
    emitted: bool,
) {
    use std::io::Write as _;

    let ledger = crate::recovery::configured_crash_recovery_file();
    let Some(directory) = ledger.parent() else {
        return;
    };
    let path = directory.join("synthetic_input_watchdog.jsonl");
    let record = serde_json::json!({
        "code": "SYNTHETIC_INPUT_WATCHDOG_RELEASED",
        "kind": kind,
        "token": token,
        "button": button,
        "held_ms": held_ms,
        "bound_ms": bound_ms,
        "release_emitted": emitted,
        "pid": std::process::id(),
        "recorded_at_unix_ms": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_millis()),
    });
    let Ok(mut encoded) = serde_json::to_vec(&record) else {
        return;
    };
    encoded.push(b'\n');
    let write_result = std::fs::create_dir_all(directory).and_then(|()| {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .and_then(|mut file| file.write_all(&encoded))
    });
    if let Err(error) = write_result {
        tracing::error!(
            code = "SYNTHETIC_INPUT_WATCHDOG_AUDIT_WRITE_FAILED",
            path = %path.display(),
            detail = %error,
            "watchdog force-release audit record could not be written; the release itself already happened"
        );
    }
}

/// Starts the synthetic-input watchdog on a **dedicated OS thread** (#2082 fix 4).
///
/// Deliberately not a tokio task: the failure modes this watchdog exists for
/// include a wedged or dead runtime and a wedged emitter actor. A plain thread
/// with a sleep loop and no locks keeps running through all of them. Idempotent.
///
/// Returns `true` when this call started the thread.
pub fn spawn_synthetic_input_watchdog() -> bool {
    if WATCHDOG_STARTED.swap(true, Ordering::SeqCst) {
        return false;
    }
    let spawned = std::thread::Builder::new()
        .name("synapse-input-watchdog".to_owned())
        .spawn(|| {
            loop {
                std::thread::sleep(std::time::Duration::from_millis(WATCHDOG_TICK_MS));
                let _released = watchdog_tick();
            }
        });
    match spawned {
        Ok(_handle) => {
            tracing::info!(
                code = "SYNTHETIC_INPUT_WATCHDOG_STARTED",
                tick_ms = WATCHDOG_TICK_MS,
                strand_slots = STRAND_SLOTS,
                "synthetic-input watchdog thread started; synthetic input held past the planned-emission budget is force-released"
            );
            true
        }
        Err(error) => {
            WATCHDOG_STARTED.store(false, Ordering::SeqCst);
            tracing::error!(
                code = "SYNTHETIC_INPUT_WATCHDOG_START_FAILED",
                detail = %error,
                "could not start the synthetic-input watchdog thread; stuck synthetic input will rely on the panic hook and release_all alone"
            );
            false
        }
    }
}
