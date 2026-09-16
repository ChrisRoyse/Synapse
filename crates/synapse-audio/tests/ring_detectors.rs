use std::{
    path::PathBuf,
    sync::{Arc, Mutex, MutexGuard},
};

use synapse_audio::{
    AudioEventSink, AudioRing,
    detectors::{DetectorProcessor, SharedDetectorState},
    ring::AudioFormat,
};
use synapse_core::Event;

#[test]
fn ring_tail_preserves_expected_frame_and_byte_counts() -> Result<(), Box<dyn std::error::Error>> {
    let ring = AudioRing::new(5);
    let format = AudioFormat {
        sample_rate_hz: 48_000,
        channels: 2,
    };
    ring.set_format(format);
    ring.push_interleaved(&vec![0.5; 48_000 * 2 * 3]);

    let window = ring.tail_seconds(2.0)?;

    assert_eq!(window.frames, 96_000);
    assert_eq!(window.samples.len(), 192_000);
    assert_eq!(window.pcm_i16_le().len(), 384_000);
    assert!(window.rms_db > -7.0);
    Ok(())
}

#[test]
fn ring_rejects_over_capacity_tail() {
    let ring = AudioRing::new(5);
    let error = match ring.tail_seconds(6.0) {
        Ok(window) => panic!("expected over-capacity tail to fail, got {window:?}"),
        Err(error) => error,
    };
    assert_eq!(
        error.code(),
        synapse_core::error_codes::AUDIO_LOOPBACK_INIT_FAILED
    );
}

#[test]
fn detectors_emit_loud_and_speech_events_for_synthetic_audio() {
    let events = Arc::new(Mutex::new(Vec::<Event>::new()));
    let sink_events = Arc::clone(&events);
    let sink: AudioEventSink = Arc::new(move |event| {
        lock_events(&sink_events).push(event);
    });
    let state = SharedDetectorState::default();
    let mut detector = DetectorProcessor::new(state.clone(), sink);
    let format = AudioFormat {
        sample_rate_hz: 48_000,
        channels: 2,
    };

    detector.process(&vec![0.0; 480 * 2], format);
    detector.process(&vec![0.9; 480 * 2], format);
    for _ in 0..50 {
        detector.process(&vec![0.0; 480 * 2], format);
    }

    let kinds = lock_events(&events)
        .iter()
        .map(|event| event.kind.clone())
        .collect::<Vec<_>>();
    assert!(kinds.iter().any(|kind| kind == "loud_transient"));
    assert!(kinds.iter().any(|kind| kind == "speech_started"));
    assert!(kinds.iter().any(|kind| kind == "speech_ended"));
    assert!(!state.snapshot().speech_active);
}

#[test]
fn loud_transient_fixture_emits_exactly_one_loud_event() -> Result<(), Box<dyn std::error::Error>> {
    let samples = read_pcm_s16_mono_16k(fixture_path("loud_transient_1s.wav"))?;
    let events = Arc::new(Mutex::new(Vec::<Event>::new()));
    let sink_events = Arc::clone(&events);
    let sink: AudioEventSink = Arc::new(move |event| {
        lock_events(&sink_events).push(event);
    });
    let mut detector = DetectorProcessor::new(SharedDetectorState::default(), sink);
    let format = AudioFormat {
        sample_rate_hz: 16_000,
        channels: 1,
    };

    for chunk in samples.chunks(160) {
        detector.process(chunk, format);
    }

    let (loud_count, loud_rms) = {
        let events_guard = lock_events(&events);
        let mut loud_count = 0;
        let mut loud_rms = -120.0;
        for event in events_guard
            .iter()
            .filter(|event| event.kind == "loud_transient")
        {
            loud_count += 1;
            loud_rms = event.data["rms_db"].as_f64().unwrap_or(-120.0);
        }
        drop(events_guard);
        (loud_count, loud_rms)
    };
    assert_eq!(loud_count, 1);
    assert!(loud_rms > -6.0);
    Ok(())
}

#[test]
fn sustained_loud_signal_emits_one_loud_transient_on_onset() {
    let events = Arc::new(Mutex::new(Vec::<Event>::new()));
    let sink_events = Arc::clone(&events);
    let sink: AudioEventSink = Arc::new(move |event| {
        lock_events(&sink_events).push(event);
    });
    let mut detector = DetectorProcessor::new(SharedDetectorState::default(), sink);
    let format = AudioFormat {
        sample_rate_hz: 48_000,
        channels: 2,
    };

    detector.process(&vec![0.0; 480 * 2], format);
    for _ in 0..20 {
        detector.process(&vec![0.5; 480 * 2], format);
    }

    let loud_count = lock_events(&events)
        .iter()
        .filter(|event| event.kind == "loud_transient")
        .count();
    assert_eq!(loud_count, 1);
}

fn lock_events(events: &Mutex<Vec<Event>>) -> MutexGuard<'_, Vec<Event>> {
    match events.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("tests")
        .join("fixtures")
        .join("audio")
        .join(name)
}

fn read_pcm_s16_mono_16k(path: PathBuf) -> Result<Vec<f32>, Box<dyn std::error::Error>> {
    let bytes = std::fs::read(path)?;
    if bytes.get(0..4) != Some(b"RIFF") || bytes.get(8..12) != Some(b"WAVE") {
        return Err("fixture is not a RIFF/WAVE file".into());
    }

    let mut cursor = 12;
    let mut saw_fmt = false;
    let mut data = None;
    while cursor + 8 <= bytes.len() {
        let id = &bytes[cursor..cursor + 4];
        let size = u32::from_le_bytes(bytes[cursor + 4..cursor + 8].try_into()?) as usize;
        let start = cursor + 8;
        let end = start.saturating_add(size);
        if end > bytes.len() {
            return Err("fixture has a truncated WAV chunk".into());
        }
        if id == b"fmt " {
            assert_eq!(u16::from_le_bytes(bytes[start..start + 2].try_into()?), 1);
            assert_eq!(
                u16::from_le_bytes(bytes[start + 2..start + 4].try_into()?),
                1
            );
            assert_eq!(
                u32::from_le_bytes(bytes[start + 4..start + 8].try_into()?),
                16_000
            );
            assert_eq!(
                u16::from_le_bytes(bytes[start + 14..start + 16].try_into()?),
                16
            );
            saw_fmt = true;
        } else if id == b"data" {
            data = Some(bytes[start..end].to_vec());
        }
        cursor = end + (size % 2);
    }

    let data = data.ok_or("fixture has no data chunk")?;
    assert!(saw_fmt);
    Ok(data
        .chunks_exact(2)
        .map(|pair| f32::from(i16::from_le_bytes([pair[0], pair[1]])) / f32::from(i16::MAX))
        .collect())
}
