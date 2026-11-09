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
