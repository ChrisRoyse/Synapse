use synapse_audio::{
    AudioConfig, AudioError, AudioRuntime, DEFAULT_RING_SECONDS, MAX_RING_SECONDS,
};
use synapse_core::error_codes;

#[test]
fn default_spawn_keeps_audio_paths_stopped() -> Result<(), AudioError> {
    let runtime = AudioRuntime::spawn(AudioConfig::default())?;

    assert_eq!(runtime.config().ring_seconds, DEFAULT_RING_SECONDS);
    assert!(!runtime.loopback_started());
    assert!(!runtime.detectors_started());
    assert_eq!(runtime.tail_seconds(0.0)?.samples.len(), 0);
    Ok(())
}

#[test]
fn invalid_ring_seconds_fail_closed() {
    let zero = spawn_error(AudioConfig {
        ring_seconds: 0,
        ..AudioConfig::default()
    });
    assert_eq!(zero.code(), error_codes::AUDIO_LOOPBACK_INIT_FAILED);

    let too_large = spawn_error(AudioConfig {
        ring_seconds: MAX_RING_SECONDS.saturating_add(1),
        ..AudioConfig::default()
    });
    assert_eq!(too_large.code(), error_codes::AUDIO_LOOPBACK_INIT_FAILED);
}

#[test]
fn detectors_without_loopback_fail_closed() {
    let detectors = spawn_error(AudioConfig {
        detectors_enabled: true,
        ..AudioConfig::default()
    });
    assert_eq!(detectors.code(), error_codes::AUDIO_LOOPBACK_INIT_FAILED);
}

fn spawn_error(config: AudioConfig) -> AudioError {
    match AudioRuntime::spawn(config) {
        Ok(_runtime) => panic!("expected AudioRuntime::spawn to fail"),
        Err(error) => error,
    }
}
