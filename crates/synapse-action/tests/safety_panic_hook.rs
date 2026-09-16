use std::{
    error::Error,
    panic::{self, AssertUnwindSafe},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};

use synapse_action::{ActionHandle, RELEASE_ALL_HANDLE, install_panic_hook};
use synapse_core::Action;

#[test]
fn panic_hook_releases_before_prior_hook_and_installs_once() -> Result<(), Box<dyn Error>> {
    let (handle, mut action_rx) = ActionHandle::channel();
    assert!(
        RELEASE_ALL_HANDLE.set(handle).is_ok(),
        "RELEASE_ALL_HANDLE should be unset at integration-test process start"
    );

    let release_count = Arc::new(AtomicUsize::new(0));
    let previous_count = Arc::new(AtomicUsize::new(0));
    let previous_observed_release_count = Arc::new(AtomicUsize::new(0));
    let (event_tx, event_rx) = mpsc::channel::<String>();

    let release_count_for_actor = Arc::clone(&release_count);
    let _actor = thread::spawn(move || {
        while let Some((action, ack, _operator_panic_epoch_at_enqueue)) = action_rx.blocking_recv()
        {
            let ordinal = release_count_for_actor.fetch_add(1, Ordering::SeqCst) + 1;
            let action_label = if matches!(action, Action::ReleaseAll) {
                "release_all"
            } else {
                "unexpected"
            };
            let _send_result = event_tx.send(format!("ordinal:{ordinal} action:{action_label}"));
            if ack.send(Ok(())).is_err() {
                return;
            }
        }
    });

    let previous_count_for_hook = Arc::clone(&previous_count);
    let release_count_for_hook = Arc::clone(&release_count);
    let previous_observed_release_count_for_hook = Arc::clone(&previous_observed_release_count);
    panic::set_hook(Box::new(move |_info| {
        previous_observed_release_count_for_hook.store(
            release_count_for_hook.load(Ordering::SeqCst),
            Ordering::SeqCst,
        );
        previous_count_for_hook.fetch_add(1, Ordering::SeqCst);
    }));

    println!(
        "readback=panic_hook_queue edge=happy before=release_count:{} previous_count:{} prior_observed_release:{} install_calls:0",
        release_count.load(Ordering::SeqCst),
        previous_count.load(Ordering::SeqCst),
        previous_observed_release_count.load(Ordering::SeqCst)
    );

    install_panic_hook();
    install_panic_hook();
    let first_result = panic::catch_unwind(AssertUnwindSafe(|| {
        panic!("synthetic #179 happy panic");
    }));
    assert!(first_result.is_err());
    let first_event = event_rx.recv_timeout(Duration::from_secs(1))?;
    assert_eq!(first_event, "ordinal:1 action:release_all");
    assert_eq!(release_count.load(Ordering::SeqCst), 1);
    assert_eq!(previous_count.load(Ordering::SeqCst), 1);
    assert_eq!(previous_observed_release_count.load(Ordering::SeqCst), 1);
    println!(
        "readback=panic_hook_queue edge=happy after_event={first_event} after=release_count:{} previous_count:{} prior_observed_release:{} panic_caught:{}",
        release_count.load(Ordering::SeqCst),
        previous_count.load(Ordering::SeqCst),
        previous_observed_release_count.load(Ordering::SeqCst),
        first_result.is_err()
    );

    println!(
        "readback=panic_hook_queue edge=idempotent before=release_count:{} previous_count:{} prior_observed_release:{} install_calls:2",
        release_count.load(Ordering::SeqCst),
        previous_count.load(Ordering::SeqCst),
        previous_observed_release_count.load(Ordering::SeqCst)
    );

    install_panic_hook();
    install_panic_hook();
    let second_result = panic::catch_unwind(AssertUnwindSafe(|| {
        panic!("synthetic #179 idempotent panic");
    }));
    assert!(second_result.is_err());
    let second_event = event_rx.recv_timeout(Duration::from_secs(1))?;
    assert_eq!(second_event, "ordinal:2 action:release_all");
    assert_eq!(release_count.load(Ordering::SeqCst), 2);
    assert_eq!(previous_count.load(Ordering::SeqCst), 2);
    assert_eq!(previous_observed_release_count.load(Ordering::SeqCst), 2);
    println!(
        "readback=panic_hook_queue edge=idempotent after_event={second_event} after=release_count:{} previous_count:{} prior_observed_release:{} release_delta:1 previous_delta:1 panic_caught:{}",
        release_count.load(Ordering::SeqCst),
        previous_count.load(Ordering::SeqCst),
        previous_observed_release_count.load(Ordering::SeqCst),
        second_result.is_err()
    );

    Ok(())
}
