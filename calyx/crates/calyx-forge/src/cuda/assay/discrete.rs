use super::*;

/// Conservative dynamic-shared-memory ceiling. The kernel also owns twelve
/// KiB of fixed reduction state, so 8,192 u32 bins keep the complete block at
/// the portable 48 KiB shared-memory floor. Larger valid tables use explicitly
/// budgeted, batch-private global histograms; neither path changes estimators.
const SHARED_HISTOGRAM_BIN_CEILING: usize = 8_192;

/// Computes Miller-Madow corrected discrete transfer entropy for every
/// equal-length index selection on the CUDA device.
///
/// `code_tables` contains four dense tables in this exact order:
/// `(Yf,Yp)`, `(Xp,Yp)`, `Yp`, `(Yf,Yp,Xp)`. `bin_counts` declares the exact
/// dense alphabet of each table. Integer histogram atomics make counts exact;
/// the only floating-point reduction is a fixed per-block tree.
pub fn discrete_te_batch_host(
    ctx: &CudaContext,
    code_tables: &[u32],
    sample_count: usize,
    bin_counts: [usize; 4],
    selections: &[i32],
    selection_len: usize,
) -> Result<CudaDiscreteTeBatch> {
    validate_inputs(
        code_tables,
        sample_count,
        bin_counts,
        selections,
        selection_len,
    )?;
    let batch_count = selections.len() / selection_len;
    let total_bins = bin_counts.iter().try_fold(0usize, |sum, bins| {
        sum.checked_add(*bins)
            .ok_or_else(|| shape_overflow("discrete TE histogram bin-count overflow"))
    })?;
    let use_shared = total_bins <= SHARED_HISTOGRAM_BIN_CEILING;
    let histogram_cells = if use_shared {
        1
    } else {
        batch_count
            .checked_mul(total_bins)
            .ok_or_else(|| shape_overflow("discrete TE batch histogram size overflow"))?
    };
    ensure_device_room(
        ctx,
        "discrete_te_batch_host",
        checked_sum_bytes(&[
            bytes::<u32>(code_tables.len(), "discrete TE code tables")?,
            bytes::<i32>(selections.len(), "discrete TE selections")?,
            bytes::<u32>(histogram_cells, "discrete TE histograms")?,
            bytes::<f32>(batch_count, "discrete TE estimates")?,
        ])?,
    )?;

    let stream = ctx.inner().default_stream();
    let codes_dev = stream.clone_htod(code_tables).map_err(|err| {
        device_unavailable(ctx, format!("discrete TE code-table upload failed: {err}"))
    })?;
    let selections_dev = stream.clone_htod(selections).map_err(|err| {
        device_unavailable(ctx, format!("discrete TE selection upload failed: {err}"))
    })?;
    let mut histograms: CudaSlice<u32> = stream.alloc_zeros(histogram_cells).map_err(|err| {
        device_unavailable(
            ctx,
            format!("discrete TE histogram allocation failed: {err}"),
        )
    })?;
    let mut estimates: CudaSlice<f32> = stream.alloc_zeros(batch_count).map_err(|err| {
        device_unavailable(
            ctx,
            format!("discrete TE estimate allocation failed: {err}"),
        )
    })?;
    let mut flags = alloc_flags(ctx, "discrete_te_batch")?;
    let function = assay_function(
        ctx,
        "assay.discrete_te_batch_f32",
        "assay_discrete_te_batch_f32",
    )?;
    let sample_count_i32 = to_i32(sample_count, "discrete TE sample count")?;
    let selection_len_i32 = to_i32(selection_len, "discrete TE selection length")?;
    let batch_count_i32 = to_i32(batch_count, "discrete TE batch count")?;
    let bin_counts_i32 = [
        to_i32(bin_counts[0], "discrete TE future-past bins")?,
        to_i32(bin_counts[1], "discrete TE source-target-past bins")?,
        to_i32(bin_counts[2], "discrete TE own-past bins")?,
        to_i32(bin_counts[3], "discrete TE joint bins")?,
    ];
    let total_bins_i32 = to_i32(total_bins, "discrete TE total bins")?;
    let use_shared_i32 = i32::from(use_shared);
    let shared_mem_bytes = if use_shared {
        u32::try_from(bytes::<u32>(total_bins, "discrete TE shared histogram")?)
            .map_err(|_| shape_overflow("discrete TE shared-memory byte count exceeds u32"))?
    } else {
        0
    };
    let config = LaunchConfig {
        grid_dim: (to_u32(batch_count, "discrete TE batch grid")?, 1, 1),
        block_dim: (THREADS, 1, 1),
        shared_mem_bytes,
    };
    let mut launch = stream.launch_builder(function.as_ref());
    unsafe {
        launch
            .arg(&codes_dev)
            .arg(&selections_dev)
            .arg(&sample_count_i32)
            .arg(&selection_len_i32)
            .arg(&batch_count_i32)
            .arg(&bin_counts_i32[0])
            .arg(&bin_counts_i32[1])
            .arg(&bin_counts_i32[2])
            .arg(&bin_counts_i32[3])
            .arg(&total_bins_i32)
            .arg(&use_shared_i32)
            .arg(&mut histograms)
            .arg(&mut estimates)
            .arg(&mut flags)
            .launch(config)
    }
    .map_err(|err| {
        device_unavailable(ctx, format!("discrete TE histogram launch failed: {err}"))
    })?;
    sync_and_decode(ctx, "discrete_te_batch", &flags)?;
    let estimates = read_f32(ctx, &estimates, "discrete TE estimates")?;
    if estimates.iter().any(|value| !value.is_finite()) {
        return Err(numerical(
            "discrete_te_batch",
            "CUDA readback contained a non-finite transfer-entropy estimate".to_string(),
        ));
    }
    Ok(CudaDiscreteTeBatch { estimates })
}

