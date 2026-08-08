//! Field service verification for #2109: the Whisper STT execution provider.
//!
//! Four parts, all against real components on this host:
//!
//! 1. **policy** — every accepted and rejected `SYNAPSE_STT_BACKEND` spelling
//!    resolves the way the fail-closed policy says it should;
//! 2. **session** — a real ORT session is built for the whisper end-to-end
//!    graph on the requested provider through the production
//!    `synapse_models::ModelLoader` path, which is what registers the pinned
//!    ONNX Runtime Extensions operator library. The same eight input tensors
//!    `WhisperTinyStt::run_session` sends are sent here;
//! 3. **transcript** — the fixture transcript, printed so a CPU run and a CUDA
//!    run can be diffed byte for byte, plus per-run latency;
//! 4. **reservation** — the exact [`synapse_audio::stt_gpu_reservation_request`]
//!    a GPU-backed session makes is acquired against the live OS-wide Calyx
//!    ledger and read back out of the ledger while it is held.
//!
//! ```text
//! SYNAPSE_STT_FSV_WAV=<fixture.wav> SYNAPSE_STT_FSV_MODEL=<whisper-e2e.onnx> \
//! SYNAPSE_STT_FSV_BACKEND=cuda \
//!   cargo run --release -p synapse-audio --features cuda --example stt_gpu_ep_fsv
//! ```

use std::fmt::Debug;
use std::path::PathBuf;
use std::time::Instant;

use calyx_forge::vram::HostGpuReservationStore;
use ort::session::Session;
use ort::value::{PrimitiveTensorElementType, Tensor};
use serde_json::{Value, json};
use synapse_audio::{SttBackendPolicy, stt_backend_policy, stt_gpu_reservation_request};
use synapse_models::{
    ModelBackend, ModelDescriptor, ModelLoader, SessionHandle, WHISPER_TINY_INT8_ONNX_ID,
    sha256_file,
};

const RUNS: usize = 5;
const DEVICE_INDEX: u32 = 0;
/// Exact prompt the production `run_session` uses.
const EN_DECODER_PROMPT: [i32; 2] = [50_257, 50_362];

fn main() {
    if let Err(error) = run() {
        eprintln!("FSV FAILED: {error}");
        std::process::exit(1);
    }
}

