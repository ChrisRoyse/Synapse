const ERROR_SUCCESS: u32 = 0;
const ERROR_DEVICE_NOT_CONNECTED: u32 = 1167;
const XINPUT_GAMEPAD_A_RAW: u16 = 0x1000;
#[cfg(windows)]
const XINPUT_POLL_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(50);
#[cfg(windows)]
const XINPUT_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(1);

#[derive(Clone, Debug, Eq, PartialEq)]
struct XInputSlotState {
    slot: u32,
    rc: u32,
    packet: u32,
    buttons: u16,
}

impl XInputSlotState {
    const fn disconnected(slot: u32) -> Self {
        Self {
            slot,
            rc: ERROR_DEVICE_NOT_CONNECTED,
            packet: 0,
            buttons: 0,
        }
    }

    const fn connected(slot: u32, packet: u32, buttons: u16) -> Self {
        Self {
            slot,
            rc: ERROR_SUCCESS,
            packet,
            buttons,
        }
    }

    const fn has_button(&self, button: u16) -> bool {
        self.rc == ERROR_SUCCESS && (self.buttons & button) == button
    }

    const fn is_connected_neutral(&self) -> bool {
        self.rc == ERROR_SUCCESS && self.buttons == 0
    }
}

#[cfg(windows)]
#[tokio::test]
#[ignore = "requires native Windows with ViGEmBus and an interactive XInput stack"]
async fn xinput_observes_vigem_a_button_and_neutral_release() {
    use synapse_action::ActionEmitter;
    use synapse_core::{Action, GamepadReport, PadButton};
    use tokio_util::sync::CancellationToken;

    let before = read_all_slots();
    println!(
        "readback=xinput edge=button_a before={}",
        format_slots(&before)
    );

    let cancel = CancellationToken::new();
    let (handle, snapshot_handle, join) = ActionEmitter::spawn(cancel.clone());

    handle
        .execute(Action::PadReport {
            pad: 0,
            report: GamepadReport {
                buttons: vec![PadButton::A],
                ..GamepadReport::default()
            },
        })
        .await
        .unwrap_or_else(|error| panic!("PadReport(A) must reach the ViGEm backend: {error}"));

    let during = poll_xinput_until(XINPUT_POLL_TIMEOUT, |states| {
        find_new_button_slot(&before, states, XINPUT_GAMEPAD_A_RAW).is_some()
    })
    .await;
    let slot = find_new_button_slot(&before, &during, XINPUT_GAMEPAD_A_RAW).unwrap_or_else(|| {
        panic!(
            "expected a newly pressed XINPUT_GAMEPAD_A within {XINPUT_POLL_TIMEOUT:?}; before={} after={}",
            format_slots(&before),
            format_slots(&during)
        )
    });
    println!(
        "readback=xinput edge=button_a after_slot={slot} after={} expected_button=0x{XINPUT_GAMEPAD_A_RAW:04x}",
        format_slots(&during)
    );

    let held_snapshot = snapshot_handle
        .snapshot()
        .await
        .unwrap_or_else(|error| panic!("held pad snapshot should be readable: {error}"));
    assert!(held_snapshot.pad_state.contains_key(&0));
    println!("readback=emitter_snapshot edge=button_a after={held_snapshot:?}");

    handle
        .execute(Action::ReleaseAll)
        .await
        .unwrap_or_else(|error| panic!("ReleaseAll must neutralize the ViGEm pad: {error}"));
    let after_release = poll_xinput_until(XINPUT_POLL_TIMEOUT, |states| {
        slot_connected_neutral(states, slot)
    })
    .await;
    println!(
        "readback=xinput edge=release before_slot={slot} before={} after={}",
        format_slots(&during),
        format_slots(&after_release)
    );
    assert!(
        slot_connected_neutral(&after_release, slot),
        "expected slot {slot} to remain connected with wButtons=0 after ReleaseAll; after={}",
        format_slots(&after_release)
    );

    let released_snapshot = snapshot_handle
        .snapshot()
        .await
        .unwrap_or_else(|error| panic!("released pad snapshot should be readable: {error}"));
    assert!(released_snapshot.pad_state.is_empty());
    println!("readback=emitter_snapshot edge=release after={released_snapshot:?}");

    cancel.cancel();
    let final_snapshot = join
        .await
        .unwrap_or_else(|error| panic!("emitter task should join cleanly: {error}"));
    assert!(final_snapshot.pad_state.is_empty());
    println!("readback=emitter_snapshot edge=shutdown after={final_snapshot:?}");
}

