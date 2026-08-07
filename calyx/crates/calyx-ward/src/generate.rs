//! Identity-locked generation guard loop for PH39.

use calyx_core::{
    AnchorKind, CalyxError, Clock, CxId, Input, LedgerRef, Lens, Modality, Result as CalyxResult,
    SlotVector,
};
use calyx_ledger::{LedgerAppender, LedgerCfStore};
use serde::{Deserialize, Serialize};

use crate::error::WardError;
use crate::guard::{ProducedSlots, guard, validate_non_inert_profile};
use crate::identity::IdentityProfile;
use crate::ledger::{WardLedgerResult, append_guard_verdict};
use crate::novelty::{NoveltyHandler, NoveltyRecord};
use crate::profile::NoveltyAction;
use crate::verdict::GuardVerdict;

pub const GUARDED_PASS_TAG: &str = "guarded:pass";
pub const GUARDED_REJECT_TAG: &str = "guarded:reject";
pub const GUARDED_REJECT_UNPROVENANCED_TAG: &str = "guarded:reject:unprovenanced";
/// Audio sample rate required by the persisted speaker-slot generation contract.
pub const WAVLM_SAMPLE_RATE: u32 = 16_000;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GenerateInput {
    pub candidate_audio: Option<Vec<f32>>,
    pub candidate_text: Option<String>,
    pub sample_rate: u32,
    pub matched_cx_id: CxId,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum GenerateOutput {
    Accepted {
        verdict: GuardVerdict,
        provenance_tag: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ledger_ref: Option<LedgerRef>,
    },
    Novel {
        record: NoveltyRecord,
    },
    Rejected {
        verdict: GuardVerdict,
        provenance_tag: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ledger_ref: Option<LedgerRef>,
    },
}

/// Runs identity generation without flattening slots.
pub fn guard_generate(
    identity_profile: &IdentityProfile,
    input: &GenerateInput,
    speaker_lens: &dyn Lens,
    style_lens: &dyn Lens,
    novelty_handler: &NoveltyHandler,
    high_stakes: bool,
) -> Result<GenerateOutput, WardError> {
    reject_inert_identity_profile(identity_profile)?;
    if high_stakes && !identity_profile.is_calibrated() {
        return Err(WardError::Provisional {
            guard_id: identity_profile.guard_profile.guard_id,
        });
    }

    let produced = produced_slots(identity_profile, input, speaker_lens, style_lens)?;
    let verdict = guard(
        &identity_profile.guard_profile,
        &produced,
        &identity_profile.matched_slot_cache,
        high_stakes,
    )?;
    route_verdict(identity_profile, verdict, &produced, novelty_handler)
}

pub fn guard_generate_with_ledger<S, C>(
    appender: &mut LedgerAppender<S, C>,
    identity_profile: &IdentityProfile,
    input: &GenerateInput,
    speaker_lens: &dyn Lens,
    style_lens: &dyn Lens,
    novelty_handler: &NoveltyHandler,
    high_stakes: bool,
) -> WardLedgerResult<GenerateOutput>
where
    S: LedgerCfStore,
    C: Clock,
{
    reject_inert_identity_profile(identity_profile)?;
    if high_stakes && !identity_profile.is_calibrated() {
        return Err(WardError::Provisional {
            guard_id: identity_profile.guard_profile.guard_id,
        }
        .into());
    }

    let produced = produced_slots(identity_profile, input, speaker_lens, style_lens)?;
    let verdict = guard(
        &identity_profile.guard_profile,
        &produced,
        &identity_profile.matched_slot_cache,
        high_stakes,
    )?;
    let output = route_verdict(
        identity_profile,
        verdict.clone(),
        &produced,
        novelty_handler,
    )?;
    let ledger_ref = append_guard_verdict(appender, input.matched_cx_id, &verdict)?;

    match output {
        GenerateOutput::Accepted {
            verdict,
            provenance_tag,
            ..
        } => Ok(GenerateOutput::Accepted {
            verdict,
            provenance_tag,
            ledger_ref: Some(ledger_ref),
        }),
        GenerateOutput::Rejected { verdict, .. } => Ok(GenerateOutput::Rejected {
            verdict,
            provenance_tag: GUARDED_REJECT_TAG.to_string(),
            ledger_ref: Some(ledger_ref),
        }),
        GenerateOutput::Novel { record } => Ok(GenerateOutput::Novel { record }),
    }
}

fn reject_inert_identity_profile(identity_profile: &IdentityProfile) -> Result<(), WardError> {
    if identity_profile.identity_slots.is_empty() {
        return Err(WardError::InvalidRequiredSlotDerivation {
            reason: "identity generation requires at least one identity slot",
        });
    }
    validate_non_inert_profile(&identity_profile.guard_profile)?;
    Ok(())
}

fn produced_slots(
    identity_profile: &IdentityProfile,
    input: &GenerateInput,
    speaker_lens: &dyn Lens,
    style_lens: &dyn Lens,
) -> Result<ProducedSlots, WardError> {
    let mut produced = ProducedSlots::new();
    for slot in &identity_profile.identity_slots {
        match slot.anchor_kind {
            AnchorKind::SpeakerMatch => {
                if let Some(audio) = input.candidate_audio.as_deref() {
                    let bytes = audio_input_bytes(audio, input.sample_rate)?;
                    produced.insert(
                        slot.slot_id,
                        dense_data(speaker_lens.measure(&Input::new(Modality::Audio, bytes)))?,
                    );
                }
            }
            AnchorKind::StyleHold => {
                if let Some(text) = input.candidate_text.as_deref() {
                    produced.insert(
                        slot.slot_id,
                        dense_data(
                            style_lens.measure(&Input::new(Modality::Text, text.as_bytes())),
                        )?,
                    );
                }
            }
            _ => {
                return Err(WardError::InvalidRequiredSlotDerivation {
                    reason: "identity slot anchor kind must be SpeakerMatch or StyleHold",
                });
            }
        }
    }
    Ok(produced)
}

/// guarded:pass
fn route_verdict(
    identity_profile: &IdentityProfile,
    verdict: GuardVerdict,
    produced: &ProducedSlots,
    novelty_handler: &NoveltyHandler,
) -> Result<GenerateOutput, WardError> {
    if verdict.overall_pass {
        return Ok(GenerateOutput::Accepted {
            verdict,
            provenance_tag: GUARDED_PASS_TAG.to_string(),
            ledger_ref: None,
        });
    }

    // CALYX_GUARD_OOD
    match novelty_handler.handle(&identity_profile.guard_profile, &verdict, produced) {
        Ok(record) => Ok(GenerateOutput::Novel { record }),
        Err(WardError::Ood { .. })
            if identity_profile.guard_profile.novelty_action == NoveltyAction::RejectClosed =>
        {
            Ok(GenerateOutput::Rejected {
                verdict,
                provenance_tag: GUARDED_REJECT_UNPROVENANCED_TAG.to_string(),
                ledger_ref: None,
            })
        }
        Err(error) => Err(error),
    }
}

fn dense_data(result: CalyxResult<SlotVector>) -> Result<Vec<f32>, WardError> {
    match result.map_err(ward_runtime)? {
        SlotVector::Dense { data, .. } => Ok(data),
        other => Err(WardError::InvalidInput {
            reason: format!("identity generation lens returned non-dense vector: {other:?}"),
        }),
    }
}

fn audio_input_bytes(audio: &[f32], sample_rate: u32) -> Result<Vec<u8>, WardError> {
    let prepared = prepare_audio(audio, sample_rate)?;
    Ok(prepared
        .iter()
        .flat_map(|sample| sample.to_le_bytes())
        .collect())
}

fn prepare_audio(audio: &[f32], sample_rate: u32) -> Result<Vec<f32>, WardError> {
    if audio.is_empty() {
        return Err(WardError::InvalidInput {
            reason: "empty generation audio".to_string(),
        });
    }
    if sample_rate == 0 {
        return Err(WardError::InvalidInput {
            reason: "generation sample_rate must be non-zero".to_string(),
        });
    }
    if audio.iter().any(|value| !value.is_finite()) {
        return Err(WardError::InvalidInput {
            reason: "generation audio contains NaN or Inf".to_string(),
        });
    }
    if sample_rate == WAVLM_SAMPLE_RATE {
        Ok(audio.to_vec())
    } else {
        Ok(resample_linear(audio, sample_rate, WAVLM_SAMPLE_RATE))
    }
}

fn resample_linear(audio: &[f32], in_rate: u32, out_rate: u32) -> Vec<f32> {
    let out_len = ((audio.len() as f64) * (out_rate as f64) / (in_rate as f64))
        .round()
        .max(1.0) as usize;
    if audio.len() == 1 {
        return vec![audio[0]; out_len];
    }
    let scale = in_rate as f64 / out_rate as f64;
    (0..out_len)
        .map(|idx| {
            let pos = idx as f64 * scale;
            let lo = pos.floor() as usize;
            let hi = (lo + 1).min(audio.len() - 1);
            let frac = (pos - lo as f64) as f32;
            audio[lo] * (1.0 - frac) + audio[hi] * frac
        })
        .collect()
}

fn ward_runtime(error: CalyxError) -> WardError {
    WardError::Runtime {
        reason: format!("{}: {}", error.code, error.message),
    }
}
