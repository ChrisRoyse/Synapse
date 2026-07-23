use calyx_core::{CalyxError, Result};

use super::plan::{LINEAR_CKA_JACKKNIFE_BLOCKS, LinearCkaTuplePlan};
use crate::ensemble::model::LinearCkaEstimate;

const GATE_SE_MULTIPLIER: f64 = 4.0;
const ROUNDING_TOLERANCE: f64 = 1.0e-9;

#[derive(Clone, Debug)]
pub struct LinearCkaSketch {
    plan_digest: [u8; 32],
    values: Vec<[f64; 3]>,
}

impl LinearCkaSketch {
    pub(super) fn matches(&self, plan: &LinearCkaTuplePlan) -> bool {
        self.plan_digest == plan.digest && self.values.len() == plan.tuples.len()
    }
}

pub fn linear_cka_sketch_from_rows(
    plan: &LinearCkaTuplePlan,
    rows: &[Vec<f32>],
) -> Result<LinearCkaSketch> {
    if rows.len() != plan.row_count {
        return Err(CalyxError::assay_insufficient_samples(format!(
            "linear CKA rows {} != tuple-plan rows {}",
            rows.len(),
            plan.row_count
        )));
    }
    let dimension = rows.first().map(Vec::len).unwrap_or(0);
    let mut energy = CenteredEnergy::new(dimension)?;
    for (index, row) in rows.iter().enumerate() {
        energy.observe(row, index)?;
    }
    let inverse_energy = energy.inverse()?;
    let mut values = Vec::with_capacity(plan.tuples.len());
    for tuple in &plan.tuples {
        values.push(tuple_z(
            [
                &rows[tuple[0]],
                &rows[tuple[1]],
                &rows[tuple[2]],
                &rows[tuple[3]],
            ],
            inverse_energy,
        )?);
    }
    Ok(LinearCkaSketch {
        plan_digest: plan.digest,
        values,
    })
}

pub fn linear_cka_sketch_from_row_fn<F>(
    plan: &LinearCkaTuplePlan,
    dimension: usize,
    mut row_at: F,
) -> Result<LinearCkaSketch>
where
    F: FnMut(usize) -> Vec<f32>,
{
    let mut energy = CenteredEnergy::new(dimension)?;
    for index in 0..plan.row_count {
        energy.observe(&row_at(index), index)?;
    }
    let inverse_energy = energy.inverse()?;
    let mut values = Vec::with_capacity(plan.tuples.len());
    for tuple in &plan.tuples {
        let rows = [
            row_at(tuple[0]),
            row_at(tuple[1]),
            row_at(tuple[2]),
            row_at(tuple[3]),
        ];
        for (position, row) in rows.iter().enumerate() {
            validate_row(row, dimension, tuple[position])?;
        }
        values.push(tuple_z(
            [&rows[0], &rows[1], &rows[2], &rows[3]],
            inverse_energy,
        )?);
    }
    Ok(LinearCkaSketch {
        plan_digest: plan.digest,
        values,
    })
}

pub(super) fn estimate_pair(
    left: &LinearCkaSketch,
    right: &LinearCkaSketch,
    exact: bool,
) -> Result<LinearCkaEstimate> {
    if left.plan_digest != right.plan_digest || left.values.len() != right.values.len() {
        return Err(CalyxError::assay_degenerate_input(
            "linear CKA sketches must share one tuple plan",
        ));
    }
    let mut total = MomentSums::default();
    let mut blocks = [MomentSums::default(); LINEAR_CKA_JACKKNIFE_BLOCKS];
    for (index, (a, b)) in left.values.iter().zip(&right.values).enumerate() {
        let cross = dot3(a, b);
        let self_a = dot3(a, a);
        let self_b = dot3(b, b);
        total.add(cross, self_a, self_b);
        let block = index * LINEAR_CKA_JACKKNIFE_BLOCKS / left.values.len();
        blocks[block].add(cross, self_a, self_b);
    }
    let raw = checked_cosine(total.values())?;
    if !(-1.0 - ROUNDING_TOLERANCE..=1.0 + ROUNDING_TOLERANCE).contains(&raw) {
        return Err(CalyxError::assay_degenerate_input(format!(
            "linear CKA escaped [-1,1]: {raw}"
        )));
    }
    let raw = raw.clamp(-1.0, 1.0);
    let point = raw.max(0.0);
    let standard_error = if exact {
        0.0
    } else {
        delete_block_standard_error(total.values(), &blocks).unwrap_or(1.0)
    };
    let gate = (point + GATE_SE_MULTIPLIER * standard_error).min(1.0);
    Ok(LinearCkaEstimate {
        raw_signed_point: raw as f32,
        redundancy_point: point as f32,
        mc_standard_error: standard_error as f32,
        mc_gate_upper_estimate: gate as f32,
    })
}

