use super::*;

#[allow(clippy::too_many_arguments)]
pub(super) fn push_slot_vector(
    plan: &SlotBuildPlan,
    cx_id: CxId,
    vector: SlotVector,
    shape: &mut Option<SlotRowShape>,
    dense_dim: &mut Option<u32>,
    sparse_dim: &mut Option<u32>,
    multi_token_dim: &mut Option<u32>,
    dense_rows: &mut Vec<(CxId, Vec<f32>)>,
    flat_dense_writer: &mut Option<dense::StreamingFlatWriter>,
    sparse_writer: &mut Option<sparse::StreamingWriter>,
    multi_writer: &mut Option<multi::StreamingSegmentsWriter>,
    vault_dir: &Path,
    root: &Path,
    base_seq: u64,
    dense_index_config: &super::super::super::PersistedDenseIndexConfig,
) -> CliResult<Option<multi::SegmentFlush>> {
    vector.validate_schema().map_err(|err| {
        stale(format!(
            "slot {} cx {cx_id} has invalid payload: {}",
            plan.slot, err.message
        ))
    })?;
    match vector {
        SlotVector::Dense { dim, data } => {
            require_shape(shape, SlotRowShape::Dense, plan.slot, cx_id)?;
            dense::validate_dense(plan.slot, cx_id, dim, &data)?;
            match *dense_dim {
                Some(expected_dim) if expected_dim != dim => {
                    return Err(stale(format!(
                        "slot {} has mixed dense dims: {expected_dim} and {dim}",
                        plan.slot
                    )));
                }
                None => *dense_dim = Some(dim),
                _ => {}
            }
            if dense_index_config.quant_bits_for(plan.slot) == 32 {
                if flat_dense_writer.is_none() {
                    *flat_dense_writer = Some(dense::StreamingFlatWriter::new(
                        vault_dir,
                        root,
                        plan.slot,
                        dim,
                        base_seq,
                        plan.expected_ids.len(),
                    )?);
                }
                flat_dense_writer
                    .as_mut()
                    .expect("flat dense writer initialized")
                    .push(cx_id, &data)?;
            } else {
                dense_rows.push((cx_id, data));
            }
            Ok(None)
        }
        SlotVector::Sparse { dim, entries } => {
            require_shape(shape, SlotRowShape::Sparse, plan.slot, cx_id)?;
            match *sparse_dim {
                Some(expected_dim) if expected_dim != dim => {
                    return Err(stale(format!(
                        "slot {} has mixed sparse dims: {expected_dim} and {dim}",
                        plan.slot
                    )));
                }
                None => *sparse_dim = Some(dim),
                _ => {}
            }
            let scoring = plan.sparse_scoring.ok_or_else(|| {
                stale(format!(
                    "slot {} contains sparse rows but the active panel/lens contract declares no sparse scoring mode",
                    plan.slot
                ))
            })?;
            if sparse_writer.is_none() {
                *sparse_writer = Some(sparse::StreamingWriter::new(
                    vault_dir,
                    root,
                    plan.slot,
                    dim,
                    base_seq,
                    plan.expected_ids.len(),
                    scoring,
                )?);
            }
            sparse_writer
                .as_mut()
                .expect("sparse writer initialized")
                .push(cx_id, &entries)?;
            Ok(None)
        }
        SlotVector::Multi { token_dim, tokens } => {
            require_shape(shape, SlotRowShape::Multi, plan.slot, cx_id)?;
            match *multi_token_dim {
                Some(expected_dim) if expected_dim != token_dim => {
                    return Err(stale(format!(
                        "slot {} has mixed multi token dims: {expected_dim} and {token_dim}",
                        plan.slot
                    )));
                }
                None => *multi_token_dim = Some(token_dim),
                _ => {}
            }
            if multi_writer.is_none() {
                *multi_writer = Some(multi::StreamingSegmentsWriter::new(
                    vault_dir, root, plan.slot, token_dim, base_seq,
                ));
            }
            multi_writer
                .as_mut()
                .expect("multi writer initialized")
                .push(cx_id, tokens)
        }
        SlotVector::Absent { .. } => Ok(None),
    }
}

pub(super) fn abort_flat_dense_writer(
    writer: &mut Option<dense::StreamingFlatWriter>,
    primary: crate::error::CliError,
) -> crate::error::CliError {
    let Some(writer) = writer.take() else {
        return primary;
    };
    match writer.abort() {
        Ok(()) => primary,
        Err(cleanup) => stale(format!(
            "slot scan failed [{}] {}; partial flat dense cleanup also failed [{}] {}",
            primary.code(),
            primary.message(),
            cleanup.code(),
            cleanup.message()
        )),
    }
}

pub(super) fn abort_sparse_writer(
    writer: &mut Option<sparse::StreamingWriter>,
    primary: crate::error::CliError,
) -> crate::error::CliError {
    let Some(writer) = writer.take() else {
        return primary;
    };
    match writer.abort() {
        Ok(()) => primary,
        Err(cleanup) => stale(format!(
            "slot scan failed [{}] {}; partial sparse row-stream cleanup also failed [{}] {}",
            primary.code(),
            primary.message(),
            cleanup.code(),
            cleanup.message()
        )),
    }
}
