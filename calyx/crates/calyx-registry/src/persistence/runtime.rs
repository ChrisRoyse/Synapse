use super::*;
use crate::drift::{CALYX_LENS_RUNTIME_DRIFT, DriftDecision, PROCESS_RUNTIME_GOLDEN_TOLERANCE};
use crate::lens::process_runtime_requires_golden;
use crate::runtime::batch_scope;
use crate::runtime_limit::runtime_uses_scoped_batch_limit;

#[derive(Clone)]
pub struct LoadedRegistrySnapshotLens {
    snapshot: RegistryLensSnapshot,
    runtime: Arc<dyn Lens>,
    runtime_load_ms: u128,
}

impl LoadedRegistrySnapshotLens {
    pub fn load(snapshot: RegistryLensSnapshot) -> Result<Self> {
        verify_registry_snapshot_contract(&snapshot)?;
        let load_start = Instant::now();
        let runtime = load_runtime_lens(&snapshot)?;
        let runtime_load_ms = load_start.elapsed().as_millis();
        Ok(Self {
            snapshot,
            runtime,
            runtime_load_ms,
        })
    }

    pub fn lens_id(&self) -> LensId {
        self.snapshot.lens_id
    }

    pub fn runtime_load_ms(&self) -> u128 {
        self.runtime_load_ms
    }

    pub fn measure_batch_with_stats(
        &self,
        inputs: &[Input],
        runtime_batch_limit: Option<usize>,
    ) -> Result<(Vec<SlotVector>, RegistrySnapshotMeasureStats)> {
        measure_loaded_snapshot_lens_batch_with_stats(
            &self.snapshot,
            self.runtime.as_ref(),
            inputs,
            runtime_batch_limit,
            0,
        )
    }
}

pub fn measure_registry_snapshot_lens_batch(
    snapshot: &RegistryLensSnapshot,
    inputs: &[Input],
) -> Result<Vec<SlotVector>> {
    let (vectors, _) = measure_registry_snapshot_lens_batch_with_stats(snapshot, inputs, None)?;
    Ok(vectors)
}

pub fn measure_registry_snapshot_lens_batch_with_stats(
    snapshot: &RegistryLensSnapshot,
    inputs: &[Input],
    runtime_batch_limit: Option<usize>,
) -> Result<(Vec<SlotVector>, RegistrySnapshotMeasureStats)> {
    let total_start = Instant::now();
    verify_registry_snapshot_inputs(snapshot, inputs)?;
    let load_start = Instant::now();
    let runtime = load_runtime_lens(snapshot)?;
    let runtime_load_ms = load_start.elapsed().as_millis();
    let (vectors, mut stats) = measure_loaded_snapshot_lens_batch_with_stats(
        snapshot,
        runtime.as_ref(),
        inputs,
        runtime_batch_limit,
        runtime_load_ms,
    )?;
    stats.total_ms = total_start.elapsed().as_millis();
    Ok((vectors, stats))
}

pub(super) fn verify_registry_snapshot_contract(snapshot: &RegistryLensSnapshot) -> Result<()> {
    if snapshot.lens_id != snapshot.contract.lens_id() {
        return Err(CalyxError::lens_frozen_violation(format!(
            "registry lens {} does not match frozen contract {}",
            snapshot.lens_id,
            snapshot.contract.lens_id()
        )));
    }
    Ok(())
}

fn verify_registry_snapshot_inputs(
    snapshot: &RegistryLensSnapshot,
    inputs: &[Input],
) -> Result<()> {
    verify_registry_snapshot_contract(snapshot)?;
    for input in inputs {
        if input.modality != snapshot.contract.modality() {
            return Err(CalyxError::lens_dim_mismatch(format!(
                "lens {} accepts {:?}, got {:?}",
                snapshot.lens_id,
                snapshot.contract.modality(),
                input.modality
            )));
        }
    }
    Ok(())
}