fn validate_inputs(
    code_tables: &[u32],
    sample_count: usize,
    bin_counts: [usize; 4],
    selections: &[i32],
    selection_len: usize,
) -> Result<()> {
    if sample_count == 0 || selection_len == 0 || selections.is_empty() {
        return Err(ForgeError::ShapeMismatch {
            expected: vec![1, 1, 1],
            got: vec![sample_count, selection_len, selections.len()],
            remediation:
                "discrete TE requires non-empty samples and non-empty equal-length selections"
                    .to_string(),
        });
    }
    let expected_codes = sample_count
        .checked_mul(4)
        .ok_or_else(|| shape_overflow("discrete TE code-table length overflow"))?;
    if code_tables.len() != expected_codes || !selections.len().is_multiple_of(selection_len) {
        return Err(ForgeError::ShapeMismatch {
            expected: vec![expected_codes, selection_len],
            got: vec![code_tables.len(), selections.len()],
            remediation:
                "pass four sample-aligned code tables and complete fixed-length selection rows"
                    .to_string(),
        });
    }
    if bin_counts.contains(&0) {
        return Err(ForgeError::ShapeMismatch {
            expected: vec![1, 1, 1, 1],
            got: bin_counts.to_vec(),
            remediation: "every discrete TE entropy table needs at least one dense symbol bin"
                .to_string(),
        });
    }
    for (table, &bins) in bin_counts.iter().enumerate() {
        let start = table * sample_count;
        for (row, &code) in code_tables[start..start + sample_count].iter().enumerate() {
            if code as usize >= bins {
                return Err(ForgeError::ShapeMismatch {
                    expected: vec![bins.saturating_sub(1)],
                    got: vec![code as usize],
                    remediation: format!(
                        "discrete TE table {table} row {row} is outside its declared dense alphabet"
                    ),
                });
            }
        }
    }
    for (position, &index) in selections.iter().enumerate() {
        if index < 0 || index as usize >= sample_count {
            return Err(ForgeError::ShapeMismatch {
                expected: vec![sample_count.saturating_sub(1)],
                got: vec![usize::try_from(index).unwrap_or(usize::MAX)],
                remediation: format!(
                    "discrete TE selection position {position} references a sample outside the source table"
                ),
            });
        }
    }
    Ok(())
}
