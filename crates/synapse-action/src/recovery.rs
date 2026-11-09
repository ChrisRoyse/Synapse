use std::{
    env, fs,
    fs::OpenOptions,
    io::Write,
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
};

use serde::{Deserialize, Serialize};
use synapse_core::{
    Action, Backend, ButtonAction, GamepadController, GamepadReport, Key, MouseButton, PadId,
};

use crate::{
    ActionBackend, ActionError, ActionResult, EmitState, VigemBackend,
    backend::software::SoftwareBackend,
};

const RECOVERY_FILE_ENV: &str = "SYNAPSE_ACTION_RECOVERY_FILE";
const DB_ENV: &str = "SYNAPSE_DB";
const RECOVERY_FILE_NAME: &str = "action_recovery.jsonl";

static RECOVERY_LOCK: OnceLock<Mutex<Option<PathBuf>>> = OnceLock::new();

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActionCrashRecoveryReport {
    pub recovery_file: PathBuf,
    pub recovered_keys: usize,
    pub recovered_buttons: usize,
    pub recovered_pads: usize,
    pub ignored_trailing_bytes: bool,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct RecoveryLedger {
    keys: Vec<Key>,
    buttons: Vec<MouseButton>,
    pads: Vec<PadRecoveryEntry>,
    ignored_trailing_bytes: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PadRecoveryEntry {
    pad: PadId,
    controller: GamepadController,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
enum RecoveryEvent {
    KeyHeld {
        key: Key,
    },
    KeyReleased {
        key: Key,
    },
    ButtonHeld {
        button: MouseButton,
    },
    ButtonReleased {
        button: MouseButton,
    },
    PadHeld {
        pad: PadId,
        controller: GamepadController,
    },
    PadReleased {
        pad: PadId,
    },
}

#[must_use]
pub fn configured_crash_recovery_file() -> PathBuf {
    configured_path()
}

/// Configures the process-wide crash-recovery ledger path.
///
/// The explicit env var wins, then the daemon DB directory, then the user's
/// durable Synapse app-data directory.
///
/// # Errors
///
/// Returns `ACTION_BACKEND_UNAVAILABLE` if the global path lock is poisoned.
pub fn configure_crash_recovery_file(db_path: Option<&Path>) -> ActionResult<PathBuf> {
    let path = derive_recovery_file_path(db_path);
    let lock = recovery_lock();
    let mut guard = lock
        .lock()
        .map_err(|_error| ActionError::BackendUnavailable {
            detail: "action crash recovery path lock poisoned".to_owned(),
        })?;
    *guard = Some(path.clone());
    drop(guard);
    Ok(path)
}

/// Releases any inputs recorded by a previous daemon before accepting new MCP
/// calls.
///
/// # Errors
///
/// Returns `ACTION_BACKEND_UNAVAILABLE` if the ledger cannot be read or if any
/// stale input cannot be released.
pub fn recover_stale_inputs_from_configured_path() -> ActionResult<ActionCrashRecoveryReport> {
    let path = configured_path();
    recover_stale_inputs_at(&path)
}

pub(crate) fn record_held_key(key: &Key) -> ActionResult<()> {
    append_recovery_event(&RecoveryEvent::KeyHeld { key: key.clone() })
}

pub(crate) fn clear_held_key(key: &Key) -> ActionResult<()> {
    append_recovery_event(&RecoveryEvent::KeyReleased { key: key.clone() })
}

pub(crate) fn record_held_button(button: MouseButton) -> ActionResult<()> {
    append_recovery_event(&RecoveryEvent::ButtonHeld { button })
}

pub(crate) fn clear_held_button(button: MouseButton) -> ActionResult<()> {
    append_recovery_event(&RecoveryEvent::ButtonReleased { button })
}

pub(crate) fn record_held_pad_report(pad: PadId, report: &GamepadReport) -> ActionResult<()> {
    if is_neutral_report(report) {
        return clear_held_pad(pad);
    }
    append_recovery_event(&RecoveryEvent::PadHeld {
        pad,
        controller: report.controller,
    })
}

pub(crate) fn clear_held_pad(pad: PadId) -> ActionResult<()> {
    append_recovery_event(&RecoveryEvent::PadReleased { pad })
}

fn recovery_lock() -> &'static Mutex<Option<PathBuf>> {
    RECOVERY_LOCK.get_or_init(|| Mutex::new(None))
}

fn configured_path() -> PathBuf {
    let lock = recovery_lock();
    match lock.lock() {
        Ok(guard) => guard
            .clone()
            .unwrap_or_else(|| derive_recovery_file_path(None)),
        Err(_error) => derive_recovery_file_path(None),
    }
}

fn derive_recovery_file_path(db_path: Option<&Path>) -> PathBuf {
    if let Some(path) = env::var_os(RECOVERY_FILE_ENV).map(PathBuf::from) {
        return path;
    }
    if let Some(path) = db_path {
        return path.join(RECOVERY_FILE_NAME);
    }
    if let Some(path) = env::var_os(DB_ENV).map(PathBuf::from) {
        return path.join(RECOVERY_FILE_NAME);
    }
    env::var_os("LOCALAPPDATA")
        .map_or_else(env::temp_dir, PathBuf::from)
        .join("synapse")
        .join(RECOVERY_FILE_NAME)
}

fn append_recovery_event(event: &RecoveryEvent) -> ActionResult<()> {
    let path = configured_path();
    append_recovery_event_at(&path, event)
}

fn append_recovery_event_at(path: &Path, event: &RecoveryEvent) -> ActionResult<()> {
    let lock = recovery_lock();
    let _guard = lock
        .lock()
        .map_err(|_error| ActionError::BackendUnavailable {
            detail: "action crash recovery ledger lock poisoned".to_owned(),
        })?;
    ensure_parent_dir(path)?;
    let before = read_ledger_from_log(path)?;
    if !before.ignored_trailing_bytes {
        let mut after = before.clone();
        after.apply(event.clone());
        if after == before {
            return Ok(());
        }
    }
    let mut encoded =
        serde_json::to_vec(event).map_err(|error| ActionError::BackendUnavailable {
            detail: format!("encode action crash recovery event failed: {error}"),
        })?;
    encoded.push(b'\n');
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|error| ActionError::BackendUnavailable {
            detail: format!(
                "open action crash recovery ledger {} failed: {error}",
                path.display()
            ),
        })?;
    file.write_all(&encoded)
        .and_then(|()| file.sync_data())
        .map_err(|error| ActionError::BackendUnavailable {
            detail: format!(
                "write action crash recovery ledger {} failed: {error}",
                path.display()
            ),
        })?;
    drop(file);

    let ledger = read_ledger_from_log(path)?;
    if ledger.is_empty() {
        remove_recovery_file(path)?;
    }
    Ok(())
}

fn recover_stale_inputs_at(path: &Path) -> ActionResult<ActionCrashRecoveryReport> {
    let ledger = read_ledger_from_log(path)?;
    let recovered_keys = ledger.keys.len();
    let recovered_buttons = ledger.buttons.len();
    let recovered_pads = ledger.pads.len();
    if ledger.is_empty() {
        tracing::info!(
            code = "ACTION_CRASH_RECOVERY_READBACK",
            recovery_file = %path.display(),
            recovered_keys,
            recovered_buttons,
            recovered_pads,
            ignored_trailing_bytes = ledger.ignored_trailing_bytes,
            "readback=action_crash_recovery edge=startup after=no_stale_inputs"
        );
        return Ok(ActionCrashRecoveryReport {
            recovery_file: path.to_path_buf(),
            recovered_keys,
            recovered_buttons,
            recovered_pads,
            ignored_trailing_bytes: ledger.ignored_trailing_bytes,
        });
    }

    let mut errors = Vec::new();
    let software = SoftwareBackend::new();
    let vigem = VigemBackend::new();
    let mut state = EmitState::new();

    for key in ledger.keys.iter().rev() {
        let result = software.execute(
            &Action::KeyUp {
                key: key.clone(),
                backend: Backend::Software,
            },
            &mut state,
        );
        if let Err(error) = result {
            errors.push(format!("key {key:?}: {error}"));
        }
    }
    for button in ledger.buttons.iter().rev() {
        let result = software.execute(
            &Action::MouseButton {
                button: *button,
                action: ButtonAction::Up,
                hold_ms: 0,
                backend: Backend::Software,
            },
            &mut state,
        );
        if let Err(error) = result {
            errors.push(format!("button {button:?}: {error}"));
        }
    }
    for pad in &ledger.pads {
        let result = vigem.execute(
            &Action::PadReport {
                pad: pad.pad,
                report: GamepadReport::neutral(pad.controller),
            },
            &mut state,
        );
        if let Err(error) = result {
            errors.push(format!("pad {}: {error}", pad.pad));
        }
    }

    if !errors.is_empty() {
        return Err(ActionError::BackendUnavailable {
            detail: format!(
                "action crash recovery failed for {} stale inputs in {}: {}",
                errors.len(),
                path.display(),
                errors.join("; ")
            ),
        });
    }

    remove_recovery_file(path)?;
    tracing::warn!(
        code = "ACTION_CRASH_RECOVERY_READBACK",
        recovery_file = %path.display(),
        recovered_keys,
        recovered_buttons,
        recovered_pads,
        ignored_trailing_bytes = ledger.ignored_trailing_bytes,
        "readback=action_crash_recovery edge=startup after=stale_inputs_released"
    );
    Ok(ActionCrashRecoveryReport {
        recovery_file: path.to_path_buf(),
        recovered_keys,
        recovered_buttons,
        recovered_pads,
        ignored_trailing_bytes: ledger.ignored_trailing_bytes,
    })
}

fn read_ledger_from_log(path: &Path) -> ActionResult<RecoveryLedger> {
    let Ok(bytes) = fs::read(path) else {
        return Ok(RecoveryLedger::default());
    };
    let content = String::from_utf8(bytes).map_err(|error| ActionError::BackendUnavailable {
        detail: format!(
            "action crash recovery ledger {} is not UTF-8: {error}",
            path.display()
        ),
    })?;
    let mut ledger = RecoveryLedger::default();
    for raw in content.split_inclusive('\n') {
        if !raw.ends_with('\n') {
            ledger.ignored_trailing_bytes |= !raw.trim().is_empty();
            continue;
        }
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        let event = serde_json::from_str::<RecoveryEvent>(line).map_err(|error| {
            ActionError::BackendUnavailable {
                detail: format!(
                    "decode action crash recovery ledger {} failed: {error}; line={line:?}",
                    path.display()
                ),
            }
        })?;
        ledger.apply(event);
    }
    Ok(ledger)
}

fn ensure_parent_dir(path: &Path) -> ActionResult<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| ActionError::BackendUnavailable {
            detail: format!(
                "create action crash recovery ledger directory {} failed: {error}",
                parent.display()
            ),
        })?;
    }
    Ok(())
}

