use std::{
    panic::{self, AssertUnwindSafe},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use synapse_action::{RELEASE_ALL_HANDLE, install_panic_hook};

#[test]
fn panic_hook_without_release_handle_preserves_previous_hook() {
    assert!(RELEASE_ALL_HANDLE.get().is_none());
    let previous_count = Arc::new(AtomicUsize::new(0));
    let previous_count_for_hook = Arc::clone(&previous_count);
    panic::set_hook(Box::new(move |_info| {
        previous_count_for_hook.fetch_add(1, Ordering::SeqCst);
    }));

    println!(
        "readback=panic_hook_global edge=no_handle before=handle_present:{} previous_count:{}",
        RELEASE_ALL_HANDLE.get().is_some(),
        previous_count.load(Ordering::SeqCst)
    );

    install_panic_hook();
    let result = panic::catch_unwind(AssertUnwindSafe(|| {
        panic!("synthetic #179 no handle panic");
    }));
    assert!(result.is_err());
    assert!(RELEASE_ALL_HANDLE.get().is_none());
    assert_eq!(previous_count.load(Ordering::SeqCst), 1);

    println!(
        "readback=panic_hook_global edge=no_handle after=handle_present:{} previous_count:{} panic_caught:{}",
        RELEASE_ALL_HANDLE.get().is_some(),
        previous_count.load(Ordering::SeqCst),
        result.is_err()
    );
}