fn delete_block_standard_error(
    total: (f64, f64, f64),
    blocks: &[MomentSums; LINEAR_CKA_JACKKNIFE_BLOCKS],
) -> Option<f64> {
    let mut leave_out = Vec::with_capacity(blocks.len());
    for block in blocks {
        let values = block.values();
        leave_out.push(
            checked_cosine((total.0 - values.0, total.1 - values.1, total.2 - values.2)).ok()?,
        );
    }
    let mean = leave_out.iter().sum::<f64>() / leave_out.len() as f64;
    let squared = leave_out
        .iter()
        .map(|value| (value - mean).powi(2))
        .sum::<f64>();
    let variance = (leave_out.len() - 1) as f64 / leave_out.len() as f64 * squared;
    variance.is_finite().then(|| variance.max(0.0).sqrt())
}

fn checked_cosine((cross, self_a, self_b): (f64, f64, f64)) -> Result<f64> {
    if !cross.is_finite() || !self_a.is_finite() || !self_b.is_finite() {
        return Err(CalyxError::forge_numerical_invariant(
            "linear CKA accumulated non-finite energy",
        ));
    }
    if self_a <= 0.0 || self_b <= 0.0 {
        return Err(CalyxError::assay_degenerate_input(
            "linear CKA has unresolved self energy",
        ));
    }
    let value = cross / (self_a * self_b).sqrt();
    if !value.is_finite() {
        return Err(CalyxError::forge_numerical_invariant(
            "linear CKA normalization was non-finite",
        ));
    }
    Ok(value)
}

#[derive(Clone, Copy, Debug, Default)]
struct MomentSums {
    cross: CompensatedSum,
    self_a: CompensatedSum,
    self_b: CompensatedSum,
}

impl MomentSums {
    fn add(&mut self, cross: f64, self_a: f64, self_b: f64) {
        self.cross.add(cross);
        self.self_a.add(self_a);
        self.self_b.add(self_b);
    }

    fn values(self) -> (f64, f64, f64) {
        (self.cross.total(), self.self_a.total(), self.self_b.total())
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct CompensatedSum {
    sum: f64,
    correction: f64,
}

impl CompensatedSum {
    fn add(&mut self, value: f64) {
        let next = self.sum + value;
        self.correction += if self.sum.abs() >= value.abs() {
            (self.sum - next) + value
        } else {
            (value - next) + self.sum
        };
        self.sum = next;
    }

    fn total(self) -> f64 {
        self.sum + self.correction
    }
}

struct CenteredEnergy {
    count: usize,
    mean: Vec<f64>,
    m2: Vec<f64>,
}

impl CenteredEnergy {
    fn new(dimension: usize) -> Result<Self> {
        if dimension == 0 {
            return Err(CalyxError::assay_insufficient_samples(
                "linear CKA vectors must not be empty",
            ));
        }
        Ok(Self {
            count: 0,
            mean: vec![0.0; dimension],
            m2: vec![0.0; dimension],
        })
    }

    fn observe(&mut self, row: &[f32], index: usize) -> Result<()> {
        validate_row(row, self.mean.len(), index)?;
        self.count += 1;
        let count = self.count as f64;
        for ((mean, m2), value) in self.mean.iter_mut().zip(&mut self.m2).zip(row) {
            let value = f64::from(*value);
            let delta = value - *mean;
            *mean += delta / count;
            *m2 += delta * (value - *mean);
        }
        Ok(())
    }

    fn inverse(&self) -> Result<f64> {
        let mut energy = CompensatedSum::default();
        for value in &self.m2 {
            energy.add(*value);
        }
        let energy = energy.total();
        if !energy.is_finite() {
            return Err(CalyxError::forge_numerical_invariant(
                "linear CKA centered energy was non-finite",
            ));
        }
        if energy <= 0.0 {
            return Err(CalyxError::assay_degenerate_input(
                "linear CKA representation has zero centered energy",
            ));
        }
        Ok(1.0 / energy)
    }
}

fn validate_row(row: &[f32], dimension: usize, index: usize) -> Result<()> {
    if row.len() != dimension {
        return Err(CalyxError::assay_insufficient_samples(format!(
            "linear CKA row {index} dim {} != {dimension}",
            row.len()
        )));
    }
    if row.iter().any(|value| !value.is_finite()) {
        return Err(CalyxError::forge_numerical_invariant(format!(
            "linear CKA row {index} contains non-finite values"
        )));
    }
    Ok(())
}

pub(super) fn tuple_z(rows: [&[f32]; 4], inverse_energy: f64) -> Result<[f64; 3]> {
    let mut r = CompensatedSum::default();
    let mut s = CompensatedSum::default();
    for (column, &a) in rows[0].iter().enumerate() {
        let a = f64::from(a);
        let b = f64::from(rows[1][column]);
        let c = f64::from(rows[2][column]);
        let d = f64::from(rows[3][column]);
        r.add((a - d) * (b - c));
        s.add((a - c) * (b - d));
    }
    let r = r.total() * inverse_energy;
    let s = s.total() * inverse_energy;
    let values = [(r + s) / 6.0, (-2.0 * r + s) / 6.0, (r - 2.0 * s) / 6.0];
    if values.iter().any(|value| !value.is_finite()) {
        return Err(CalyxError::forge_numerical_invariant(
            "linear CKA tuple sketch was non-finite",
        ));
    }
    Ok(values)
}

fn dot3(left: &[f64; 3], right: &[f64; 3]) -> f64 {
    left[0] * right[0] + left[1] * right[1] + left[2] * right[2]
}