fn remove_recovery_file(path: &Path) -> ActionResult<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(ActionError::BackendUnavailable {
            detail: format!(
                "remove action crash recovery ledger {} failed: {error}",
                path.display()
            ),
        }),
    }
}

impl RecoveryLedger {
    fn apply(&mut self, event: RecoveryEvent) {
        match event {
            RecoveryEvent::KeyHeld { key } => push_unique(&mut self.keys, key),
            RecoveryEvent::KeyReleased { key } => self.keys.retain(|held| held != &key),
            RecoveryEvent::ButtonHeld { button } => push_unique(&mut self.buttons, button),
            RecoveryEvent::ButtonReleased { button } => {
                self.buttons.retain(|held| *held != button);
            }
            RecoveryEvent::PadHeld { pad, controller } => {
                self.pads.retain(|held| held.pad != pad);
                self.pads.push(PadRecoveryEntry { pad, controller });
            }
            RecoveryEvent::PadReleased { pad } => {
                self.pads.retain(|held| held.pad != pad);
            }
        }
    }

    const fn is_empty(&self) -> bool {
        self.keys.is_empty() && self.buttons.is_empty() && self.pads.is_empty()
    }
}

fn push_unique<T: PartialEq>(items: &mut Vec<T>, item: T) {
    if !items.contains(&item) {
        items.push(item);
    }
}

fn is_neutral_report(report: &GamepadReport) -> bool {
    report.buttons.is_empty()
        && report.thumb_l == (0.0, 0.0)
        && report.thumb_r == (0.0, 0.0)
        && report.lt == 0.0
        && report.rt == 0.0
}
