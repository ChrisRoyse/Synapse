use synapse_core::KeystrokeDynamics;

use crate::sample_typing_schedule;

pub(super) const TEXT_VK_RETURN: u16 = 0x0D;
pub(super) const TEXT_VK_TAB: u16 = 0x09;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct TextDispatchStep {
    pub(super) iki_ms_before: u32,
    pub(super) inputs: Vec<TextDispatchInput>,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub(super) enum TextDispatchInput {
    UnicodeUnit(u16),
    VirtualKey(u16),
}

impl TextDispatchInput {
    #[cfg(test)]
    const fn sendinput_event_count(self) -> usize {
        match self {
            Self::UnicodeUnit(_) | Self::VirtualKey(_) => 2,
        }
    }
}

pub(super) fn text_dispatch_plan(
    text: &str,
    dynamics: &KeystrokeDynamics,
) -> Vec<TextDispatchStep> {
    sample_typing_schedule(text, dynamics, None)
        .into_iter()
        .map(|event| TextDispatchStep {
            iki_ms_before: event.iki_ms_before,
            inputs: dispatch_inputs(event.r#char),
        })
        .collect()
}

fn dispatch_inputs(ch: char) -> Vec<TextDispatchInput> {
    match ch {
        '\n' | '\r' => vec![TextDispatchInput::VirtualKey(TEXT_VK_RETURN)],
        '\t' => vec![TextDispatchInput::VirtualKey(TEXT_VK_TAB)],
        _ => {
            let mut units = [0; 2];
            ch.encode_utf16(&mut units)
                .iter()
                .copied()
                .map(TextDispatchInput::UnicodeUnit)
                .collect()
        }
    }
}

#[cfg(test)]
mod tests {
    use synapse_core::KeystrokeDynamics;

    use super::{TEXT_VK_RETURN, TEXT_VK_TAB, TextDispatchInput, text_dispatch_plan};

    #[test]
    fn linear_41_char_text_keeps_timing_and_bounded_sendinput_batches() {
        let before = "Synapse M2 SendInput 2026-05-23 cafe OK!!";
        assert_eq!(before.chars().count(), 41);

        let dynamics = KeystrokeDynamics::Linear { ms_per_char: 50 };
        let after = text_dispatch_plan(before, &dynamics);
        let delays: Vec<_> = after.iter().map(|step| step.iki_ms_before).collect();
        let total_delay_ms: u32 = delays.iter().sum();
        let max_call_events = after
            .iter()
            .flat_map(|step| step.inputs.iter())
            .map(|input| input.sendinput_event_count())
            .max()
            .unwrap_or(0);
        println!(
            "readback=text_dispatch_plan edge=linear_41 before=text:{before:?},dynamics:Linear(50) after={after:?} result_value=steps:{},total_delay_ms:{total_delay_ms},max_sendinput_events_per_call:{max_call_events}",
            after.len()
        );

        assert_eq!(after.len(), 41);
        assert_eq!(delays[0], 0);
        assert!(delays[1..].iter().all(|delay| *delay == 50));
        assert_eq!(total_delay_ms, 2_000);
        assert_eq!(max_call_events, 2);
    }

    #[test]
    fn newline_and_tab_use_virtual_keys_not_unicode_controls() {
        let before = "A\n\t";
        let dynamics = KeystrokeDynamics::Linear { ms_per_char: 25 };
        let after = text_dispatch_plan(before, &dynamics);
        let inputs: Vec<_> = after
            .iter()
            .flat_map(|step| step.inputs.iter().copied())
            .collect();
        println!(
            "readback=text_dispatch_plan edge=controls before=text:{before:?},dynamics:Linear(25) after={after:?} result_value=inputs:{inputs:?}"
        );

        assert_eq!(after.len(), 3);
        assert_eq!(
            after[0].inputs,
            [TextDispatchInput::UnicodeUnit(u16::from(b'A'))]
        );
        assert_eq!(
            after[1].inputs,
            [TextDispatchInput::VirtualKey(TEXT_VK_RETURN)]
        );
        assert_eq!(
            after[2].inputs,
            [TextDispatchInput::VirtualKey(TEXT_VK_TAB)]
        );
        assert_eq!(
            after
                .iter()
                .map(|step| step.iki_ms_before)
                .collect::<Vec<_>>(),
            [0, 25, 25]
        );
    }

    #[test]
    fn empty_input_stays_empty() {
        let before = "";
        let after = text_dispatch_plan(before, &KeystrokeDynamics::Burst);
        println!(
            "readback=text_dispatch_plan edge=empty before=text:{before:?},dynamics:Burst after={after:?} result_value=steps:{}",
            after.len()
        );

        assert!(after.is_empty());
    }

    #[test]
    fn supplementary_unicode_splits_surrogates_without_oversized_calls() {
        let before = "A😀";
        let dynamics = KeystrokeDynamics::Linear { ms_per_char: 7 };
        let after = text_dispatch_plan(before, &dynamics);
        println!(
            "readback=text_dispatch_plan edge=supplementary_unicode before=text:{before:?},dynamics:Linear(7) after={after:?} result_value=second_step_inputs:{:?}",
            after[1].inputs
        );

        assert_eq!(after.len(), 2);
        assert_eq!(
            after[0].inputs,
            [TextDispatchInput::UnicodeUnit(u16::from(b'A'))]
        );
        assert_eq!(
            after[1].inputs,
            [
                TextDispatchInput::UnicodeUnit(0xD83D),
                TextDispatchInput::UnicodeUnit(0xDE00),
            ]
        );
        assert!(
            after
                .iter()
                .flat_map(|step| step.inputs.iter())
                .all(|input| input.sendinput_event_count() == 2)
        );
    }

    #[test]
    fn burst_long_text_still_keeps_each_sendinput_call_bounded() {
        let before = "x".repeat(64);
        let after = text_dispatch_plan(&before, &KeystrokeDynamics::Burst);
        let max_call_events = after
            .iter()
            .flat_map(|step| step.inputs.iter())
            .map(|input| input.sendinput_event_count())
            .max()
            .unwrap_or(0);
        println!(
            "readback=text_dispatch_plan edge=burst_long before=len:{} after={after:?} result_value=steps:{},max_sendinput_events_per_call:{max_call_events}",
            before.len(),
            after.len()
        );

        assert_eq!(after.len(), 64);
        assert!(after.iter().all(|step| step.iki_ms_before == 0));
        assert_eq!(max_call_events, 2);
    }
}
