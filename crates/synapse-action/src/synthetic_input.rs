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
//!
//! # Two sources of truth, deliberately kept apart
//!
//! The sweep reads two different things and must never conflate them:
//!
//! * **The mirror** ([`STRAND_TOKENS`], [`BUTTON_SINCE_MS`]) is *"what **this
//!   process** pressed and has not yet released"*. It is **authoritative**: it
//!   names the exact OS emission form (`wVk` / scancode / UTF-16 unit) of every
//!   press this process made, which is the only way to synthesize a release of
//!   the same shape. It is also, by construction, **empty at process start** —
//!   it cannot describe a strand left behind by a *previous* generation.
//! * **`GetAsyncKeyState`** is *"what is **physically down** right now"*. It is
//!   **evidence only**: it cannot say who pressed the key, and it cannot
//!   distinguish inherited synthetic state from a key the human is physically
//!   holding at this instant.
//!
//! That asymmetry sets the scope of each sweep:
//!
//! * The **panic** sweep runs *inside the process that owns the mirror*, so the
//!   mirror already names every non-modifier key this process could have
//!   stranded. Widening its evidence to the whole VK space would add only keys
//!   this process never pressed — i.e. keys the operator is holding — so it
//!   stays [bounded](win32::sweep).
//! * The **startup** sweep runs when the mirror is empty and the strand, if any,
//!   belongs to a dead process. `GetAsyncKeyState` alone cannot tell that strand
//!   apart from a key the operator's hand is on, so it is never sufficient
//!   authority to emit: the startup path takes its *ownership* evidence from the
//!   durable cross-generation ledger ([`crate::recovery`]) and uses the
//!   [whole-virtual-key-space scan](win32::sweep_full) only to widen a release
//!   the ledger has already justified (#2082 finding A1: a stranded `VK_F13`
//!   survived the old modifier-only boot sweep while a stranded `XBUTTON2` was
//!   correctly released, and the log still said `..._SWEEP_CLEAN`).
//!
//! # Why the startup sweep is gated (operator-interference incident, 2026-08-25)
//!
//! The startup path used to release every key the scan found down, on the
//! recorded argument that "the worst case is one interrupted auto-repeat,
//! against a strand that survives every reboot of the daemon". That trade is
//! only sound while daemon boots are *rare*. On 2026-08-25 the daemon entered an
//! out-of-memory crash loop inside its 810 MB Job cap and booted every ~85
//! seconds for hours; each boot re-ran the blind sweep and released whatever the
//! operator was physically holding. The recorded evidence is unambiguous — boots
//! that released `W`+`RBUTTON`, `W`+`D`+`RBUTTON`, `SPACE`, `1` and `LBUTTON`
//! straight out of the operator's hands, while the ownership ledger reported
//! `recovered_keys=0` on all 34 of those boots, i.e. the daemon provably held
//! nothing. A boot storm turns "one interrupted auto-repeat" into continuous
//! input hijacking, so the rare-boot premise is now checked rather than assumed:
//! see [`StartupEvidence`].

use std::fmt::Write as _;
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
/// Cumulative count of strands the watchdog has force-released this generation.
static WATCHDOG_FORCE_RELEASES: AtomicU64 = AtomicU64::new(0);

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
    /// Modifier key-ups emitted (always the full set: the modifier sweep is
    /// unconditional, which is safe because a key-up of an up key is inert).
    pub modifier_releases_emitted: usize,
    /// Mouse button-ups emitted. CONDITIONAL: one per button with release
    /// evidence (OS reports it down, or the process mirror holds it). `0` on a
    /// clean sweep — a bare button-up is a phantom click
    /// (`WM_RBUTTONUP` → `WM_CONTEXTMENU`), never inert.
    pub button_releases_emitted: usize,
    /// `SendInput` batches that did not fully insert even after retry.
    pub emission_failures: usize,
    /// Whether the lone-Alt/Win dummy-key mask was injected.
    pub masked_lone_modifier_tap: bool,
    /// Whether the full virtual-key-space scan ran (startup only).
    ///
    /// When `false`, a `..._SWEEP_CLEAN` verdict means only "no modifier and no
    /// mouse button was down"; it is **not** a statement about the rest of the
    /// keyboard. When `true`, the verdict covers every scannable virtual key.
    pub scanned_full_virtual_key_space: bool,
    /// Bit `n` set means virtual key `n` read down during the full scan.
    ///
    /// A 256-bit map rather than a `Vec`, so the report stays `Copy` and the
    /// whole sweep stays allocation-free.
    pub scanned_keys_found_down: VirtualKeyBitmap,
    /// Population count of [`Self::scanned_keys_found_down`].
    pub scanned_keys_found_down_count: usize,
    /// Key-ups emitted by the full scan (one per key found down — unlike the
    /// modifier sweep, the scan is *conditional*, so it never touches a key the
    /// OS did not report as physically held).
    pub scanned_key_releases_emitted: usize,
}