fn measure_loaded_snapshot_lens_batch_with_stats(
    snapshot: &RegistryLensSnapshot,
    runtime: &dyn Lens,
    inputs: &[Input],
    runtime_batch_limit: Option<usize>,
    runtime_load_ms: u128,
) -> Result<(Vec<SlotVector>, RegistrySnapshotMeasureStats)> {
    let total_start = Instant::now();
    verify_registry_snapshot_inputs(snapshot, inputs)?;
    let scoped_runtime_limit = runtime_uses_scoped_batch_limit(snapshot.spec.as_ref());
    let effective_chunk_size = if scoped_runtime_limit && !inputs.is_empty() {
        inputs.len()
    } else {
        effective_runtime_chunk_size(snapshot, inputs.len(), runtime_batch_limit)?
    };
    let chunk_count = if inputs.is_empty() {
        0
    } else {
        inputs.len().div_ceil(effective_chunk_size)
    };
    let measure_start = Instant::now();
    let mut vectors = Vec::with_capacity(inputs.len());
    if !inputs.is_empty() && scoped_runtime_limit {
        vectors = batch_scope::with_runtime_batch_limit(runtime_batch_limit, || {
            runtime.measure_batch(inputs)
        })?;
    } else if !inputs.is_empty() {
        for chunk in inputs.chunks(effective_chunk_size) {
            let chunk_vectors = runtime.measure_batch(chunk)?;
            if chunk_vectors.len() != chunk.len() {
                return Err(CalyxError::lens_dim_mismatch(format!(
                    "lens {} returned {} vectors for {} input chunk rows",
                    snapshot.lens_id,
                    chunk_vectors.len(),
                    chunk.len()
                )));
            }
            vectors.extend(chunk_vectors);
        }
    }
    let measure_ms = measure_start.elapsed().as_millis();
    if vectors.len() != inputs.len() {
        return Err(CalyxError::lens_dim_mismatch(format!(
            "lens {} returned {} vectors for {} inputs",
            snapshot.lens_id,
            vectors.len(),
            inputs.len()
        )));
    }
    for vector in &vectors {
        snapshot.contract.verify_vector(snapshot.lens_id, vector)?;
    }
    let stats = RegistrySnapshotMeasureStats {
        input_count: inputs.len(),
        runtime_batch_limit,
        effective_chunk_size,
        chunk_count,
        runtime_load_ms,
        measure_ms,
        total_ms: total_start.elapsed().as_millis(),
    };
    Ok((vectors, stats))
}

fn effective_runtime_chunk_size(
    snapshot: &RegistryLensSnapshot,
    input_count: usize,
    runtime_batch_limit: Option<usize>,
) -> Result<usize> {
    if runtime_batch_limit == Some(0) {
        return Err(CalyxError::lens_unreachable(
            "runtime batch limit must be > 0 when supplied",
        ));
    }
    let spec_limit = snapshot.spec.as_ref().and_then(|spec| spec.max_batch);
    if spec_limit == Some(0) {
        return Err(lens_config_invalid("LensSpec max_batch must be > 0"));
    }
    let limit = match (runtime_batch_limit, spec_limit) {
        (Some(runtime), Some(spec)) => runtime.min(spec),
        (Some(runtime), None) => runtime,
        (None, Some(spec)) => spec,
        (None, None) => input_count.max(1),
    };
    Ok(limit.max(1))
}

