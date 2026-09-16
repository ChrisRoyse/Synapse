use std::path::PathBuf;

use synapse_audio::{
    AudioConfig, AudioFormat, AudioRuntime, AudioWindow, TranscriptionConfidenceSource,
    WhisperTinyStt, stt::WHISPER_TINY_INT8_SHA256,
};
use synapse_core::error_codes;

#[test]
fn stt_missing_model_maps_audio_code() -> TestResult {
    let runtime = AudioRuntime::spawn(AudioConfig {
        stt_model_path: Some(PathBuf::from("missing-whisper-tiny-int8.onnx")),
        ..AudioConfig::default()
    })?;

    let err = match runtime.transcribe_file(fixture_path(), "en") {
        Ok(transcription) => {
            return Err(format!("missing model returned {transcription:?}").into());
        }
        Err(err) => err,
    };

    assert_eq!(err.code(), error_codes::AUDIO_STT_MODEL_NOT_LOADED);
    Ok(())
}

#[test]
fn stt_corrupt_model_maps_hash_mismatch() -> TestResult {
    let temp = tempfile::tempdir()?;
    let model = temp.path().join("whisper-tiny-int8.onnx");
    std::fs::write(&model, b"not an onnx model")?;
    let runtime = AudioRuntime::spawn(AudioConfig {
        stt_model_path: Some(model),
        ..AudioConfig::default()
    })?;

    let err = match runtime.transcribe_file(fixture_path(), "en") {
        Ok(transcription) => {
            return Err(format!("corrupt model returned {transcription:?}").into());
        }
        Err(err) => err,
    };

    assert_eq!(err.code(), error_codes::MODEL_HASH_MISMATCH);
    assert!(err.to_string().contains(WHISPER_TINY_INT8_SHA256));
    Ok(())
}

#[test]
fn empty_and_silence_windows_return_blank_without_model_load() -> TestResult {
    let stt = WhisperTinyStt::new(Some(PathBuf::from("missing-whisper-tiny-int8.onnx")));

    let empty = stt.transcribe_window(&window(Vec::new(), 0), "en")?;
    let silence = stt.transcribe_window(&window(vec![0.0; 16_000], 16_000), "en")?;

    assert_eq!(empty.text, "");
    assert!(empty.confidence.abs() <= f32::EPSILON);
    assert_eq!(
        empty.confidence_source,
        TranscriptionConfidenceSource::NotApplicable
    );
    assert_eq!(silence.text, "");
    assert!(silence.confidence.abs() <= f32::EPSILON);
    assert_eq!(
        silence.confidence_source,
        TranscriptionConfidenceSource::NotApplicable
    );
    assert!(!stt.is_loaded());
    Ok(())
}

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("tests")
        .join("fixtures")
        .join("audio")
        .join("hello_world_5s.wav")
}

#[allow(clippy::missing_const_for_fn)]
fn window(samples: Vec<f32>, frames: usize) -> AudioWindow {
    AudioWindow {
        format: AudioFormat {
            sample_rate_hz: 16_000,
            channels: 1,
        },
        frames,
        samples,
        rms_db: -120.0,
    }
}