#[allow(clippy::too_many_lines)]
fn run() -> Result<(), String> {
    policy_probe()?;

    let model = PathBuf::from(
        std::env::var("SYNAPSE_STT_FSV_MODEL")
            .map_err(|_| "set SYNAPSE_STT_FSV_MODEL to an ONNX graph on this host".to_owned())?,
    );
    // The descriptor id decides whether `ep.rs` registers the pinned ORT
    // Extensions operator library, so it is a knob here: the whisper id runs
    // the full eight-tensor inference, any other id proves only that the
    // provider builds a session on this host.
    let model_id = std::env::var("SYNAPSE_STT_FSV_MODEL_ID")
        .unwrap_or_else(|_| WHISPER_TINY_INT8_ONNX_ID.to_owned());
    let whisper_graph = model_id == WHISPER_TINY_INT8_ONNX_ID;
    let wav = std::env::var("SYNAPSE_STT_FSV_WAV").map(PathBuf::from).ok();
    if whisper_graph && wav.is_none() {
        return Err("set SYNAPSE_STT_FSV_WAV to a fixed WAV fixture".to_owned());
    }
    let backend = match std::env::var("SYNAPSE_STT_FSV_BACKEND")
        .unwrap_or_else(|_| "cpu".to_owned())
        .to_ascii_lowercase()
        .as_str()
    {
        "cuda" => ModelBackend::Cuda,
        "directml" => ModelBackend::DirectMl,
        "cpu" => ModelBackend::Cpu,
        other => {
            return Err(format!(
                "SYNAPSE_STT_FSV_BACKEND {other:?} is not a provider"
            ));
        }
    };
    let bytes = match wav.as_ref() {
        Some(path) => std::fs::read(path).map_err(|error| format!("read fixture: {error}"))?,
        None => Vec::new(),
    };

    let store = HostGpuReservationStore::from_env(DEVICE_INDEX)
        .map_err(|error| format!("open host GPU reservation ledger: {error}"))?;
    let before = store
        .readback()
        .map_err(|error| format!("ledger readback before: {error}"))?;

    // The reservation is acquired before the provider is allowed to allocate,
    // and released only after the session is dropped — the same ordering
    // `WhisperTinyStt` uses for the session it caches for the process lifetime.
    let reservation = if backend == ModelBackend::Cpu {
        None
    } else {
        Some(
            store
                .acquire(stt_gpu_reservation_request(backend))
                .map_err(|error| format!("host GPU admission refused the STT envelope: {error}"))?,
        )
    };

    let sha = sha256_file(&model).map_err(|error| format!("hash model: {error}"))?;
    let descriptor = ModelDescriptor {
        // The id is what makes `ep.rs` register the pinned ORT-Extensions
        // operator library, which is the thing most likely to break under a GPU
        // provider (issue ask 3).
        id: model_id.clone(),
        path: model.clone(),
        sha256: sha.clone(),
        input_shape: vec![1, 0],
        class_map: Vec::new(),
    };
    let started = Instant::now();
    let loaded = ModelLoader::new(vec![backend])
        .load(descriptor)
        .map_err(|error| format!("{} :: {error}", error.code()))?;
    let load_ms = elapsed_ms(started);
    let SessionHandle::Ort(session) = loaded.session() else {
        return Err("model loaded without an ORT session".to_owned());
    };
    let selected = loaded.selected_backend();

    let mut transcripts = Vec::with_capacity(RUNS);
    let mut latencies = Vec::with_capacity(RUNS);
    if whisper_graph {
        for _ in 0..RUNS {
            let started = Instant::now();
            let text = {
                let mut guard = session
                    .lock()
                    .map_err(|_| "ORT session lock poisoned".to_owned())?;
                transcribe(&mut guard, bytes.clone())?
            };
            latencies.push(elapsed_ms(started));
            transcripts.push(text);
        }
        if transcripts.iter().any(|text| text != &transcripts[0]) {
            return Err(format!(
                "the same fixture produced different transcripts across runs: {transcripts:?}"
            ));
        }
    }

    let pid = std::process::id();
    let during = store
        .readback()
        .map_err(|error| format!("ledger readback during: {error}"))?;
    let live_row = during
        .reservations
        .iter()
        .find(|row| row.pid == pid && row.owner == "synapse-audio-stt")
        .map_or(Value::Null, |row| {
            json!({
                "reservation_id": row.reservation_id,
                "owner": row.owner,
                "job_id": row.job_id,
                "command": row.command,
                "pid": row.pid,
                "requested_mib": row.requested_mib,
            })
        });
    if reservation.is_some() && live_row.is_null() {
        return Err("a GPU session is live but the ledger has no row for this pid".to_owned());
    }
    if reservation.is_none() && !live_row.is_null() {
        return Err("a CPU session holds a GPU ledger row".to_owned());
    }

    // Drop the session first, then release the lease, so the ledger row always
    // outlives the device memory it admitted.
    drop(loaded);
    let after = match reservation {
        Some(lease) => lease
            .release()
            .map_err(|error| format!("release STT reservation: {error}"))?,
        None => during.clone(),
    };

    println!(
        "{}",
        json!({
            "backend": format!("{backend:?}"),
            "selected_backend": format!("{selected:?}"),
            "model_id": model_id,
            "inference_ran": whisper_graph,
            "fixture": wav.as_ref().map(|path| path.display().to_string()),
            "fixture_bytes": bytes.len(),
            "model": model.display().to_string(),
            "model_sha256": sha,
            "session_load_ms": load_ms,
            "transcript": transcripts.first(),
            "transcript_chars": transcripts.first().map(|text: &String| text.chars().count()),
            "latencies_ms": latencies,
            "latency_ms_first": latencies.first(),
            "latency_ms_warm_mean": if latencies.len() > 1 {
                Some(
                    latencies[1..].iter().sum::<f64>()
                        / f64::from(u32::try_from(latencies.len() - 1).unwrap_or(1)),
                )
            } else {
                None
            },
            "ledger_row_live": live_row,
            "admitted_total_before": before.admitted_total,
            "admitted_total_during": during.admitted_total,
            "reserved_mib_before": before.reserved_mib,
            "reserved_mib_during": during.reserved_mib,
            "reserved_mib_after_release": after.reserved_mib,
            "declared_envelope_mib": synapse_audio::stt_gpu_admission_mib(),
        })
    );
    Ok(())
}

