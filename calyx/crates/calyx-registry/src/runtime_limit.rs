use std::collections::BTreeMap;

use calyx_core::{CalyxError, Input, LensId, Result, SlotVector};

use crate::Registry;
use crate::runtime::batch_scope;
use crate::spec::LensRuntime;

pub fn measure_registry_batch_with_runtime_limit(
    registry: &Registry,
    lens_id: LensId,
    inputs: &[Input],
    runtime_batch_limit: Option<usize>,
) -> Result<Vec<SlotVector>> {
    if runtime_batch_limit == Some(0) {
        return Err(CalyxError::lens_unreachable(
            "runtime batch limit must be > 0 when supplied",
        ));
    }
    if runtime_uses_scoped_batch_limit(registry.lens_spec(lens_id)) {
        return batch_scope::with_runtime_batch_limit(runtime_batch_limit, || {
            registry.measure_batch(lens_id, inputs)
        });
    }
    let Some(limit) = runtime_batch_limit else {
        return registry.measure_batch(lens_id, inputs);
    };
    let mut out = Vec::with_capacity(inputs.len());
    for chunk in inputs.chunks(limit) {
        out.extend(registry.measure_batch(lens_id, chunk)?);
    }
    Ok(out)
}

/// Measures a compatible group while applying one shared chunk boundary to
/// every requested output. Each chunk is exactly one grouped forward pass.
pub fn measure_registry_group_with_runtime_limit(
    registry: &Registry,
    lens_ids: &[LensId],
    inputs: &[Input],
    runtime_batch_limit: Option<usize>,
) -> Result<BTreeMap<LensId, Vec<SlotVector>>> {
    if runtime_batch_limit == Some(0) {
        return Err(CalyxError::lens_unreachable(
            "runtime batch limit must be > 0 when supplied",
        ));
    }
    if lens_ids.is_empty() {
        return Ok(BTreeMap::new());
    }
    let mut out: BTreeMap<LensId, Vec<SlotVector>> = lens_ids
        .iter()
        .copied()
        .map(|lens_id| (lens_id, Vec::with_capacity(inputs.len())))
        .collect();
    if out.len() != lens_ids.len() {
        return Err(CalyxError::lens_dim_mismatch(
            "grouped measurement lens ids must be unique",
        ));
    }
    let chunk_size = runtime_batch_limit.unwrap_or_else(|| inputs.len().max(1));
    for chunk in inputs.chunks(chunk_size) {
        let measured = registry.measure_grouped_batch(lens_ids, chunk)?;
        for &lens_id in lens_ids {
            let vectors = measured.get(&lens_id).ok_or_else(|| {
                CalyxError::lens_dim_mismatch(format!(
                    "grouped runtime omitted lens {lens_id} for a chunk"
                ))
            })?;
            out.get_mut(&lens_id)
                .expect("grouped output initialized for every unique lens")
                .extend(vectors.iter().cloned());
        }
    }
    Ok(out)
}

pub(crate) fn runtime_uses_scoped_batch_limit(spec: Option<&crate::LensSpec>) -> bool {
    matches!(
        spec.map(|spec| &spec.runtime),
        Some(LensRuntime::Onnx { .. } | LensRuntime::OnnxColbert { .. })
    )
}
