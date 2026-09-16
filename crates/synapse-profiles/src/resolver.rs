use regex::Regex;
use synapse_core::{ProfileId, ProfileMatch};
use tracing::instrument;

use crate::parser::LoadedProfile;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ForegroundWindow {
    pub exe: Option<String>,
    pub title: Option<String>,
    pub steam_appid: Option<u32>,
    pub window_class: Option<String>,
}

#[derive(Copy, Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum MatchRank {
    WindowClass = 1,
    SteamAppId = 2,
    TitleRegex = 3,
    Exe = 4,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProfileMatchResolution {
    pub profile_id: ProfileId,
    pub rank_name: &'static str,
}

/// Resolves the foreground profile using ADR-0006 precedence:
/// `exe > title_regex > steam_appid > window_class`, then newest file mtime.
#[instrument(skip_all, fields(profile_count = profiles.len()))]
#[must_use]
pub fn resolve_active_profile(
    profiles: &[LoadedProfile],
    foreground: &ForegroundWindow,
) -> Option<ProfileMatchResolution> {
    profiles
        .iter()
        .enumerate()
        .filter_map(|(index, loaded)| {
            best_rank(&loaded.profile.matches, foreground).map(|rank| (loaded, rank, index))
        })
        .max_by(
            |(left, left_rank, left_index), (right, right_rank, right_index)| {
                left_rank
                    .cmp(right_rank)
                    .then_with(|| left.modified.cmp(&right.modified))
                    .then_with(|| right.source_path.cmp(&left.source_path))
                    .then_with(|| right.profile.id.cmp(&left.profile.id))
                    .then_with(|| right_index.cmp(left_index))
            },
        )
        .map(|(loaded, rank, _index)| ProfileMatchResolution {
            profile_id: loaded.profile.id.clone(),
            rank_name: rank.name(),
        })
}

fn best_rank(matches: &[ProfileMatch], foreground: &ForegroundWindow) -> Option<MatchRank> {
    matches
        .iter()
        .filter_map(|candidate| candidate_rank(candidate, foreground))
        .max()
}

fn candidate_rank(candidate: &ProfileMatch, foreground: &ForegroundWindow) -> Option<MatchRank> {
    let mut rank = if let Some(expected) = candidate.exe.as_deref() {
        let actual = foreground.exe.as_deref()?;
        if !expected.eq_ignore_ascii_case(actual) {
            return None;
        }
        Some(MatchRank::Exe)
    } else {
        None
    };

    if let Some(pattern) = candidate.title_regex.as_deref() {
        let title = foreground.title.as_deref()?;
        let Ok(regex) = Regex::new(pattern) else {
            return None;
        };
        if !regex.is_match(title) {
            return None;
        }
        rank = Some(rank.map_or(MatchRank::TitleRegex, |value| {
            value.max(MatchRank::TitleRegex)
        }));
    }

    if let Some(expected) = candidate.steam_appid {
        let actual = foreground.steam_appid?;
        if expected != actual {
            return None;
        }
        rank = Some(rank.map_or(MatchRank::SteamAppId, |value| {
            value.max(MatchRank::SteamAppId)
        }));
    }

    if let Some(expected) = candidate.window_class.as_deref() {
        let actual = foreground.window_class.as_deref()?;
        if !expected.eq_ignore_ascii_case(actual) {
            return None;
        }
        rank = Some(rank.map_or(MatchRank::WindowClass, |value| {
            value.max(MatchRank::WindowClass)
        }));
    }

    rank
}

impl MatchRank {
    const fn name(self) -> &'static str {
        match self {
            Self::Exe => "exe",
            Self::TitleRegex => "title_regex",
            Self::SteamAppId => "steam_appid",
            Self::WindowClass => "window_class",
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        path::PathBuf,
        time::{Duration, UNIX_EPOCH},
    };

    use synapse_core::{
        Backend, OcrBackend, PerceptionMode, Profile, ProfileBackends, ProfileCapture,
        ProfileCaptureTarget, ProfileDetection, ProfileMatch, ProfileOcr, ProfileUseScope,
    };

