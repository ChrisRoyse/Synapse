use std::collections::BTreeMap;

use calyx_core::{Modality, Panel, Result, Slot, SlotId, SlotShape, SlotState};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RrfProfile {
    Causal,
    Code,
    Entity,
    Temporal,
    Speaker,
    Style,
    Civic,
    Media,
    Bridge,
    Kernel,
    Semantic,
    Lexical,
    Multimodal,
    General,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WeightedProfile {
    pub profile: RrfProfile,
    pub panel_version: u32,
    pub weights: BTreeMap<SlotId, f32>,
    pub lexical_excludes_dense: bool,
}

pub fn weighted_profiles(panel: &Panel) -> Result<Vec<WeightedProfile>> {
    use RrfProfile::*;
    [
        Causal, Code, Entity, Temporal, Speaker, Style, Civic, Media, Bridge, Kernel, Semantic,
        Lexical, Multimodal, General,
    ]
    .into_iter()
    .map(|profile| lookup(profile, panel))
    .collect()
}

pub fn lookup(profile: RrfProfile, panel: &Panel) -> Result<WeightedProfile> {
    let weights = panel
        .slots
        .iter()
        .filter(|slot| slot.state == SlotState::Active && !slot.retrieval_only)
        .filter(|slot| profile_matches(profile, slot))
        .map(|slot| (slot.slot_id, grounded_weight(slot)))
        .collect::<BTreeMap<_, _>>();
    if weights.is_empty() {
        return Err(crate::error::sextant_error(
            crate::error::CALYX_SEXTANT_PROFILE_UNBOUND,
            format!(
                "RRF profile {profile:?} has no contract-matching primary slot in panel {}",
                panel.version
            ),
        ));
    }
    Ok(WeightedProfile {
        profile,
        panel_version: panel.version,
        weights,
        lexical_excludes_dense: profile == RrfProfile::Lexical,
    })
}

fn profile_matches(profile: RrfProfile, slot: &Slot) -> bool {
    use RrfProfile::*;
    match profile {
        General => true,
        Lexical => matches!(slot.shape, SlotShape::Sparse(_)),
        Semantic => matches!(slot.shape, SlotShape::Dense(_) | SlotShape::Multi { .. }),
        Code => slot.modality == Modality::Code || axis_matches(slot, &["code", "ast", "symbol"]),
        Multimodal => {
            slot.modality == Modality::Mixed
                || matches!(
                    slot.modality,
                    Modality::Image | Modality::Audio | Modality::Video
                )
        }
        Causal => axis_matches(
            slot,
            &[
                "causal",
                "cause",
                "effect",
                "action",
                "outcome",
                "transition",
            ],
        ),
        Entity => axis_matches(
            slot,
            &[
                "entity", "app", "process", "document", "url", "tool", "model", "target",
            ],
        ),
        Temporal => axis_matches(
            slot,
            &[
                "temporal", "time", "hour", "day", "duration", "sequence", "position",
            ],
        ),
        Speaker => {
            slot.bits_about
                .contains_key(&calyx_core::AnchorKind::SpeakerMatch)
                || axis_matches(slot, &["speaker", "role", "actor"])
        }
        Style => {
            slot.bits_about
                .contains_key(&calyx_core::AnchorKind::StyleHold)
                || axis_matches(slot, &["style", "persona"])
        }
        Civic => axis_matches(slot, &["civic", "country", "geo", "actor", "event"]),
        Media => {
            matches!(
                slot.modality,
                Modality::Image | Modality::Audio | Modality::Video
            ) || axis_matches(
                slot,
                &["media", "image", "audio", "video", "title", "transcript"],
            )
        }
        Bridge => axis_matches(slot, &["bridge", "relation", "entity", "record"]),
        Kernel => {
            !slot.bits_about.is_empty() || axis_matches(slot, &["kernel", "record", "outcome"])
        }
    }
}

fn axis_matches(slot: &Slot, needles: &[&str]) -> bool {
    let key = slot.slot_key.key().to_ascii_lowercase();
    let axis = slot
        .axis
        .as_deref()
        .unwrap_or_default()
        .to_ascii_lowercase();
    needles
        .iter()
        .any(|needle| key.contains(needle) || axis.contains(needle))
}

fn grounded_weight(slot: &Slot) -> f32 {
    slot.bits_about
        .values()
        .map(|signal| signal.bits)
        .filter(|bits| bits.is_finite() && *bits > 0.0)
        .max_by(f32::total_cmp)
        .unwrap_or(1.0)
}
