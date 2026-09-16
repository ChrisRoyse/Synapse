use std::time::Duration;

use synapse_action::{
    ACTION_QUEUE_CAPACITY, ActionError, ActionHandle, operator_release_epoch,
    operator_release_requested_since,
};
use synapse_core::{Action, Backend, Key, KeyCode};

#[test]
fn try_execute_reports_queue_full_at_capacity_boundary() {
    let (handle, rx) = ActionHandle::channel();
    assert_eq!(rx.len(), 0);
    println!(
        "readback=action_queue edge=capacity before=queued:{}",
        rx.len()
    );

    for _index in 0..ACTION_QUEUE_CAPACITY {
        handle
            .try_execute(Action::ReleaseAll)
            .unwrap_or_else(|err| panic!("queue should accept capacity-sized burst: {err}"));
    }

    assert_eq!(rx.len(), ACTION_QUEUE_CAPACITY);
    let error = match handle.try_execute(Action::ReleaseAll) {
        Ok(()) => panic!("257th queued action should hit bounded mpsc capacity"),
        Err(error) => error,
    };
    assert_eq!(error.code(), synapse_core::error_codes::ACTION_QUEUE_FULL);
    println!(
        "readback=action_queue edge=capacity after=queued:{} result_value={}",
        rx.len(),
        error.code()
    );
}

#[test]
fn try_execute_reports_closed_channel_without_mutating_queue() {
    let (handle, rx) = ActionHandle::channel();
    println!(
        "readback=action_queue edge=closed before=queued:{}",
        rx.len()
    );
    drop(rx);

    let error = match handle.try_execute(Action::ReleaseAll) {
        Ok(()) => panic!("closed receiver should reject enqueue"),
        Err(error) => error,
    };
    assert!(matches!(error, ActionError::BackendUnavailable { .. }));
    println!(
        "readback=action_queue edge=closed after=receiver_dropped result_value={}",
        error.code()
    );
}

#[test]
fn release_all_blocking_timeout_enqueues_then_reports_timeout() {
    let (handle, rx) = ActionHandle::channel();
    println!(
        "readback=action_queue edge=release_timeout before=queued:{}",
        rx.len()
    );

    let error = match handle.fire_release_all_blocking_with_timeout(Duration::ZERO) {
        Ok(()) => panic!("no actor can ack release_all, so this must time out"),
        Err(error) => error,
    };
    assert!(matches!(error, ActionError::BackendUnavailable { .. }));
    assert_eq!(rx.len(), 1);
    println!(
        "readback=action_queue edge=release_timeout after=queued:{} result_value={}",
        rx.len(),
        error.detail()
    );
}

#[tokio::test]
async fn execute_waits_for_actor_ack_happy_path() {
    let (handle, mut rx) = ActionHandle::channel();
    println!(
        "readback=action_queue edge=execute_ack before=queued:{}",
        rx.len()
    );

    let actor = tokio::spawn(async move {
        let Some((action, ack, _operator_panic_epoch_at_enqueue)) = rx.recv().await else {
            panic!("handle should enqueue one action");
        };
        assert!(matches!(action, Action::ReleaseAll));
        ack.send(Ok(()))
            .unwrap_or_else(|err| panic!("test receiver should still wait for ack: {err:?}"));
        rx.len()
    });

    handle
        .execute(Action::ReleaseAll)
        .await
        .unwrap_or_else(|err| panic!("actor ack should make execute succeed: {err}"));
    let final_len = actor
        .await
        .unwrap_or_else(|err| panic!("actor task should complete: {err}"));
    assert_eq!(final_len, 0);
    println!("readback=action_queue edge=execute_ack after=queued:{final_len}");
}

#[tokio::test]
async fn execute_reports_queue_full_when_normal_queue_is_saturated() {
    let (handle, rx) = ActionHandle::channel();
    for index in 0..ACTION_QUEUE_CAPACITY {
        handle
            .try_execute(Action::KeyDown {
                key: key_named(&format!("held-{index}")),
                backend: Backend::Software,
            })
            .unwrap_or_else(|err| panic!("queue should accept capacity-sized burst: {err}"));
    }
    let before_len = rx.len();
    println!("readback=action_queue edge=execute_full before=queued:{before_len}");

    let error = match handle
        .execute(Action::KeyDown {
            key: key_named("overflow"),
            backend: Backend::Software,
        })
        .await
    {
        Ok(()) => panic!("execute must fail closed when the bounded action queue is full"),
        Err(error) => error,
    };

    assert_eq!(before_len, ACTION_QUEUE_CAPACITY);
    assert_eq!(rx.len(), ACTION_QUEUE_CAPACITY);
    assert_eq!(error.code(), synapse_core::error_codes::ACTION_QUEUE_FULL);
    println!(
        "readback=action_queue edge=execute_full after=queued:{} result_value={} detail={}",
        rx.len(),
        error.code(),
        error.detail()
    );
}

#[tokio::test]
async fn execute_release_all_requests_hold_interrupt_before_actor_ack() {
    let (handle, mut rx) = ActionHandle::channel();
    let before_epoch = operator_release_epoch();
    println!("readback=release_interrupt edge=execute_release_all before_epoch={before_epoch}");

    let pending = tokio::spawn(async move { handle.execute(Action::ReleaseAll).await });
    let Some((action, ack, _operator_panic_epoch_at_enqueue)) = rx.recv().await else {
        panic!("release_all action should be enqueued");
    };

    assert!(matches!(action, Action::ReleaseAll));
    assert!(operator_release_requested_since(before_epoch));
    let after_epoch = operator_release_epoch();
    println!(
        "readback=release_interrupt edge=execute_release_all after_epoch={after_epoch} requested_since_before={}",
        operator_release_requested_since(before_epoch)
    );

    ack.send(Ok(()))
        .unwrap_or_else(|_error| panic!("release_all ack receiver should still be alive"));
    pending
        .await
        .unwrap_or_else(|error| panic!("release_all task should join: {error}"))
        .unwrap_or_else(|error| panic!("release_all should observe actor ack: {error}"));
}

fn key_named(value: &str) -> Key {
    Key {
        code: KeyCode::Named {
            value: value.to_owned(),
        },
        use_scancode: false,
    }
}
