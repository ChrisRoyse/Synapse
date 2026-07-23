use crate::group_split::GroupSplit;

use super::*;

pub(super) struct LogisticSummary {
    pub(super) bits: f32,
    pub(super) accuracy: f32,
    pub(super) solver_iterations: usize,
    pub(super) solver_final_relative_parameter_change: f32,
    pub(super) solver_converged: bool,
}

struct LogisticModel {
    weights: Vec<Vec<f32>>,
    bias: f32,
    iterations: usize,
    final_relative_parameter_change: f32,
}

pub(super) fn logistic_heldout_summary(
    blocks: &[LogisticBlock<'_>],
    labels: &[bool],
    split: &GroupSplit,
    block_scales: &[f32],
    seed: u64,
) -> Result<LogisticSummary> {
    if blocks.len() != block_scales.len() {
        return Err(CalyxError::assay_degenerate_input(format!(
            "logistic seed {seed} block count {} != conditioning scale count {}",
            blocks.len(),
            block_scales.len()
        )));
    }
    let model = fit_logistic(blocks, labels, &split.train, block_scales, seed)?;
    score_logistic(&model, blocks, labels, &split.test, block_scales, seed)
}

fn fit_logistic(
    blocks: &[LogisticBlock<'_>],
    labels: &[bool],
    rows: &[usize],
    block_scales: &[f32],
    seed: u64,
) -> Result<LogisticModel> {
    let mut weights = blocks
        .iter()
        .map(|block| vec![0.0; block.dim()])
        .collect::<Vec<_>>();
    let mut bias = 0.0;
    let n = rows.len().max(1) as f32;
    let learning_rate = logistic_learning_rate(blocks.len());
    let mut final_relative_parameter_change = f32::INFINITY;
    for iteration in 1..=LOGISTIC_MAX_ITERATIONS {
        let mut gradients = weights
            .iter()
            .map(|block| vec![0.0; block.len()])
            .collect::<Vec<_>>();
        let mut bias_grad = 0.0;
        for &row_idx in rows {
            let label = *labels.get(row_idx).ok_or_else(|| {
                CalyxError::assay_insufficient_samples(format!(
                    "logistic seed {seed} training row {row_idx} has no label"
                ))
            })?;
            let logit = blocked_dot(blocks, row_idx, &weights, block_scales)? + bias;
            if !logit.is_finite() {
                return Err(CalyxError::forge_numerical_invariant(format!(
                    "logistic seed {seed} iteration {iteration} training row {row_idx} produced non-finite logit"
                )));
            }
            let error = sigmoid(logit) - f32::from(label);
            for ((block, gradient), scale) in blocks.iter().zip(&mut gradients).zip(block_scales) {
                let row = &block.vectors[row_idx];
                for (slot, &value) in gradient.iter_mut().zip(row) {
                    *slot += error * (value / scale);
                    if !slot.is_finite() {
                        return Err(CalyxError::forge_numerical_invariant(format!(
                            "logistic seed {seed} iteration {iteration} lens block {} produced a non-finite gradient",
                            block.name
                        )));
                    }
                }
            }
            bias_grad += error;
            if !bias_grad.is_finite() {
                return Err(CalyxError::forge_numerical_invariant(format!(
                    "logistic seed {seed} iteration {iteration} produced a non-finite bias gradient"
                )));
            }
        }
        let bias_gradient = bias_grad / n;
        for ((block_weights, gradient), block) in weights.iter().zip(&mut gradients).zip(blocks) {
            for (&weight, grad) in block_weights.iter().zip(gradient) {
                *grad = *grad / n + LOGISTIC_L2 * weight;
                if !grad.is_finite() {
                    return Err(CalyxError::forge_numerical_invariant(format!(
                        "logistic seed {seed} iteration {iteration} lens block {} produced a non-finite regularized gradient",
                        block.name
                    )));
                }
            }
        }
        let mut max_parameter_change = 0.0_f32;
        let mut max_parameter_magnitude = 0.0_f32;
        for ((block_weights, gradient), block) in weights.iter_mut().zip(&gradients).zip(blocks) {
            for (weight, &grad) in block_weights.iter_mut().zip(gradient) {
                let change = learning_rate * grad;
                *weight -= change;
                if !weight.is_finite() {
                    return Err(CalyxError::forge_numerical_invariant(format!(
                        "logistic seed {seed} iteration {iteration} lens block {} produced a non-finite weight update",
                        block.name
                    )));
                }
                max_parameter_change = max_parameter_change.max(change.abs());
                max_parameter_magnitude = max_parameter_magnitude.max(weight.abs());
            }
        }
        let bias_change = learning_rate * bias_gradient;
        bias -= bias_change;
        if !bias.is_finite() {
            return Err(CalyxError::forge_numerical_invariant(format!(
                "logistic seed {seed} iteration {iteration} produced a non-finite bias update"
            )));
        }
        max_parameter_change = max_parameter_change.max(bias_change.abs());
        max_parameter_magnitude = max_parameter_magnitude.max(bias.abs());
        final_relative_parameter_change = if max_parameter_magnitude > 0.0 {
            max_parameter_change / max_parameter_magnitude
        } else if max_parameter_change == 0.0 {
            0.0
        } else {
            f32::INFINITY
        };
        if !final_relative_parameter_change.is_finite() {
            return Err(CalyxError::forge_numerical_invariant(format!(
                "logistic seed {seed} iteration {iteration} produced a non-finite relative parameter change"
            )));
        }
        let convergence_checkpoint = iteration % LOGISTIC_CONVERGENCE_CHECK_INTERVAL == 0
            || iteration == LOGISTIC_MAX_ITERATIONS;
        if convergence_checkpoint
            && final_relative_parameter_change <= LOGISTIC_RELATIVE_PARAMETER_TOLERANCE
        {
            return Ok(LogisticModel {
                weights,
                bias,
                iterations: iteration,
                final_relative_parameter_change,
            });
        }
    }
    Err(crate::calibration::underpowered(format!(
        "logistic seed {seed} did not converge: final max relative parameter change {final_relative_parameter_change:.9} exceeds tolerance {LOGISTIC_RELATIVE_PARAMETER_TOLERANCE:.9} after {LOGISTIC_MAX_ITERATIONS} iterations; lens_blocks={} total_features={}",
        blocks.len(),
        weights.iter().map(Vec::len).sum::<usize>()
    )))
}

fn score_logistic(
    model: &LogisticModel,
    blocks: &[LogisticBlock<'_>],
    labels: &[bool],
    rows: &[usize],
    block_scales: &[f32],
    seed: u64,
) -> Result<LogisticSummary> {
    let mut predictions = Vec::with_capacity(rows.len());
    let mut heldout_labels = Vec::with_capacity(rows.len());
    for &row_idx in rows {
        let label = *labels.get(row_idx).ok_or_else(|| {
            CalyxError::assay_insufficient_samples(format!(
                "logistic seed {seed} held-out row {row_idx} has no label"
            ))
        })?;
        let logit = blocked_dot(blocks, row_idx, &model.weights, block_scales)? + model.bias;
        if !logit.is_finite() {
            return Err(CalyxError::forge_numerical_invariant(format!(
                "logistic seed {seed} held-out row {row_idx} produced non-finite logit"
            )));
        }
        predictions.push(sigmoid(logit) >= 0.5);
        heldout_labels.push(label);
    }
    let accuracy = predictions
        .iter()
        .zip(&heldout_labels)
        .filter(|(prediction, label)| **prediction == **label)
        .count() as f32
        / heldout_labels.len().max(1) as f32;
    Ok(LogisticSummary {
        bits: binary_mi(&heldout_labels, &predictions),
        accuracy,
        solver_iterations: model.iterations,
        solver_final_relative_parameter_change: model.final_relative_parameter_change,
        solver_converged: true,
    })
}

fn blocked_dot(
    blocks: &[LogisticBlock<'_>],
    row_idx: usize,
    weights: &[Vec<f32>],
    block_scales: &[f32],
) -> Result<f32> {
    let mut sum = 0.0_f32;
    for (block_idx, ((block, block_weights), &scale)) in
        blocks.iter().zip(weights).zip(block_scales).enumerate()
    {
        let row = block.vectors.get(row_idx).ok_or_else(|| {
            CalyxError::assay_insufficient_samples(format!(
                "logistic lens block {} row {row_idx} is out of range",
                block.name
            ))
        })?;
        if row.len() != block_weights.len() {
            return Err(CalyxError::lens_dim_mismatch(format!(
                "logistic lens block {block_idx} {} row dim {} != weight dim {}",
                block.name,
                row.len(),
                block_weights.len()
            )));
        }
        for (&value, &weight) in row.iter().zip(block_weights) {
            sum += (value / scale) * weight;
        }
    }
    Ok(sum)
}

fn sigmoid(logit: f32) -> f32 {
    1.0 / (1.0 + (-logit.clamp(-40.0, 40.0)).exp())
}

pub(super) fn mean(values: &[f32]) -> f32 {
    values.iter().sum::<f32>() / values.len().max(1) as f32
}

pub(super) fn sample_sigma(values: &[f32]) -> f32 {
    if values.len() < 2 {
        return 0.0;
    }
    let mean = mean(values);
    let variance = values
        .iter()
        .map(|value| {
            let delta = *value - mean;
            delta * delta
        })
        .sum::<f32>()
        / (values.len() - 1) as f32;
    variance.sqrt()
}

pub(super) fn seed_ci(mean: f32, sigma: f32, n: usize) -> (f32, f32) {
    let t = match n.saturating_sub(1) {
        0 => 0.0,
        1 => 12.706,
        2 => 4.303,
        3 => 3.182,
        4 => 2.776,
        _ => 1.960,
    };
    let half_width = t * sigma / (n.max(1) as f32).sqrt();
    ((mean - half_width).max(0.0), mean + half_width)
}

pub(super) fn binary_mi(labels: &[bool], predictions: &[bool]) -> f32 {
    let n = labels.len().max(1) as f32;
    let mut joint = [[0.0_f32; 2]; 2];
    for (label, prediction) in labels.iter().zip(predictions) {
        joint[*label as usize][*prediction as usize] += 1.0;
    }
    let py = [
        (joint[0][0] + joint[0][1]) / n,
        (joint[1][0] + joint[1][1]) / n,
    ];
    let pp = [
        (joint[0][0] + joint[1][0]) / n,
        (joint[0][1] + joint[1][1]) / n,
    ];
    let mut mi = 0.0;
    for y in 0..2 {
        for p in 0..2 {
            let joint_p = joint[y][p] / n;
            if joint_p > 0.0 && py[y] > 0.0 && pp[p] > 0.0 {
                mi += joint_p * (joint_p / (py[y] * pp[p])).log2();
            }
        }
    }
    mi.max(0.0)
}
