use std::sync::{Arc, RwLock};

use super::*;
use crate::BackendResolutionPolicy;

#[tokio::test]
async fn actor_uses_profile_backend_resolution_policy_for_auto_keydown() {
    let cancel = CancellationToken::new();
    let recording = Arc::new(RecordingBackend::new());
    let backend = Arc::clone(&recording) as Arc<dyn ActionBackend>;
    let policy = Arc::new(RwLock::new(BackendResolutionPolicy {
        default_backend: Backend::Hardware,
        keyboard_default: Backend::Auto,
        mouse_default: Backend::Auto,
        pad_default: Backend::Auto,
    }));
    let (handle, snapshot_handle, emitter) =
        ActionEmitter::channel_with_backends_and_policy(Backends::all_routed_to(backend), policy);
    let join = tokio::spawn(emitter.run(cancel.clone()));
    let key = key_named("profile-hardware");

    let before = snapshot_or_panic(&snapshot_handle).await;
    println!(
        "readback=actor_backend_resolution edge=before snapshot={before:?} backend_held={:?}",
        recording.held_keys()
    );
    assert!(before.held_keys.is_empty());

    handle
        .execute(Action::KeyDown {
            key: key.clone(),
            backend: Backend::Auto,
        })
        .await
        .unwrap_or_else(|error| {
            panic!("auto keydown should execute through recording backend: {error}")
        });

    let after = snapshot_or_panic(&snapshot_handle).await;
    println!(
        "readback=actor_backend_resolution edge=after_auto_keydown snapshot={after:?} backend_held={:?}",
        recording.held_keys()
    );
    assert!(
        after
            .held_keys_by_backend
            .get(&ResolvedBackend::Hardware)
            .is_some_and(|keys| keys.contains(&key))
    );
    assert!(
        !after
            .held_keys_by_backend
            .contains_key(&ResolvedBackend::Software)
    );
    assert!(recording.held_keys().contains(&key.code));

    cancel.cancel();
    let final_snapshot = join_actor_or_panic(join).await;
    assert!(final_snapshot.held_keys.is_empty());
}