pub(super) fn load_runtime_lens(snapshot: &RegistryLensSnapshot) -> Result<Arc<dyn Lens>> {
    let spec = snapshot.spec.as_ref().ok_or_else(|| {
        CalyxError::lens_unreachable(format!(
            "persisted lens {} has no LensSpec, so its runtime cannot be reconstructed",
            snapshot.lens_id
        ))
    })?;
    let (lens, runtime_contract) = load_runtime_lens_from_spec(spec)?;
    if runtime_contract != snapshot.contract {
        return Err(CalyxError::lens_frozen_violation(format!(
            "persisted lens {} runtime contract drift: {}",
            snapshot.lens_id,
            contract_field_diffs("runtime", &snapshot.contract, &runtime_contract)
                .into_iter()
                .map(|field| format!(
                    "{} persisted={} reconstructed={}",
                    field.field, field.persisted, field.reconstructed
                ))
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }
    snapshot.contract.verify_registration(lens.as_ref())?;
    verify_runtime_golden(
        snapshot,
        lens.as_ref(),
        process_runtime_requires_golden(spec),
    )?;
    Ok(lens)
}

fn verify_runtime_golden(
    snapshot: &RegistryLensSnapshot,
    lens: &dyn Lens,
    required: bool,
) -> Result<()> {
    let Some(golden) = snapshot.runtime_golden.as_ref() else {
        if required {
            return Err(runtime_drift_error(format!(
                "persisted process runtime lens {} has no registration golden",
                snapshot.lens_id
            )));
        }
        return Ok(());
    };
    if golden.lens_id != snapshot.lens_id {
        return Err(runtime_drift_error(format!(
            "runtime golden lens {} does not match persisted lens {}",
            golden.lens_id, snapshot.lens_id
        )));
    }
    if golden.probe.modality != snapshot.contract.modality() {
        return Err(runtime_drift_error(format!(
            "runtime golden probe modality {:?} does not match persisted {:?}",
            golden.probe.modality,
            snapshot.contract.modality()
        )));
    }
    if !golden.tolerance.is_finite()
        || !(0.0..=PROCESS_RUNTIME_GOLDEN_TOLERANCE).contains(&golden.tolerance)
    {
        return Err(runtime_drift_error(format!(
            "runtime golden tolerance {} exceeds the process-runtime contract maximum {}",
            golden.tolerance, PROCESS_RUNTIME_GOLDEN_TOLERANCE
        )));
    }

    let observed = lens.measure(&golden.probe).map_err(|error| {
        runtime_drift_error(format!(
            "runtime golden probe for lens {} failed with {}: {}",
            snapshot.lens_id, error.code, error.message
        ))
    })?;
    snapshot
        .contract
        .verify_vector(snapshot.lens_id, &observed)
        .map_err(|error| {
            runtime_drift_error(format!(
                "runtime golden probe for lens {} violated its contract with {}: {}",
                snapshot.lens_id, error.code, error.message
            ))
        })?;
    let observed = observed.as_dense().ok_or_else(|| {
        runtime_drift_error(format!(
            "runtime golden probe for lens {} did not return dense output",
            snapshot.lens_id
        ))
    })?;

    match golden.evaluate(observed) {
        DriftDecision::Reuse { lens_id, .. } if lens_id == snapshot.lens_id => Ok(()),
        DriftDecision::Reuse { lens_id, .. } => Err(runtime_drift_error(format!(
            "runtime golden reused lens {lens_id} instead of persisted lens {}",
            snapshot.lens_id
        ))),
        DriftDecision::Drifted {
            new_lens_id,
            max_abs_delta,
            ..
        } => Err(runtime_drift_error(format!(
            "runtime behavior for lens {} drifted to {new_lens_id}; max_abs_delta={max_abs_delta}",
            snapshot.lens_id
        ))),
    }
}

fn runtime_drift_error(message: impl Into<String>) -> CalyxError {
    CalyxError {
        code: CALYX_LENS_RUNTIME_DRIFT,
        message: message.into(),
        remediation: "re-register the process-boundary lens and persist its new runtime golden",
    }
}

fn lens_config_invalid(message: impl Into<String>) -> CalyxError {
    CalyxError {
        code: "CALYX_LENS_CONFIG_INVALID",
        message: message.into(),
        remediation: "fix persisted LensSpec runtime fields or re-register the lens",
    }
}