    use super::{ForegroundWindow, resolve_active_profile};
    use crate::parser::{LoadedProfile, ProfileDefaults};

    #[test]
    fn adr_0006_prefers_exe_over_newer_title_regex() {
        let profiles = vec![
            loaded_profile(
                "title",
                vec![profile_match().title_regex(".*Visual Studio Code.*")],
                20,
                "profiles/title.toml",
            ),
            loaded_profile(
                "exe",
                vec![profile_match().exe("Code.exe")],
                10,
                "profiles/exe.toml",
            ),
        ];
        let foreground = foreground()
            .exe("Code.exe")
            .title("agent - Visual Studio Code");

        let Some(resolution) = resolve_active_profile(&profiles, &foreground) else {
            panic!("profile matched");
        };

        assert_eq!(resolution.profile_id, "exe");
        assert_eq!(resolution.rank_name, "exe");
    }

    #[test]
    fn adr_0006_uses_newer_mtime_for_same_rank_conflict() {
        let profiles = vec![
            loaded_profile(
                "old",
                vec![profile_match().exe("Code.exe")],
                10,
                "profiles/old.toml",
            ),
            loaded_profile(
                "new",
                vec![profile_match().exe("Code.exe")],
                20,
                "profiles/new.toml",
            ),
        ];
        let foreground = foreground().exe("Code.exe");

        let Some(resolution) = resolve_active_profile(&profiles, &foreground) else {
            panic!("profile matched");
        };

        assert_eq!(resolution.profile_id, "new");
        assert_eq!(resolution.rank_name, "exe");
    }

    #[test]
    fn adr_0006_uses_strongest_matching_field_within_profile() {
        let profiles = vec![
            loaded_profile(
                "steam",
                vec![profile_match().steam_appid(42)],
                30,
                "profiles/steam.toml",
            ),
            loaded_profile(
                "mixed",
                vec![
                    profile_match().window_class("Chrome_WidgetWin_1"),
                    profile_match().title_regex(".*Visual Studio Code.*"),
                ],
                10,
                "profiles/mixed.toml",
            ),
        ];
        let foreground = foreground()
            .steam_appid(42)
            .window_class("Chrome_WidgetWin_1")
            .title("agent - Visual Studio Code");

        let Some(resolution) = resolve_active_profile(&profiles, &foreground) else {
            panic!("profile matched");
        };

        assert_eq!(resolution.profile_id, "mixed");
        assert_eq!(resolution.rank_name, "title_regex");
    }

    #[test]
    fn match_entry_requires_every_declared_field() {
        let profiles = vec![loaded_profile(
            "luanti",
            vec![ProfileMatch {
                exe: Some("luanti.exe".to_owned()),
                title_regex: Some("^Luanti 5\\.16\\.[0-9]+".to_owned()),
                steam_appid: None,
                window_class: None,
                process_args: Vec::new(),
            }],
            10,
            "profiles/luanti.minetest.toml",
        )];

        let wrong_title = foreground()
            .exe("luanti.exe")
            .title("Unexpected Launcher Window");
        let right_title = foreground()
            .exe("luanti.exe")
            .title("Luanti 5.16.1 [Multiplayer] [4.6.0 NVIDIA 610.47]");

        assert_eq!(resolve_active_profile(&profiles, &wrong_title), None);
        let Some(resolution) = resolve_active_profile(&profiles, &right_title) else {
            panic!("Luanti profile should match when exe and title both match");
        };
        assert_eq!(resolution.profile_id, "luanti");
        assert_eq!(resolution.rank_name, "exe");
    }

    #[test]
    fn adr_0006_ignores_invalid_title_regex_candidate() {
        let profiles = vec![
            loaded_profile(
                "invalid_title",
                vec![profile_match().title_regex("(")],
                20,
                "profiles/invalid_title.toml",
            ),
            loaded_profile(
                "class",
                vec![profile_match().window_class("ApplicationFrameWindow")],
                10,
                "profiles/class.toml",
            ),
        ];
        let foreground = foreground()
            .title("Any title")
            .window_class("ApplicationFrameWindow");

        let Some(resolution) = resolve_active_profile(&profiles, &foreground) else {
            panic!("profile matched");
        };

        assert_eq!(resolution.profile_id, "class");
        assert_eq!(resolution.rank_name, "window_class");
    }