impl SyntheticReleaseReport {
    /// True when the OS reported synthetic state still down at sweep entry.
    ///
    /// At daemon startup this is the signature of an **inherited strand** from a
    /// previous unclean generation, and must be logged loudly rather than
    /// silently cleared.
    #[must_use]
    pub const fn found_anything_down(&self) -> bool {
        self.modifiers_found_down != 0
            || self.buttons_found_down != 0
            || self.scanned_keys_found_down_count != 0
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

    /// Comma-separated `name(0xNN)` list of the virtual keys the full scan found
    /// down, in ascending virtual-key order.
    ///
    /// Allocates, so it is called only from the logging path *after* the release
    /// has already been emitted — never from inside a sweep.
    #[must_use]
    pub fn scanned_keys_found_down_labels(&self) -> String {
        let mut out = String::new();
        for vkey in SCAN_FIRST_VKEY..=SCAN_LAST_VKEY {
            if !virtual_key_bit(&self.scanned_keys_found_down, vkey) {
                continue;
            }
            if !out.is_empty() {
                out.push(',');
            }
            match virtual_key_label(vkey) {
                Some(name) => {
                    out.push_str(name);
                    let _ = write!(out, "(0x{vkey:02x})");
                }
                None => {
                    let _ = write!(out, "vk(0x{vkey:02x})");
                }
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

/// A 256-bit set of virtual keys, one bit per code.
///
/// Fixed size and `Copy`, so [`SyntheticReleaseReport`] can carry the full scan
/// result without allocating.
pub type VirtualKeyBitmap = [u64; 4];

/// First virtual key the startup scan reads: `VK_BACK`.
///
/// Everything below it is either a mouse button (`VK_LBUTTON` `0x01` ..
/// `VK_XBUTTON2` `0x06`, released by the button half of the sweep and *not* a
/// keyboard key) or undefined (`0x00`, `0x07`).
const SCAN_FIRST_VKEY: u16 = 0x08;

/// Last virtual key the startup scan reads: `VK_OEM_CLEAR`.
///
/// `0xff` is excluded because it is not a key: Microsoft documents it as
/// reserved, and this module already uses it as [`DUMMY_MASK_VKEY`].
const SCAN_LAST_VKEY: u16 = 0xfe;

/// Virtual keys the startup scan must **not** read or release.
///
/// The scan is otherwise `SCAN_FIRST_VKEY..=SCAN_LAST_VKEY` and is
/// *conditional*: it emits a key-up only for a code `GetAsyncKeyState` reports
/// physically down. Two independent reasons put a code on this list.
///
/// ## 1. It is not a key — reading or releasing it is meaningless or harmful
///
/// * `0x15..=0x1a` (`VK_KANA`/`VK_HANGUL`, `VK_IME_ON`, `VK_JUNJA`, `VK_FINAL`,
///   `VK_HANJA`/`VK_KANJI`, `VK_IME_OFF`) and `0x1c..=0x1f` (`VK_CONVERT`,
///   `VK_NONCONVERT`, `VK_ACCEPT`, `VK_MODECHANGE`) are **IME state-machine
///   inputs**, not keys that can latch the keyboard down. Injecting a key-up
///   for one clears no strand and pokes a live IME.
/// * `0xe5` `VK_PROCESSKEY` is the sentinel an IME sets on a message it has
///   already consumed; there is no physical key behind it.
/// * `0xe7` `VK_PACKET` is the marker `KEYEVENTF_UNICODE` uses to carry a UTF-16
///   unit in `wScan`. Synthesizing a bare `VK_PACKET` key-up with `wScan = 0`
///   would inject a NUL **character** into the foreground window — the sweep
///   would become the thing that types.
/// * `0x5f` `VK_SLEEP` is a power-management key. No stranded-down failure mode
///   exists for it (the system acts on the down transition), so the sweep has
///   nothing to gain and a suspend to lose.
///
/// ## 2. Microsoft documents it as reserved / unassigned
///
/// `0x0a..=0x0b`, `0x0e..=0x0f`, `0x3a..=0x40`, `0x5e`, `0x88..=0x8f`,
/// `0x97..=0x9f`, `0xb8..=0xb9`, `0xc1..=0xda`, `0xe0`, `0xe8`. No layout maps
/// them, so no press can have set them and no key-up can clear anything.
///
/// ## Deliberately **not** excluded
///
/// * `0x14` `VK_CAPITAL`, `0x90` `VK_NUMLOCK`, `0x91` `VK_SCROLL` — the toggle
///   flips on the **down** transition, so a synthesized key-up cannot change the
///   toggle state, and a physically held toggle key is a real strand.
/// * The OEM block (`0xdb..=0xe4`, `0xe6`, `0xe9..=0xf5`) — these are real
///   printable/OEM keys on many layouts; a stranded `[` is a real strand.
/// * Media / browser / volume keys (`0xa6..=0xb7`, `0xfa`) — their action fires
///   on the down transition, so a key-up is inert.
///
/// The modifier codes are excluded here only because [`MODIFIER_SWEEP`] already
/// releases every one of them **unconditionally** in the same sweep; scanning
/// them again would emit a duplicate key-up and would bypass the lone-Alt/Win
/// dummy-key mask.
#[cfg_attr(not(windows), allow(dead_code))]
fn excluded_from_startup_scan(vkey: u16) -> bool {
    matches!(
        vkey,
        // Not a key at all, or unsafe to synthesize a key-up for.
        0x15..=0x1a | 0x1c..=0x1f | 0x5f | 0xe5 | 0xe7
        // Documented reserved / unassigned.
        | 0x0a..=0x0b | 0x0e..=0x0f | 0x3a..=0x40 | 0x5e | 0x88..=0x8f
        | 0x97..=0x9f | 0xb8..=0xb9 | 0xc1..=0xda | 0xe0 | 0xe8
    ) || is_modifier_sweep_vkey(vkey)
}

/// Whether [`MODIFIER_SWEEP`] already covers `vkey` unconditionally.
fn is_modifier_sweep_vkey(vkey: u16) -> bool {
    MODIFIER_SWEEP
        .iter()
        .any(|(modifier, _label)| *modifier == vkey)
}

fn virtual_key_bit(map: &VirtualKeyBitmap, vkey: u16) -> bool {
    let index = usize::from(vkey >> 6);
    let bit = u32::from(vkey & 0x3f);
    map.get(index)
        .is_some_and(|word| word & (1_u64 << bit) != 0)
}

#[cfg_attr(not(windows), allow(dead_code))]
fn set_virtual_key_bit(map: &mut VirtualKeyBitmap, vkey: u16) {
    let index = usize::from(vkey >> 6);
    let bit = u32::from(vkey & 0x3f);
    if let Some(word) = map.get_mut(index) {
        *word |= 1_u64 << bit;
    }
}

/// Human-readable name for the virtual keys a strand report is likely to name.
///
/// Unknown codes are logged as their hex value rather than guessed at: a wrong
/// name in an operator-safety log is worse than a raw code.
const fn virtual_key_label(vkey: u16) -> Option<&'static str> {
    Some(match vkey {
        0x08 => "backspace",
        0x09 => "tab",
        0x0c => "clear",
        0x0d => "enter",
        0x13 => "pause",
        0x14 => "capslock",
        0x1b => "escape",
        0x20 => "space",
        0x21 => "pageup",
        0x22 => "pagedown",
        0x23 => "end",
        0x24 => "home",
        0x25 => "left",
        0x26 => "up",
        0x27 => "right",
        0x28 => "down",
        0x2c => "printscreen",
        0x2d => "insert",
        0x2e => "delete",
        0x30..=0x39 => "digit",
        0x41..=0x5a => "letter",
        0x5d => "apps",
        0x60..=0x69 => "numpad_digit",
        0x6a => "numpad_multiply",
        0x6b => "numpad_add",
        0x6d => "numpad_subtract",
        0x6e => "numpad_decimal",
        0x6f => "numpad_divide",
        0x70 => "f1",
        0x71 => "f2",
        0x72 => "f3",
        0x73 => "f4",
        0x74 => "f5",
        0x75 => "f6",
        0x76 => "f7",
        0x77 => "f8",
        0x78 => "f9",
        0x79 => "f10",
        0x7a => "f11",
        0x7b => "f12",
        0x7c => "f13",
        0x7d => "f14",
        0x7e => "f15",
        0x7f => "f16",
        0x80..=0x87 => "f17_f24",
        0x90 => "numlock",
        0x91 => "scrolllock",
        0xa6..=0xb7 => "browser_media_volume",
        0xdb..=0xe4 | 0xe6 | 0xe9..=0xf5 => "oem",
        _ => return None,
    })
}

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
        SCAN_FIRST_VKEY, SCAN_LAST_VKEY, STRAND_SINCE_MS, STRAND_SLOTS, STRAND_TOKENS,
        SyntheticReleaseReport, TAG_SCANCODE, TAG_UNICODE, TAG_VIRTUAL_KEY, TOKEN_EMPTY,
        VirtualKeyBitmap, button_from_index, button_index, excluded_from_startup_scan,
        set_virtual_key_bit, token_tag, token_value, virtual_key_bit,
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

        // 4. Every modifier - but ONLY on evidence that there is something to
        //    release. Operator-interference incident, 2026-08-30.
        //
        //    Step 5 below justifies an unconditional sweep with "a key-up for a
        //    key that is already up is inert". That is not true for a LONE Alt
        //    or Win key-up: Windows turns it into an Alt tap (menu-bar focus
        //    steal) or a Win tap (Start menu). Step 3 is the mask that makes it
        //    inert - and step 3 only arms when `alt_or_win_down`. So in exactly
        //    the case where nothing is down, this loop emitted unmasked lone
        //    Alt and Win key-ups into the global input queue.
        //
        //    That is what the operator felt: the daemon OOM-panicked in a
        //    restart loop, and every panic ran this sweep with
        //    modifiers_found_down=0 and tracked_strands_released=0, emitting 11
        //    modifier key-ups - two of them unmasked Win taps - each time.
        //
        //    Releasing what this process cannot prove it holds is interference,
        //    not recovery: the same rule the startup sweep already applies.
        let has_release_evidence =
            report.modifiers_found_down != 0 || report.tracked_strands_released != 0;
        if has_release_evidence {
            for (vkey, _label) in &MODIFIER_SWEEP {
                if len < MAX_BATCH {
                    batch[len] = key_input(*vkey, 0, KEYEVENTF_KEYUP);
                    len += 1;
                    report.modifier_releases_emitted += 1;
                }
            }
        }

        // 5. Mouse buttons — CONDITIONAL, unlike the modifiers, and the
        //    asymmetry is load-bearing (operator-interference incident,
        //    2026-08-08). A key-up for a key that is already up is inert, so
        //    the modifier sweep can afford "release everything, always". A
        //    button-up is NOT inert: `DefWindowProc` turns a bare
        //    `WM_RBUTTONUP` into `WM_CONTEXTMENU`, so an unconditional
        //    `RBUTTON` release IS a right-click to whatever sits under the
        //    operator's cursor, and bare `XBUTTON` ups drive browser
        //    back/forward. The unconditional form meant every sweep — every
        //    daemon boot, every raw release_all fallback — phantom-right-
        //    clicked the operator's desktop (caught live by an LL mouse hook:
        //    bare injected RBUTTON_UP events, no downs, at each isolated
        //    daemon boot). A button-up is emitted only when there is evidence
        //    a release is owed: the OS reports the button physically down
        //    (step 1's snapshot — covers strands inherited from a dead
        //    generation, the case phase C/D proved), or this process's mirror
        //    says we pressed it (covers a mid-action sweep racing the OS
        //    readback). Neither evidence, no emission — there is nothing to
        //    release, and "release anyway" is the phantom click.
        for index in 0..BUTTON_SLOTS {
            if let Some(button) = button_from_index(index) {
                let mirror_held = BUTTON_SINCE_MS
                    .get(button_index(button))
                    .map(|since| since.swap(0, Ordering::AcqRel))
                    .is_some_and(|since| since != 0);
                let os_reports_down = report.buttons_found_down & (1 << index) != 0;
                if (os_reports_down || mirror_held) && len < MAX_BATCH {
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

    /// How many scan releases are staged before a flush.
    ///
    /// Bounded on purpose: the scan can find any number of keys down, but the
    /// stack frame that carries them must stay a fixed, small size. Each chunk
    /// is an independent batch of key-ups, and key-ups are order-independent and
    /// idempotent, so splitting the emission changes nothing observable.
    const SCAN_FLUSH_CHUNK: usize = 32;

    /// Reads `GetAsyncKeyState` across the whole scannable virtual-key space.
    ///
    /// Evidence only — nothing is emitted here — so the caller can take one
    /// consistent snapshot *before* any release lands.
    fn scan_virtual_key_space() -> (VirtualKeyBitmap, usize) {
        let mut found: VirtualKeyBitmap = [0; 4];
        let mut count = 0_usize;
        for vkey in SCAN_FIRST_VKEY..=SCAN_LAST_VKEY {
            if excluded_from_startup_scan(vkey) {
                continue;
            }
            if is_down(vkey) {
                set_virtual_key_bit(&mut found, vkey);
                count += 1;
            }
        }
        (found, count)
    }

    /// Emits one key-up per virtual key the scan found down, in chunks.
    fn release_scanned_keys(report: &mut SyntheticReleaseReport) {
        let mut batch: [INPUT; SCAN_FLUSH_CHUNK] =
            [const { key_input(0, 0, KEYEVENTF_KEYUP) }; SCAN_FLUSH_CHUNK];
        let mut len = 0_usize;
        for vkey in SCAN_FIRST_VKEY..=SCAN_LAST_VKEY {
            if !virtual_key_bit(&report.scanned_keys_found_down, vkey) {
                continue;
            }
            batch[len] = key_input(vkey, 0, KEYEVENTF_KEYUP);
            len += 1;
            report.scanned_key_releases_emitted += 1;
            if len == SCAN_FLUSH_CHUNK {
                if !send_release_batch(&batch[..len]) {
                    report.emission_failures += 1;
                }
                len = 0;
            }
        }
        if len > 0 && !send_release_batch(&batch[..len]) {
            report.emission_failures += 1;
        }
    }

    /// The bounded sweep **plus** a full virtual-key-space scan (#2082 A1).
    ///
    /// Order is deliberate: the scan's `GetAsyncKeyState` evidence is collected
    /// first, before [`sweep`] emits anything, so what the log reports as "found
    /// down" is one coherent snapshot of the moment the daemon booted rather
    /// than a reading taken partway through its own release batch.
    ///
    /// Only the startup path calls this. It carries its own stack frame and its
    /// own batch buffer, so the panic path's cost and frame size are unchanged.
    pub(super) fn sweep_full() -> SyntheticReleaseReport {
        let (scanned_keys_found_down, scanned_keys_found_down_count) = scan_virtual_key_space();
        let mut report = sweep();
        report.scanned_full_virtual_key_space = true;
        report.scanned_keys_found_down = scanned_keys_found_down;
        report.scanned_keys_found_down_count = scanned_keys_found_down_count;
        release_scanned_keys(&mut report);
        report
    }

    /// The same evidence [`sweep_full`] gathers, with **nothing emitted**.
    ///
    /// Used by the startup path when the durable ledger does not authorize a
    /// release. Every `*_releases_emitted` counter stays zero, and — unlike
    /// [`sweep`] — the process-local mirror is left untouched, because this
    /// function releases nothing and so must not tell the watchdog that anything
    /// was released. On a fresh boot that mirror is empty anyway; keeping the
    /// read non-destructive is what makes this safe to call from any generation.
    pub(super) fn scan_only() -> SyntheticReleaseReport {
        let mut report = SyntheticReleaseReport::default();
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
        let (scanned_keys_found_down, scanned_keys_found_down_count) = scan_virtual_key_space();
        report.scanned_full_virtual_key_space = true;
        report.scanned_keys_found_down = scanned_keys_found_down;
        report.scanned_keys_found_down_count = scanned_keys_found_down_count;
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

    pub(super) fn sweep_full() -> SyntheticReleaseReport {
        SyntheticReleaseReport::default()
    }

    pub(super) fn scan_only() -> SyntheticReleaseReport {
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

/// [`release_all_synthetic_input`] widened to the **whole virtual-key space**.
///
/// Adds one conditional `GetAsyncKeyState` read per scannable virtual key and a
/// key-up for each one found physically down. Use this where the process-local
/// mirror cannot possibly describe the strand — i.e. at daemon startup, where
/// the strand belongs to a previous generation.
///
/// # Why this is not the panic path
///
/// Two reasons, both structural rather than stylistic:
///
/// 1. **It would add nothing correct.** The panic hook runs inside the process
///    that owns [`STRAND_TOKENS`], and that mirror is authoritative for every
///    key this process pressed — including non-modifiers, which is exactly why
///    `key_down` hands its entry to the mirror. Everything the wider scan would
///    add is a key this process never pressed, i.e. a key the **operator** is
///    physically holding. Releasing those is a regression, not a fix, and the
///    panic path is the one most likely to run while a human is mid-keystroke.
/// 2. **It would widen a frame that must stay minimal.** The panic hook may run
///    on any thread, including one that panicked *because* it ran out of stack.
///    A full-space release needs ~250 additional `GetAsyncKeyState` syscalls and
///    a release buffer to stage them; the bounded sweep's frame is a single
///    fixed `[INPUT; 82]`. A panic hook that itself faults releases nothing at
///    all, so the cheapest sweep that is still complete is the correct one.
///
/// The startup path has neither constraint: it runs on the main thread, on a
/// healthy process, before either transport can accept a request.
#[must_use]
pub fn release_all_synthetic_input_full_scan() -> SyntheticReleaseReport {
    win32::sweep_full()
}

/// What the boot path knows, before it decides whether it may emit anything.
///
/// The startup sweep is the one release path with no authority of its own: the
/// process-local mirror is empty by construction, so nothing it can read tells
/// it *who* pressed a key that reads down. This type carries the two facts that
/// do, both established by the caller before the sweep runs.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct StartupEvidence {
    /// Keys the durable cross-generation ledger proved the previous generation
    /// was still holding ([`crate::recovery::ActionCrashRecoveryReport`]).
    pub ledger_recovered_keys: usize,
    /// Mouse buttons the same ledger proved were still held.
    pub ledger_recovered_buttons: usize,
    /// Set when this boot is one of a storm — see
    /// [`crate::recovery::boot_storm_verdict`]. A daemon that is crash-looping
    /// re-runs this path every few seconds, which is exactly the condition that
    /// turns a single stray release into sustained operator interference.
    pub boot_storm: bool,
}

impl StartupEvidence {
    /// Whether the boot path is allowed to synthesize releases at all.
    ///
    /// Emission requires positive proof that the previous generation died
    /// **holding** something. Absent that, every key and button the scan reports
    /// down belongs to the operator, and a key-up for it is not a harmless
    /// no-op: it stops auto-repeat, delivers `WM_KEYUP`, and for a button
    /// becomes a real click under the operator's cursor.
    ///
    /// A boot storm withholds emission unconditionally. A strand that outlives
    /// one boot is a bounded, recoverable defect; a sweep that fires every few
    /// seconds against a live human is not.
    #[must_use]
    pub const fn authorizes_emission(&self) -> bool {
        !self.boot_storm && (self.ledger_recovered_keys > 0 || self.ledger_recovered_buttons > 0)
    }
}

/// Startup sweep (#2082 fix 2, re-gated 2026-08-25): release **only** what the
/// durable ledger proves this daemon stranded, and log exactly what was found
/// still down either way.
///
/// A daemon that starts after any unclean exit inherits whatever synthetic state
/// the previous generation stranded, and `SendInput` state does not die with the
/// process. `GetAsyncKeyState` at entry is the readback Microsoft's `SendInput`
/// remarks point at for that problem — but it answers "what is down", never "who
/// put it down", so on its own it cannot authorize an emission.
///
/// # Scope: the whole virtual-key space (#2082 finding A1)
///
/// When `evidence` does authorize emission, this path runs
/// [`release_all_synthetic_input_full_scan`], not the bounded sweep: reading
/// only the 11 modifiers and 5 mouse buttons once left a stranded `VK_F13` down
/// through boot while the log said `..._SWEEP_CLEAN`. `CLEAN` means the scan
/// covered every scannable virtual key and found none of them down.
///
/// # Withholding
///
/// When [`StartupEvidence::authorizes_emission`] is false the scan still runs
/// and still reports, but nothing is emitted. That is the 2026-08-25 fix: the
/// operator's physically-held keys are no longer collateral for a strand the
/// ledger says does not exist.
pub fn release_all_synthetic_input_on_startup(evidence: StartupEvidence) -> SyntheticReleaseReport {
    if !evidence.authorizes_emission() {
        let report = win32::scan_only();
        tracing::warn!(
            code = "SYNTHETIC_INPUT_STARTUP_SWEEP_WITHHELD",
            modifiers_found_down = %report.modifiers_found_down_labels(),
            buttons_found_down = %report.buttons_found_down_labels(),
            scanned_keys_found_down = %report.scanned_keys_found_down_labels(),
            scanned_keys_found_down_count = report.scanned_keys_found_down_count,
            scanned_full_virtual_key_space = report.scanned_full_virtual_key_space,
            ledger_recovered_keys = evidence.ledger_recovered_keys,
            ledger_recovered_buttons = evidence.ledger_recovered_buttons,
            boot_storm = evidence.boot_storm,
            modifier_releases_emitted = 0,
            button_releases_emitted = 0,
            scanned_key_releases_emitted = 0,
            source_of_truth = "durable action recovery ledger for ownership; GetAsyncKeyState for physical state",
            "readback=synthetic_input edge=startup no synthetic release was emitted: the durable ledger does not show this daemon holding anything (or this boot is part of a storm), so everything reading down belongs to the operator and releasing it would be interference, not recovery"
        );
        return report;
    }
    let report = release_all_synthetic_input_full_scan();
    if report.found_anything_down() {
        tracing::warn!(
            code = "SYNTHETIC_INPUT_STARTUP_STRAND_RELEASED",
            modifiers_found_down = %report.modifiers_found_down_labels(),
            buttons_found_down = %report.buttons_found_down_labels(),
            scanned_keys_found_down = %report.scanned_keys_found_down_labels(),
            scanned_keys_found_down_count = report.scanned_keys_found_down_count,
            scanned_key_releases_emitted = report.scanned_key_releases_emitted,
            scanned_full_virtual_key_space = report.scanned_full_virtual_key_space,
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
            scanned_full_virtual_key_space = report.scanned_full_virtual_key_space,
            scan_first_vkey = SCAN_FIRST_VKEY,
            scan_last_vkey = SCAN_LAST_VKEY,
            modifier_releases_emitted = report.modifier_releases_emitted,
            button_releases_emitted = report.button_releases_emitted,
            scanned_key_releases_emitted = report.scanned_key_releases_emitted,
            emission_failures = report.emission_failures,
            source_of_truth = "GetAsyncKeyState read before the startup SendInput release sweep",
            "readback=synthetic_input edge=startup no modifier, no mouse button and no scanned virtual key was down; the unconditional modifier release sweep ran anyway and no button-up was emitted (buttons release only on evidence — a bare button-up is a phantom click)"
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

/// Why a guard is being dropped while still armed (#2082 finding B1).
///
/// `act_stroke` is not on the public tool facade, so the drag/stroke guards
/// cannot be driven black-box through their own verb. Making the guard narrate
/// itself — armed, disarmed, and *why* it dropped armed — is what lets an FSV
/// prove the RAII path fired from the daemon log alone, from whatever public
/// verb does reach it.
fn guard_exit_reason(release_refusal_code: Option<&'static str>) -> &'static str {
    if release_refusal_code.is_some() {
        // The normal release was attempted and the emission layer said no —
        // a tripped foreground fence is the case #2057 cares about.
        "release_refused"
    } else if std::thread::panicking() {
        "unwind"
    } else {
        "scope_exit_without_release"
    }
}

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
    release_refusal_code: Option<&'static str>,
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
        Self::log_armed(token, stage, false);
        Self {
            token,
            stage,
            release_refusal_code: None,
        }
    }

    /// Adopts an already-mirrored key so a scoped body is unwind-safe.
    #[must_use]
    pub fn adopt(token: u32, stage: &'static str) -> Self {
        Self::log_armed(token, stage, true);
        Self {
            token,
            stage,
            release_refusal_code: None,
        }
    }

    fn log_armed(token: u32, stage: &'static str, adopted: bool) {
        if token == TOKEN_EMPTY {
            return;
        }
        tracing::info!(
            code = "SYNTHETIC_INPUT_GUARD_ARMED",
            kind = "key",
            stage,
            token,
            adopted,
            "a synthetic key press is now under a Drop obligation; every exit from this scope emits the matching key-up"
        );
    }

    /// Records that the *normal* release path refused or failed, so the guard's
    /// `Drop` can say which of the three exits it is taking.
    pub const fn note_release_failure(&mut self, code: &'static str) {
        self.release_refusal_code = Some(code);
    }

    /// The matching release has left the OS; stop guarding.
    pub fn disarm(&mut self) {
        if self.token != TOKEN_EMPTY {
            let token = self.token;
            clear_key_strand(token);
            self.token = TOKEN_EMPTY;
            tracing::info!(
                code = "SYNTHETIC_INPUT_GUARD_DISARMED",
                kind = "key",
                stage = self.stage,
                token,
                reason = "released",
                "the matching key-up left the OS; the Drop obligation is discharged"
            );
        }
    }

    /// Stops guarding but **keeps** the mirror entry, because the press is
    /// meant to outlive this scope (`act_key_down` released by a later
    /// `act_key_up`). The panic sweep, startup sweep and watchdog still see it.
    pub fn disarm_without_clearing_mirror(&mut self) {
        if self.token != TOKEN_EMPTY {
            let token = self.token;
            self.token = TOKEN_EMPTY;
            tracing::info!(
                code = "SYNTHETIC_INPUT_GUARD_DISARMED",
                kind = "key",
                stage = self.stage,
                token,
                reason = "handed_to_mirror",
                "the press is meant to outlive this scope; the strand mirror, not this guard, now carries the release obligation"
            );
        }
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
        let reason = guard_exit_reason(self.release_refusal_code);
        let released = release_token_now(token);
        clear_key_strand(token);
        self.token = TOKEN_EMPTY;
        if released {
            tracing::error!(
                code = "SYNTHETIC_INPUT_GUARD_RELEASED",
                kind = "key",
                stage = self.stage,
                token,
                reason,
                release_refusal_code = self.release_refusal_code,
                "a synthetic key press left its scope without a matching release; the RAII guard emitted the key-up"
            );
        } else {
            tracing::error!(
                code = "SYNTHETIC_INPUT_RELEASE_EMISSION_FAILED",
                phase = "key_guard_drop",
                stage = self.stage,
                token,
                reason,
                release_refusal_code = self.release_refusal_code,
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
    release_refusal_code: Option<&'static str>,
}

impl HeldButtonStrand {
    /// Arms a guard for `button` and mirrors it.
    #[must_use]
    pub fn arm(button: MouseButton, stage: &'static str) -> Self {
        register_button_strand(button);
        Self::log_armed(button, stage, false);
        Self {
            button: Some(button),
            stage,
            release_refusal_code: None,
        }
    }

    /// Adopts an already-mirrored button so a scoped body is unwind-safe.
    #[must_use]
    pub fn adopt(button: MouseButton, stage: &'static str) -> Self {
        Self::log_armed(button, stage, true);
        Self {
            button: Some(button),
            stage,
            release_refusal_code: None,
        }
    }

    fn log_armed(button: MouseButton, stage: &'static str, adopted: bool) {
        tracing::info!(
            code = "SYNTHETIC_INPUT_GUARD_ARMED",
            kind = "button",
            stage,
            button = button_label(button),
            adopted,
            "a synthetic mouse button is now under a Drop obligation; every exit from this scope emits the matching button-up"
        );
    }

    /// Records that the *normal* release path refused or failed, so the guard's
    /// `Drop` can say which of the three exits it is taking.
    ///
    /// The drag/stroke callers pass the [`crate::ActionError`] code of the failed
    /// button-up, which is how a foreground-fence refusal
    /// (`ACTION_FOREGROUND_LOST`) becomes distinguishable in the log from a plain
    /// unwind.
    pub const fn note_release_failure(&mut self, code: &'static str) {
        self.release_refusal_code = Some(code);
    }

    /// The matching release has left the OS; stop guarding.
    pub fn disarm(&mut self) {
        if let Some(button) = self.button.take() {
            clear_button_strand(button);
            tracing::info!(
                code = "SYNTHETIC_INPUT_GUARD_DISARMED",
                kind = "button",
                stage = self.stage,
                button = button_label(button),
                reason = "released",
                "the matching button-up left the OS; the Drop obligation is discharged"
            );
        }
    }

    /// Stops guarding but **keeps** the mirror entry, because the press is meant
    /// to outlive this scope (`act_mouse_button action=down` released by a later
    /// `action=up`). The panic sweep, startup sweep and watchdog still see it.
    pub fn disarm_without_clearing_mirror(&mut self) {
        if let Some(button) = self.button.take() {
            tracing::info!(
                code = "SYNTHETIC_INPUT_GUARD_DISARMED",
                kind = "button",
                stage = self.stage,
                button = button_label(button),
                reason = "handed_to_mirror",
                "the press is meant to outlive this scope; the strand mirror, not this guard, now carries the release obligation"
            );
        }
    }
}

impl Drop for HeldButtonStrand {
    fn drop(&mut self) {
        let Some(button) = self.button.take() else {
            return;
        };
        let reason = guard_exit_reason(self.release_refusal_code);
        let released = release_button_now(button);
        clear_button_strand(button);
        if released {
            tracing::error!(
                code = "SYNTHETIC_INPUT_GUARD_RELEASED",
                kind = "button",
                stage = self.stage,
                button = button_label(button),
                reason,
                release_refusal_code = self.release_refusal_code,
                "a synthetic mouse press left its scope without a matching release; the RAII guard emitted the button-up"
            );
        } else {
            tracing::error!(
                code = "SYNTHETIC_INPUT_RELEASE_EMISSION_FAILED",
                phase = "button_guard_drop",
                stage = self.stage,
                button = button_label(button),
                reason,
                release_refusal_code = self.release_refusal_code,
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

/// Externally readable state of the synthetic-input watchdog (#2082 finding E).
///
/// The FSV that tried to prove the watchdog fires could not: there is no
/// `act_key_down` verb, and every hold reachable from the public surface arms an
/// emission budget, which moves the bound from the 30 s no-budget track to the
/// 300 s budget track. Nothing in the daemon said which track was in force, so
/// the untestable branch was also unobservable. This makes both the **bound** and
/// the **tracked hold state** readable without any new action verb, so a future
/// FSV can assert the selection rule directly instead of trying to trip it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SyntheticHoldWatchdogStatus {
    /// Whether the watchdog thread is running in this process.
    pub started: bool,
    /// Watchdog poll interval.
    pub tick_ms: u64,
    /// Capacity of the strand mirror.
    pub strand_slots: usize,
    /// The bound in force **right now**, i.e. what a hold would be measured
    /// against if the watchdog ticked at this instant.
    pub bound_ms: u64,
    /// Which of the two tracks [`Self::bound_ms`] came from.
    pub emission_budget_armed: bool,
    /// The no-budget track: `HELD_KEY_MAX_DURATION_MS` + grace.
    pub no_budget_bound_ms: u64,
    /// The budget-armed track: `MAX_LEASE_TTL_MS` + grace.
    pub budget_armed_bound_ms: u64,
    /// Mirrored key strands currently held.
    pub tracked_key_holds: usize,
    /// Mirrored mouse buttons currently held.
    pub tracked_button_holds: usize,
    /// Age of the oldest tracked hold, in milliseconds; `0` when nothing is held.
    pub longest_hold_ms: u64,
    /// Presses that could not be mirrored because the mirror was full.
    pub strand_mirror_overflows: u64,
    /// Strands this generation's watchdog has force-released.
    pub force_releases: u64,
}

impl SyntheticHoldWatchdogStatus {
    /// Which of the two bound tracks is currently selected.
    #[must_use]
    pub const fn bound_track(&self) -> &'static str {
        if self.emission_budget_armed {
            "emission_budget"
        } else {
            "held_key_max_duration"
        }
    }

    /// A flat `key=value` rendering for the `action` health subsystem's `detail`
    /// string, which is already a space-separated `key=value` blob.
    #[must_use]
    pub fn label(&self) -> String {
        format!(
            "synthetic_watchdog_started={} synthetic_watchdog_tick_ms={} \
             synthetic_hold_bound_ms={} synthetic_hold_bound_track={} \
             synthetic_hold_bound_no_budget_ms={} synthetic_hold_bound_budget_armed_ms={} \
             synthetic_holds_tracked_keys={} synthetic_holds_tracked_buttons={} \
             synthetic_hold_longest_ms={} synthetic_strand_mirror_overflows={} \
             synthetic_watchdog_force_releases={}",
            self.started,
            self.tick_ms,
            self.bound_ms,
            self.bound_track(),
            self.no_budget_bound_ms,
            self.budget_armed_bound_ms,
            self.tracked_key_holds,
            self.tracked_button_holds,
            self.longest_hold_ms,
            self.strand_mirror_overflows,
            self.force_releases,
        )
    }
}

/// Reads the watchdog's configured bound and its current tracked-hold state.
///
/// Side-effect free: it loads the same atomics the watchdog reads, so calling it
/// from a health probe cannot perturb what it reports. The bound-selection rule
/// is [`crate::lease::synthetic_hold_watchdog_bound_ms`]'s — it is reproduced
/// here only so both tracks and the choice between them can be reported in one
/// consistent reading.
#[must_use]
pub fn synthetic_hold_watchdog_status() -> SyntheticHoldWatchdogStatus {
    let now = now_ms();
    let mut tracked_key_holds = 0_usize;
    let mut tracked_button_holds = 0_usize;
    let mut oldest = 0_u64;

    for (cell, since_cell) in STRAND_TOKENS.iter().zip(STRAND_SINCE_MS.iter()) {
        let since = since_cell.load(Ordering::Acquire);
        if cell.load(Ordering::Acquire) == TOKEN_EMPTY || since == 0 {
            continue;
        }
        tracked_key_holds += 1;
        oldest = oldest.max(now.saturating_sub(since));
    }
    for since_cell in &BUTTON_SINCE_MS {
        let since = since_cell.load(Ordering::Acquire);
        if since == 0 {
            continue;
        }
        tracked_button_holds += 1;
        oldest = oldest.max(now.saturating_sub(since));
    }

    let no_budget_bound_ms = crate::emitter::HELD_KEY_MAX_DURATION_MS
        .saturating_add(crate::lease::SYNTHETIC_HOLD_WATCHDOG_GRACE_MS);
    let budget_armed_bound_ms = crate::lease::MAX_LEASE_TTL_MS
        .saturating_add(crate::lease::SYNTHETIC_HOLD_WATCHDOG_GRACE_MS);
    // Read the budget exactly once and derive `bound_ms` from that same reading
    // rather than calling `synthetic_hold_watchdog_bound_ms()` separately: two
    // acquisitions could straddle an arm/disarm and publish a `bound_ms` that
    // contradicts the `emission_budget_armed` printed beside it. It also halves
    // this probe's traffic on a process-global lock.
    let emission_budget_armed = crate::lease::emission_budget_armed();
    let bound_ms = if emission_budget_armed {
        budget_armed_bound_ms
    } else {
        no_budget_bound_ms
    };

    SyntheticHoldWatchdogStatus {
        started: WATCHDOG_STARTED.load(Ordering::SeqCst),
        tick_ms: WATCHDOG_TICK_MS,
        strand_slots: STRAND_SLOTS,
        bound_ms,
        emission_budget_armed,
        no_budget_bound_ms,
        budget_armed_bound_ms,
        tracked_key_holds,
        tracked_button_holds,
        longest_hold_ms: oldest,
        strand_mirror_overflows: STRAND_OVERFLOWS.load(Ordering::Relaxed),
        force_releases: WATCHDOG_FORCE_RELEASES.load(Ordering::Relaxed),
    }
}

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
        WATCHDOG_FORCE_RELEASES.fetch_add(1, Ordering::Relaxed);
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
        WATCHDOG_FORCE_RELEASES.fetch_add(1, Ordering::Relaxed);
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
            // The bound is not a constant — it is selected per tick from whether
            // an emission budget is armed. Logging both tracks and the one in
            // force at boot is what makes the 30 s vs 300 s selection auditable
            // from the daemon log alone (#2082 finding E).
            let status = synthetic_hold_watchdog_status();
            tracing::info!(
                code = "SYNTHETIC_INPUT_WATCHDOG_STARTED",
                tick_ms = WATCHDOG_TICK_MS,
                strand_slots = STRAND_SLOTS,
                bound_ms_at_start = status.bound_ms,
                bound_track_at_start = status.bound_track(),
                no_budget_bound_ms = status.no_budget_bound_ms,
                budget_armed_bound_ms = status.budget_armed_bound_ms,
                emission_budget_armed_at_start = status.emission_budget_armed,
                bound_selection = "no emission budget armed -> HELD_KEY_MAX_DURATION_MS + grace; budget armed -> MAX_LEASE_TTL_MS + grace",
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