/// The provider policy must accept exactly the documented spellings and refuse
/// everything else rather than silently defaulting.
fn policy_probe() -> Result<(), String> {
    let cases: [(Option<&str>, Result<SttBackendPolicy, ()>); 8] = [
        (None, Ok(SttBackendPolicy::Auto)),
        (Some(""), Ok(SttBackendPolicy::Auto)),
        (Some("auto"), Ok(SttBackendPolicy::Auto)),
        (Some("AUTO"), Ok(SttBackendPolicy::Auto)),
        (
            Some("cuda"),
            Ok(SttBackendPolicy::Pinned(ModelBackend::Cuda)),
        ),
        (
            Some("directml"),
            Ok(SttBackendPolicy::Pinned(ModelBackend::DirectMl)),
        ),
        (Some("cpu"), Ok(SttBackendPolicy::Pinned(ModelBackend::Cpu))),
        (Some("gpu"), Err(())),
    ];
    for (value, expected) in cases {
        // SAFETY: single-threaded probe, before any session or thread exists.
        unsafe {
            match value {
                Some(value) => std::env::set_var("SYNAPSE_STT_BACKEND", value),
                None => std::env::remove_var("SYNAPSE_STT_BACKEND"),
            }
        }
        let actual = stt_backend_policy().map_err(|_| ());
        if actual != expected {
            return Err(format!(
                "policy drift for {value:?}: expected {expected:?}, got {actual:?}"
            ));
        }
    }
    // SAFETY: same single-threaded window.
    unsafe {
        std::env::remove_var("SYNAPSE_STT_BACKEND");
    }
    eprintln!("policy probe = 8/8 spellings resolved as specified");
    Ok(())
}

fn transcribe(session: &mut Session, bytes: Vec<u8>) -> Result<String, String> {
    let audio = tensor("audio_stream", [1, bytes.len()], bytes)?;
    let max_length = tensor("max_length", [1], vec![96_i32])?;
    let min_length = tensor("min_length", [1], vec![0_i32])?;
    let num_beams = tensor("num_beams", [1], vec![1_i32])?;
    let num_return_sequences = tensor("num_return_sequences", [1], vec![1_i32])?;
    let length_penalty = tensor("length_penalty", [1], vec![1.0_f32])?;
    let repetition_penalty = tensor("repetition_penalty", [1], vec![1.0_f32])?;
    let decoder_input_ids = tensor(
        "decoder_input_ids",
        [1, EN_DECODER_PROMPT.len()],
        EN_DECODER_PROMPT.to_vec(),
    )?;
    let outputs = session
        .run(ort::inputs! {
            "audio_stream" => audio,
            "max_length" => max_length,
            "min_length" => min_length,
            "num_beams" => num_beams,
            "num_return_sequences" => num_return_sequences,
            "length_penalty" => length_penalty,
            "repetition_penalty" => repetition_penalty,
            "decoder_input_ids" => decoder_input_ids,
        })
        .map_err(|error| format!("inference failed: {error}"))?;
    Ok(outputs
        .get("str")
        .ok_or("model did not return `str`")?
        .try_extract_strings()
        .map_err(|error| format!("output extraction failed: {error}"))?
        .1
        .into_iter()
        .next()
        .unwrap_or_default()
        .trim()
        .to_owned())
}

fn tensor<T, const N: usize>(
    name: &str,
    shape: [usize; N],
    data: Vec<T>,
) -> Result<Tensor<T>, String>
where
    T: PrimitiveTensorElementType + Debug + Clone + 'static,
{
    Tensor::from_array((shape, data.into_boxed_slice()))
        .map_err(|error| format!("failed to create {name} tensor: {error}"))
}

fn elapsed_ms(started: Instant) -> f64 {
    (started.elapsed().as_secs_f64() * 1_000_000.0).round() / 1_000.0
}