#[cfg(not(windows))]
#[test]
fn xinput_is_windows_only_boundary() {
    let before = platform_readback();
    println!("readback=xinput edge=platform_boundary before={before}");
    let after = platform_readback();
    println!("readback=xinput edge=platform_boundary after={after}");
    assert_eq!(after, "not_windows");
}

#[test]
fn transition_detector_selects_new_a_button_slot() {
    let before = vec![
        XInputSlotState::disconnected(0),
        XInputSlotState::connected(1, 7, 0),
    ];
    let after = vec![
        XInputSlotState::connected(0, 1, XINPUT_GAMEPAD_A_RAW),
        XInputSlotState::connected(1, 8, 0),
    ];
    println!(
        "readback=xinput_transition edge=new_button before={} after={}",
        format_slots(&before),
        format_slots(&after)
    );
    assert_eq!(
        find_new_button_slot(&before, &after, XINPUT_GAMEPAD_A_RAW),
        Some(0)
    );
}

#[test]
fn transition_detector_rejects_preexisting_a_button() {
    let before = vec![XInputSlotState::connected(0, 1, XINPUT_GAMEPAD_A_RAW)];
    let after = vec![XInputSlotState::connected(0, 2, XINPUT_GAMEPAD_A_RAW)];
    println!(
        "readback=xinput_transition edge=preexisting_button before={} after={}",
        format_slots(&before),
        format_slots(&after)
    );
    assert_eq!(
        find_new_button_slot(&before, &after, XINPUT_GAMEPAD_A_RAW),
        None
    );
}

#[test]
fn neutral_readback_requires_connected_zero_buttons() {
    let before = vec![
        XInputSlotState::connected(0, 3, XINPUT_GAMEPAD_A_RAW),
        XInputSlotState::disconnected(1),
    ];
    let after = vec![
        XInputSlotState::connected(0, 4, 0),
        XInputSlotState::disconnected(1),
    ];
    println!(
        "readback=xinput_neutral edge=release before={} after={}",
        format_slots(&before),
        format_slots(&after)
    );
    assert!(slot_connected_neutral(&after, 0));
    assert!(!slot_connected_neutral(&after, 1));
}

#[must_use]
fn find_new_button_slot(
    before: &[XInputSlotState],
    after: &[XInputSlotState],
    button: u16,
) -> Option<u32> {
    after
        .iter()
        .find(|current| {
            current.has_button(button)
                && !before
                    .iter()
                    .any(|previous| previous.slot == current.slot && previous.has_button(button))
        })
        .map(|state| state.slot)
}

#[must_use]
fn slot_connected_neutral(states: &[XInputSlotState], slot: u32) -> bool {
    states
        .iter()
        .find(|state| state.slot == slot)
        .is_some_and(XInputSlotState::is_connected_neutral)
}

#[must_use]
fn format_slots(states: &[XInputSlotState]) -> String {
    states
        .iter()
        .map(|state| {
            format!(
                "slot={} rc={} packet={} wButtons=0x{:04x}",
                state.slot, state.rc, state.packet, state.buttons
            )
        })
        .collect::<Vec<_>>()
        .join("; ")
}

#[cfg(not(windows))]
const fn platform_readback() -> &'static str {
    "not_windows"
}

#[cfg(windows)]
async fn poll_xinput_until<F>(
    timeout: std::time::Duration,
    mut predicate: F,
) -> Vec<XInputSlotState>
where
    F: FnMut(&[XInputSlotState]) -> bool,
{
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let states = read_all_slots();
        if predicate(&states) || tokio::time::Instant::now() >= deadline {
            return states;
        }
        tokio::time::sleep(XINPUT_POLL_INTERVAL).await;
    }
}

#[cfg(windows)]
fn read_all_slots() -> Vec<XInputSlotState> {
    (0..4).map(read_slot).collect()
}

#[cfg(windows)]
fn read_slot(slot: u32) -> XInputSlotState {
    use windows::Win32::UI::Input::XboxController::{XINPUT_STATE, XInputGetState};

    let mut state = XINPUT_STATE::default();
    // SAFETY: `state` is a valid, writable `XINPUT_STATE` pointer for the
    // duration of the call, and `slot` is constrained by `read_all_slots` to
    // the documented XInput user index range 0..=3.
    let rc = unsafe { XInputGetState(slot, &raw mut state) };
    if rc == ERROR_SUCCESS {
        XInputSlotState::connected(slot, state.dwPacketNumber, state.Gamepad.wButtons.0)
    } else {
        XInputSlotState {
            slot,
            rc,
            packet: 0,
            buttons: 0,
        }
    }
}
