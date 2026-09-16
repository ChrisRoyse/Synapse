use std::path::PathBuf;

use synapse_audio::{AudioFormat, AudioWindow, detectors::rms_db, direction::estimate_direction};

#[test]
fn panned_fixture_segments_estimate_expected_azimuths() -> TestResult {
    let (format, samples) = read_wav_fixture("pan_minus60_0_plus60.wav")?;
    assert_eq!(format.sample_rate_hz, 16_000);
    assert_eq!(format.channels, 2);

    for (segment, expected) in [(-60.0_f32), 0.0, 60.0].into_iter().enumerate() {
        let window = window_for_segment(format, &samples, segment, 1);
        let estimate = estimate_direction(&window);
        println!(
            "direction_fixture segment={segment} expected={expected:.1} after=azimuth:{:.2} confidence:{:.2}",
            estimate.azimuth_deg, estimate.confidence
        );
        assert!(
            (estimate.azimuth_deg - expected).abs() <= 15.0,
            "segment {segment} expected {expected}, got {estimate:?}"
        );
        assert!(
            estimate.confidence > 0.0,
            "segment {segment} confidence was zero"
        );
    }
    Ok(())
}

#[test]
fn mono_input_has_undefined_direction() {
    let window = AudioWindow {
        format: AudioFormat {
            sample_rate_hz: 16_000,
            channels: 1,
        },
        frames: 16_000,
        samples: sine_mono(16_000, 440.0, 0.25),
        rms_db: -12.0,
    };

    let estimate = estimate_direction(&window);
    println!(
        "direction_edge mono after=azimuth:{:.2} confidence:{:.2}",
        estimate.azimuth_deg, estimate.confidence
    );
    assert!(estimate.azimuth_deg.abs() <= f32::EPSILON);
    assert!(estimate.confidence.abs() <= f32::EPSILON);
}

#[test]
fn silence_has_undefined_direction() {
    let window = AudioWindow {
        format: AudioFormat {
            sample_rate_hz: 16_000,
            channels: 2,
        },
        frames: 16_000,
        samples: vec![0.0; 32_000],
        rms_db: -120.0,
    };

    let estimate = estimate_direction(&window);
    println!(
        "direction_edge silence after=azimuth:{:.2} confidence:{:.2}",
        estimate.azimuth_deg, estimate.confidence
    );
    assert!(estimate.azimuth_deg.abs() <= f32::EPSILON);
    assert!(estimate.confidence.abs() <= f32::EPSILON);
}

#[test]
#[allow(clippy::cast_precision_loss)]
fn ambient_music_like_stereo_has_low_nonzero_confidence() {
    let mut samples = Vec::with_capacity(32_000);
    for frame in 0..16_000 {
        let t = frame as f32 / 16_000.0;
        samples.push((2.0 * std::f32::consts::PI * 330.0 * t).sin() * 0.2);
        samples.push((2.0 * std::f32::consts::PI * 554.37 * t).sin() * 0.2);
    }
    let window = AudioWindow {
        format: AudioFormat {
            sample_rate_hz: 16_000,
            channels: 2,
        },
        frames: 16_000,
        rms_db: rms_db(&samples),
        samples,
    };

    let estimate = estimate_direction(&window);
    println!(
        "direction_edge ambient after=azimuth:{:.2} confidence:{:.2}",
        estimate.azimuth_deg, estimate.confidence
    );
    assert!(estimate.confidence > 0.0);
    assert!(estimate.confidence < 0.4, "{estimate:?}");
}

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

fn window_for_segment(
    format: AudioFormat,
    samples: &[f32],
    segment: usize,
    seconds: usize,
) -> AudioWindow {
    let channels = usize::from(format.channels);
    let frames = usize::try_from(format.sample_rate_hz).unwrap_or(usize::MAX) * seconds;
    let start = segment * frames * channels;
    let end = start + frames * channels;
    let samples = samples[start..end].to_vec();
    AudioWindow {
        format,
        frames,
        rms_db: rms_db(&samples),
        samples,
    }
}

#[allow(clippy::cast_precision_loss)]
fn sine_mono(frames: usize, hz: f32, amplitude: f32) -> Vec<f32> {
    (0..frames)
        .map(|frame| {
            let t = frame as f32 / 16_000.0;
            (2.0 * std::f32::consts::PI * hz * t).sin() * amplitude
        })
        .collect()
}

fn read_wav_fixture(name: &str) -> TestResult<(AudioFormat, Vec<f32>)> {
    let bytes = std::fs::read(fixture_path(name))?;
    if bytes.get(0..4) != Some(b"RIFF") || bytes.get(8..12) != Some(b"WAVE") {
        return Err("fixture is not a RIFF/WAVE file".into());
    }

    let mut cursor = 12;
    let mut format = None;
    let mut data = None;
    while cursor + 8 <= bytes.len() {
        let id = &bytes[cursor..cursor + 4];
        let size = u32::from_le_bytes(bytes[cursor + 4..cursor + 8].try_into()?) as usize;
        let start = cursor + 8;
        let end = start.saturating_add(size);
        if end > bytes.len() {
            return Err("fixture has a truncated WAV chunk".into());
        }
        match id {
            b"fmt " => {
                assert_eq!(u16::from_le_bytes(bytes[start..start + 2].try_into()?), 1);
                let channels = u16::from_le_bytes(bytes[start + 2..start + 4].try_into()?);
                let sample_rate_hz = u32::from_le_bytes(bytes[start + 4..start + 8].try_into()?);
                assert_eq!(
                    u16::from_le_bytes(bytes[start + 14..start + 16].try_into()?),
                    16
                );
                format = Some(AudioFormat {
                    sample_rate_hz,
                    channels,
                });
            }
            b"data" => data = Some(bytes[start..end].to_vec()),
            _ => {}
        }
        cursor = end + (size % 2);
    }

    let format = format.ok_or("fixture has no fmt chunk")?;
    let data = data.ok_or("fixture has no data chunk")?;
    let samples = data
        .chunks_exact(2)
        .map(|pair| f32::from(i16::from_le_bytes([pair[0], pair[1]])) / f32::from(i16::MAX))
        .collect();
    Ok((format, samples))
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
