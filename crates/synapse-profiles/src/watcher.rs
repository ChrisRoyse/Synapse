#![allow(clippy::missing_errors_doc)]

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::{Arc, RwLock, mpsc},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use synapse_core::{
    PerceptionMode, Profile, ProfileBackends, ProfileId, ProfileMatch, ProfileUseScope,
};
use tracing::{instrument, warn};

use crate::{
    error::{ProfileError, ProfileLoadError},
    parser::{LoadedProfile, ScreenBounds, parse_profile_file_with_bounds},
    resolver::{ForegroundWindow, ProfileMatchResolution, resolve_active_profile},
};

const WATCH_DEBOUNCE: Duration = Duration::from_millis(200);
const PROFILES_ACTIVE_METRIC: &str = "profiles_active";
const PROFILE_RELOADS_METRIC: &str = "profile_reloads_total";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProfileStatus {
    pub id: ProfileId,
    pub label: String,
    pub use_scope: ProfileUseScope,
    pub mode: PerceptionMode,
    pub detection_model_id: Option<String>,
    pub detection_classes: Vec<String>,
    pub hud_fields: Vec<String>,
    pub keymap_actions: Vec<String>,
    pub backends: ProfileBackends,
    pub event_extensions: Vec<ProfileEventExtensionStatus>,
    pub active: bool,
    pub schema_version: u32,
    pub matches: Vec<ProfileMatch>,
    pub metadata: BTreeMap<String, String>,
    pub source_path: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProfileEventExtensionStatus {
    pub name: String,
    pub emits_kind: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ForegroundProfileTransition {
    pub previous_profile_id: Option<ProfileId>,
    pub active_profile_id: Option<ProfileId>,
    pub previous_scope: Option<ProfileUseScope>,
    pub active_scope: Option<ProfileUseScope>,
    pub effective_previous_scope: ProfileUseScope,
    pub effective_active_scope: ProfileUseScope,
    pub resolution: Option<ProfileMatchResolution>,
    pub changed: bool,
    pub scope_changed: bool,
}

#[derive(Debug, Default)]
struct ProfileState {
    profiles: BTreeMap<ProfileId, LoadedProfile>,
    active_profile_id: Option<ProfileId>,
    last_errors: Vec<ProfileLoadError>,
    last_reload_at: Option<SystemTime>,
}

#[derive(Debug)]
pub struct ProfileRuntime {
    profile_dir: PathBuf,
    bounds: ScreenBounds,
    state: Arc<RwLock<ProfileState>>,
    _watcher: RecommendedWatcher,
}

impl ProfileRuntime {
    #[instrument(skip_all, fields(profile_dir = %profile_dir.as_ref().display()))]
    pub fn spawn(profile_dir: impl AsRef<Path>) -> Result<Self, ProfileError> {
        Self::spawn_with_screen_bounds(profile_dir, ScreenBounds::default())
    }

    #[instrument(skip_all, fields(profile_dir = %profile_dir.as_ref().display(), screen_width = bounds.width, screen_height = bounds.height))]
    pub fn spawn_with_screen_bounds(
        profile_dir: impl AsRef<Path>,
        bounds: ScreenBounds,
    ) -> Result<Self, ProfileError> {
        let profile_dir = profile_dir.as_ref().to_path_buf();
        fs::create_dir_all(&profile_dir).map_err(|source| ProfileError::Io {
            path: profile_dir.clone(),
            source,
        })?;

        let state = Arc::new(RwLock::new(ProfileState::default()));
        refresh_state(&profile_dir, bounds, &state)?;

        let (tx, rx) = mpsc::channel();
        let mut watcher = notify::recommended_watcher(move |event| {
            let _ = tx.send(event);
        })
        .map_err(|source| ProfileError::Watch {
            path: profile_dir.clone(),
            message: source.to_string(),
        })?;
        watcher
            .watch(&profile_dir, RecursiveMode::NonRecursive)
            .map_err(|source| ProfileError::Watch {
                path: profile_dir.clone(),
                message: source.to_string(),
            })?;

        let runtime = Self {
            profile_dir: profile_dir.clone(),
            bounds,
            state: Arc::clone(&state),
            _watcher: watcher,
        };

        thread::Builder::new()
            .name("synapse-profile-watch".to_owned())
            .spawn(move || loop {
                match rx.recv() {
                    Ok(Ok(_event)) => {
                        thread::sleep(WATCH_DEBOUNCE);
                        while rx.try_recv().is_ok() {}
                        if let Err(error) = refresh_state(&profile_dir, bounds, &state) {
                            warn!(code = error.code(), error = %error, "profile refresh failed");
                        }
                    }
                    Ok(Err(error)) => {
                        warn!(
                            code = synapse_core::error_codes::PROFILE_PARSE_ERROR,
                            error = %error,
                            "profile watcher event failed"
                        );
                    }
                    Err(_) => break,
                }
            })
            .map_err(|source| ProfileError::Io {
                path: runtime.profile_dir.clone(),
                source,
            })?;

        Ok(runtime)
    }

    #[must_use]
    #[instrument(skip_all)]
    pub fn profile_dir(&self) -> &Path {
        &self.profile_dir
    }

    #[instrument(skip_all, fields(profile_dir = %self.profile_dir.display()))]
    pub fn refresh(&self) -> Result<(), ProfileError> {
        refresh_state(&self.profile_dir, self.bounds, &self.state)
    }

    #[instrument(skip_all, fields(profile_id = profile_id))]
    pub fn activate(&self, profile_id: &str) -> Result<(), ProfileError> {
        let mut state = self
            .state
            .write()
            .map_err(|_| ProfileError::StatePoisoned)?;
        if !state.profiles.contains_key(profile_id) {
            return Err(ProfileError::NotFound {
                profile_id: profile_id.to_owned(),
            });
        }
        let previous_profile_id = state.active_profile_id.clone();
        state.active_profile_id = Some(profile_id.to_owned());
        drop(state);
        if previous_profile_id.as_deref() != Some(profile_id)
            && let Some(previous_profile_id) = previous_profile_id
        {
            metrics::gauge!(PROFILES_ACTIVE_METRIC, "profile_id" => previous_profile_id).set(0.0);
        }
        metrics::gauge!(PROFILES_ACTIVE_METRIC, "profile_id" => profile_id.to_owned()).set(1.0);
        tracing::info!(code = "PROFILE_ACTIVATED", profile_id, "profile activated");
        Ok(())
    }

    #[instrument(skip_all)]
    pub fn list(&self, include_inactive: bool) -> Result<Vec<ProfileStatus>, ProfileError> {
        let state = self.state.read().map_err(|_| ProfileError::StatePoisoned)?;
        Ok(profile_statuses(&state, include_inactive))
    }

    #[instrument(skip_all, fields(profile_id = profile_id))]
    pub fn profile(&self, profile_id: &str) -> Result<Option<Profile>, ProfileError> {
        let state = self.state.read().map_err(|_| ProfileError::StatePoisoned)?;
        Ok(state
            .profiles
            .get(profile_id)
            .map(|loaded| loaded.profile.clone()))
    }

    #[instrument(skip_all)]
    pub fn loaded_profiles(&self) -> Result<Vec<LoadedProfile>, ProfileError> {
        let state = self.state.read().map_err(|_| ProfileError::StatePoisoned)?;
        Ok(state.profiles.values().cloned().collect())
    }

    #[instrument(skip_all)]
    pub fn active_profile_id(&self) -> Result<Option<ProfileId>, ProfileError> {
        let state = self.state.read().map_err(|_| ProfileError::StatePoisoned)?;
        Ok(state.active_profile_id.clone())
    }

    #[instrument(skip_all)]
    pub fn last_errors(&self) -> Result<Vec<ProfileLoadError>, ProfileError> {
        let state = self.state.read().map_err(|_| ProfileError::StatePoisoned)?;
        Ok(state.last_errors.clone())
    }

    #[instrument(skip_all)]
    pub fn last_reload_at(&self) -> Result<Option<String>, ProfileError> {
        let state = self.state.read().map_err(|_| ProfileError::StatePoisoned)?;
        Ok(state.last_reload_at.and_then(system_time_epoch_ms))
    }

    #[instrument(skip_all)]
    pub fn resolve_foreground(
        &self,
        foreground: &ForegroundWindow,
    ) -> Result<Option<ProfileMatchResolution>, ProfileError> {
        let profiles = {
            let state = self.state.read().map_err(|_| ProfileError::StatePoisoned)?;
            state.profiles.values().cloned().collect::<Vec<_>>()
        };
        Ok(resolve_active_profile(&profiles, foreground))
    }

    #[instrument(skip_all)]
    pub fn activate_for_foreground(
        &self,
        foreground: &ForegroundWindow,
    ) -> Result<Option<ProfileMatchResolution>, ProfileError> {
        Ok(self.reevaluate_foreground(foreground)?.resolution)
    }

    #[instrument(skip_all)]
    pub fn reevaluate_foreground(
        &self,
        foreground: &ForegroundWindow,
    ) -> Result<ForegroundProfileTransition, ProfileError> {
        let mut state = self
            .state
            .write()
            .map_err(|_| ProfileError::StatePoisoned)?;
        let profiles = state.profiles.values().cloned().collect::<Vec<_>>();
        let previous_profile_id = state.active_profile_id.clone();
        let previous_scope = previous_profile_id
            .as_ref()
            .and_then(|profile_id| state.profiles.get(profile_id))
            .map(|loaded| loaded.profile.use_scope);
        let resolution = resolve_active_profile(&profiles, foreground);
        let (active_profile_id, active_scope) =
            resolution.as_ref().map_or((None, None), |resolution| {
                let active_scope = state
                    .profiles
                    .get(&resolution.profile_id)
                    .map(|loaded| loaded.profile.use_scope);
                (Some(resolution.profile_id.clone()), active_scope)
            });
        state.active_profile_id.clone_from(&active_profile_id);

        let effective_previous_scope = effective_scope(previous_scope);
        let effective_active_scope = effective_scope(active_scope);
        let changed = previous_profile_id != active_profile_id;
        let scope_changed = effective_previous_scope != effective_active_scope;
        drop(state);

        if changed {
            if let Some(active_profile_id) = &active_profile_id {
                if let Some(previous_profile_id) = &previous_profile_id {
                    metrics::gauge!(
                        PROFILES_ACTIVE_METRIC,
                        "profile_id" => previous_profile_id.clone()
                    )
                    .set(0.0);
                }
                metrics::gauge!(
                    PROFILES_ACTIVE_METRIC,
                    "profile_id" => active_profile_id.clone()
                )
                .set(1.0);
                tracing::info!(
                    code = "PROFILE_FOREGROUND_ACTIVATED",
                    previous_profile_id = ?previous_profile_id,
                    active_profile_id = %active_profile_id,
                    match_rank = ?resolution.as_ref().map(|resolution| resolution.rank_name),
                    "foreground profile activated"
                );
            } else {
                if let Some(previous_profile_id) = &previous_profile_id {
                    metrics::gauge!(
                        PROFILES_ACTIVE_METRIC,
                        "profile_id" => previous_profile_id.clone()
                    )
                    .set(0.0);
                }
                tracing::info!(
                    code = "PROFILE_FOREGROUND_CLEARED",
                    previous_profile_id = ?previous_profile_id,
                    "foreground profile cleared after unmatched foreground"
                );
            }
        }

        Ok(ForegroundProfileTransition {
            previous_profile_id,
            active_profile_id,
            previous_scope,
            active_scope,
            effective_previous_scope,
            effective_active_scope,
            resolution,
            changed,
            scope_changed,
        })
    }
}

const fn effective_scope(scope: Option<ProfileUseScope>) -> ProfileUseScope {
    match scope {
        Some(scope) => scope,
        None => ProfileUseScope::Unknown,
    }
}

fn profile_statuses(state: &ProfileState, include_inactive: bool) -> Vec<ProfileStatus> {
    state
        .profiles
        .values()
        .filter_map(|loaded| {
            let active = state
                .active_profile_id
                .as_ref()
                .is_some_and(|active_id| active_id == &loaded.profile.id);
            (active || include_inactive).then(|| ProfileStatus {
                id: loaded.profile.id.clone(),
                label: loaded.profile.label.clone(),
                use_scope: loaded.profile.use_scope,
                mode: loaded.profile.mode,
                detection_model_id: loaded.profile.detection.model_id.clone(),
                detection_classes: loaded.profile.detection.classes_of_interest.clone(),
                hud_fields: loaded
                    .profile
                    .hud
                    .iter()
                    .map(|field| field.name.clone())
                    .collect(),
                keymap_actions: loaded.profile.keymap.keys().cloned().collect(),
                backends: loaded.profile.backends,
                event_extensions: loaded
                    .profile
                    .event_extensions
                    .iter()
                    .map(|extension| ProfileEventExtensionStatus {
                        name: extension.name.clone(),
                        emits_kind: extension.emits_kind.clone(),
                    })
                    .collect(),
                active,
                schema_version: loaded.schema_version,
                matches: loaded.profile.matches.clone(),
                metadata: loaded.profile.metadata.clone(),
                source_path: loaded.source_path.clone(),
            })
        })
        .collect()
}

fn refresh_state(
    profile_dir: &Path,
    bounds: ScreenBounds,
    state: &Arc<RwLock<ProfileState>>,
) -> Result<(), ProfileError> {
    let previous = {
        let guard = state.read().map_err(|_| ProfileError::StatePoisoned)?;
        guard.profiles.clone()
    };
    let (profiles, errors) = load_dir(profile_dir, bounds, &previous)?;
    let mut guard = state.write().map_err(|_| ProfileError::StatePoisoned)?;
    guard.profiles = profiles;
    if guard
        .active_profile_id
        .as_ref()
        .is_some_and(|active_id| !guard.profiles.contains_key(active_id))
    {
        guard.active_profile_id = None;
    }
    guard.last_errors = errors;
    guard.last_reload_at = Some(SystemTime::now());
    emit_profile_reload_metrics(&guard);
    drop(guard);
    Ok(())
}

fn emit_profile_reload_metrics(state: &ProfileState) {
    for profile_id in state.profiles.keys() {
        metrics::counter!(
            PROFILE_RELOADS_METRIC,
            "profile_id" => profile_id.clone(),
            "outcome" => "loaded"
        )
        .increment(1);
        let active = state
            .active_profile_id
            .as_ref()
            .is_some_and(|active_id| active_id == profile_id);
        metrics::gauge!(PROFILES_ACTIVE_METRIC, "profile_id" => profile_id.clone())
            .set(if active { 1.0 } else { 0.0 });
    }
    for error in &state.last_errors {
        let profile_id = error
            .path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or("__unknown__");
        metrics::counter!(
            PROFILE_RELOADS_METRIC,
            "profile_id" => profile_id.to_owned(),
            "outcome" => "error"
        )
        .increment(1);
    }
}

fn system_time_epoch_ms(value: SystemTime) -> Option<String> {
    value
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_millis().to_string())
}

fn load_dir(
    profile_dir: &Path,
    bounds: ScreenBounds,
    previous: &BTreeMap<ProfileId, LoadedProfile>,
) -> Result<(BTreeMap<ProfileId, LoadedProfile>, Vec<ProfileLoadError>), ProfileError> {
    let mut profiles = BTreeMap::new();
    let mut errors = Vec::new();
    for entry in fs::read_dir(profile_dir).map_err(|source| ProfileError::Io {
        path: profile_dir.to_path_buf(),
        source,
    })? {
        let entry = entry.map_err(|source| ProfileError::Io {
            path: profile_dir.to_path_buf(),
            source,
        })?;
        let path = entry.path();
        if !path
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| extension.eq_ignore_ascii_case("toml"))
        {
            continue;
        }
        match parse_profile_file_with_bounds(&path, bounds) {
            Ok(profile) => {
                profiles.insert(profile.profile.id.clone(), profile);
            }
            Err(error) => {
                warn!(code = error.code(), error = %error, "profile load failed");
                errors.push(ProfileLoadError::from_error(&error));
                for previous_profile in previous.values() {
                    if previous_profile.source_path == path {
                        profiles.insert(
                            previous_profile.profile.id.clone(),
                            previous_profile.clone(),
                        );
                    }
                }
            }
        }
    }

    Ok((profiles, errors))
}