    fn loaded_profile(
        id: &str,
        matches: Vec<ProfileMatch>,
        modified_secs: u64,
        source_path: &str,
    ) -> LoadedProfile {
        LoadedProfile {
            profile: Profile {
                id: id.to_owned(),
                label: id.to_owned(),
                version: "1.0.0".to_owned(),
                use_scope: ProfileUseScope::Productivity,
                matches,
                mode: PerceptionMode::A11yOnly,
                capture: ProfileCapture {
                    target: ProfileCaptureTarget::ForegroundWindow,
                    min_update_interval_ms: 50,
                    cursor_visible: true,
                },
                detection: ProfileDetection {
                    model_id: None,
                    classes_of_interest: Vec::new(),
                    confidence_threshold: 0.0,
                    max_detections: 0,
                },
                ocr: ProfileOcr {
                    default_backend: OcrBackend::Auto,
                    regions: Vec::new(),
                    parser_config: BTreeMap::new(),
                },
                hud: Vec::new(),
                keymap: BTreeMap::new(),
                backends: ProfileBackends {
                    default: Backend::Software,
                    keyboard_default: Backend::Software,
                    mouse_default: Backend::Software,
                    pad_default: Backend::Software,
                },
                event_extensions: Vec::new(),
                metadata: BTreeMap::new(),
            },
            schema_version: 1,
            defaults: ProfileDefaults {
                mouse_velocity_profile_default: "natural".to_owned(),
                keyboard_dynamics_default: "natural".to_owned(),
            },
            source_path: PathBuf::from(source_path),
            modified: UNIX_EPOCH + Duration::from_secs(modified_secs),
        }
    }

    struct ProfileMatchBuilder {
        profile_match: ProfileMatch,
    }

    fn profile_match() -> ProfileMatchBuilder {
        ProfileMatchBuilder {
            profile_match: ProfileMatch {
                exe: None,
                title_regex: None,
                steam_appid: None,
                window_class: None,
                process_args: Vec::new(),
            },
        }
    }

    impl ProfileMatchBuilder {
        fn exe(mut self, exe: &str) -> ProfileMatch {
            self.profile_match.exe = Some(exe.to_owned());
            self.profile_match
        }

        fn title_regex(mut self, title_regex: &str) -> ProfileMatch {
            self.profile_match.title_regex = Some(title_regex.to_owned());
            self.profile_match
        }

        fn steam_appid(mut self, steam_appid: u32) -> ProfileMatch {
            self.profile_match.steam_appid = Some(steam_appid);
            self.profile_match
        }

        fn window_class(mut self, window_class: &str) -> ProfileMatch {
            self.profile_match.window_class = Some(window_class.to_owned());
            self.profile_match
        }
    }

    #[derive(Default)]
    struct ForegroundWindowBuilder {
        foreground: ForegroundWindow,
    }

    fn foreground() -> ForegroundWindowBuilder {
        ForegroundWindowBuilder {
            foreground: ForegroundWindow::default(),
        }
    }

    impl ForegroundWindowBuilder {
        fn exe(mut self, exe: &str) -> Self {
            self.foreground.exe = Some(exe.to_owned());
            self
        }

        fn title(mut self, title: &str) -> Self {
            self.foreground.title = Some(title.to_owned());
            self
        }

        fn steam_appid(mut self, steam_appid: u32) -> Self {
            self.foreground.steam_appid = Some(steam_appid);
            self
        }

        fn window_class(mut self, window_class: &str) -> Self {
            self.foreground.window_class = Some(window_class.to_owned());
            self
        }
    }

    impl AsRef<ForegroundWindow> for ForegroundWindowBuilder {
        fn as_ref(&self) -> &ForegroundWindow {
            &self.foreground
        }
    }

    impl std::ops::Deref for ForegroundWindowBuilder {
        type Target = ForegroundWindow;

        fn deref(&self) -> &Self::Target {
            &self.foreground
        }
    }
}
