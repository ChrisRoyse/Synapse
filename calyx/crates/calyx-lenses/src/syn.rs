use calyx_core::{CalyxError, Result, SlotVector, SparseEntry, content_address};
use serde_json::Value;

const MICROS: f64 = 1_000_000.0;
const MAX_TEXT_TOKENS: usize = 4096;
const MAX_TOKEN_SLOTS: usize = 128;
const MAX_MULTI_HOT_TOKENS: usize = 2048;
const MAX_CROSS_TOKENS: usize = 256;
const MAX_RECORD_NUMBERS: usize = 8192;

pub(super) fn cyclic_time(bytes: &[u8], period: u32) -> Result<SlotVector> {
    ensure_positive("syn cyclic time period", period)?;
    let value = parse_number(bytes, "syn cyclic time input")?;
    let phase = value.rem_euclid(f64::from(period)) / f64::from(period);
    let angle = std::f64::consts::TAU * phase;
    dense(vec![
        finite_f32(angle.sin(), "syn cyclic time sin")?,
        finite_f32(angle.cos(), "syn cyclic time cos")?,
    ])
}

pub(super) fn scalar_raw(bytes: &[u8]) -> Result<SlotVector> {
    dense(vec![finite_f32(
        parse_number(bytes, "syn scalar raw input")?,
        "syn scalar raw output",
    )?])
}

pub(super) fn scalar_log1p(bytes: &[u8]) -> Result<SlotVector> {
    let value = parse_number(bytes, "syn scalar log1p input")?;
    if value <= -1.0 {
        return Err(numerical(
            "syn scalar log1p input must be greater than -1.0",
        ));
    }
    dense(vec![finite_f32(value.ln_1p(), "syn scalar log1p output")?])
}

pub(super) fn scalar_zscore(bytes: &[u8], mean_micros: i64, std_micros: u64) -> Result<SlotVector> {
    if std_micros == 0 {
        return Err(numerical("syn scalar zscore std_micros must be > 0"));
    }
    let value = parse_number(bytes, "syn scalar zscore input")?;
    let mean = mean_micros as f64 / MICROS;
    let std = std_micros as f64 / MICROS;
    dense(vec![finite_f32(
        (value - mean) / std,
        "syn scalar zscore output",
    )?])
}

pub(super) fn scalar_rank(bytes: &[u8], min_micros: i64, max_micros: i64) -> Result<SlotVector> {
    if min_micros >= max_micros {
        return Err(numerical(
            "syn scalar rank requires min_micros < max_micros",
        ));
    }
    let value = parse_number(bytes, "syn scalar rank input")?;
    let min = min_micros as f64 / MICROS;
    let max = max_micros as f64 / MICROS;
    if value < min || value > max {
        return Err(numerical(format!(
            "syn scalar rank input {value} outside frozen range [{min}, {max}]"
        )));
    }
    dense(vec![finite_f32(
        (value - min) / (max - min),
        "syn scalar rank output",
    )?])
}

/// The bounded rank placed on the unit half-circle (#1963).
///
/// Byte-identical in behaviour to the registry crate's `scalar_rank_arc`: the
/// two must agree or the measure path and the panel contract would resolve to
/// different lens ids for the same declared lens.
pub(super) fn scalar_rank_arc(
    bytes: &[u8],
    min_micros: i64,
    max_micros: i64,
) -> Result<SlotVector> {
    let unit = rank_unit(bytes, min_micros, max_micros, "syn scalar rank arc")?;
    let angle = std::f64::consts::PI * unit;
    dense(vec![
        finite_f32(angle.cos(), "syn scalar rank arc cos")?,
        finite_f32(angle.sin(), "syn scalar rank arc sin")?,
    ])
}

/// Shared, fail-closed rank normalization. See the registry crate's twin.
fn rank_unit(bytes: &[u8], min_micros: i64, max_micros: i64, label: &str) -> Result<f64> {
    if min_micros >= max_micros {
        return Err(numerical(format!(
            "{label} requires min_micros < max_micros"
        )));
    }
    let value = parse_number(bytes, &format!("{label} input"))?;
    let min = min_micros as f64 / MICROS;
    let max = max_micros as f64 / MICROS;
    if value < min || value > max {
        return Err(numerical(format!(
            "{label} input {value} outside frozen range [{min}, {max}]"
        )));
    }
    Ok((value - min) / (max - min))
}

pub(super) fn one_hot(bytes: &[u8], buckets: u32) -> Result<SlotVector> {
    ensure_positive("syn onehot buckets", buckets)?;
    let mut data = vec![0.0_f32; buckets as usize];
    let digest = content_address([b"syn-onehot-v1".as_slice(), bytes]);
    let idx = hash_prefix(&digest) % buckets;
    data[idx as usize] = 1.0;
    dense(data)
}

pub(super) fn one_hot_index(bytes: &[u8], levels: u32) -> Result<SlotVector> {
    ensure_positive("syn onehot index levels", levels)?;
    let value = parse_number(bytes, "syn onehot index input")?;
    if value.fract() != 0.0 || value < 0.0 || value >= f64::from(levels) {
        return Err(numerical(format!(
            "syn onehot index input {value} must be a whole category index in [0, {levels})"
        )));
    }
    #[allow(
        clippy::cast_possible_truncation,
        reason = "range and integrality checked above"
    )]
    let index = value as usize;
    let mut data = vec![0.0_f32; levels as usize];
    data[index] = 1.0;
    dense(data)
}

pub(super) fn hash(bytes: &[u8], dim: u32) -> Result<SlotVector> {
    let dim = ensure_power_of_two("syn hash dim", dim)?;
    let digest = content_address([b"syn-hash-v1".as_slice(), bytes]);
    let idx = hash_prefix(&digest) & (dim - 1);
    let val = signed_hash_value(&digest);
    Ok(SlotVector::Sparse {
        dim,
        entries: vec![SparseEntry { idx, val }],
    })
}

pub(super) fn sparse_text(bytes: &[u8], dim: u32) -> Result<SlotVector> {
    let tokens = text_tokens(bytes, MAX_TEXT_TOKENS, "syn sparse text")?;
    signed_sparse(
        tokens.iter().map(String::as_str),
        dim,
        b"syn-sparse-text-v1",
    )
}

/// Raw hashed term frequencies over free text, unsigned and unnormalized.
///
/// This is the payload a BM25 lane requires and `sparse_text` cannot provide.
/// `sparse_text` L1-normalizes signed hashes, so every stored vector sums to
/// 1.0: the term frequency becomes a *relative* frequency and the document
/// length disappears, which is exactly why a one-word title tied an exact
/// full-title match (#1900). BM25's `tf` must be a raw count and its `b`/avgdl
/// saturation must compare a real document length against a real corpus
/// average, so this encoder emits the count and lets the index supply the
/// corpus statistics — the standard HashingTF-then-IDF split, and the only
/// arrangement that keeps the lens data-oblivious while the ranking is
/// corpus-aware.
///
/// Signed hashing is deliberately dropped here. Signed feature hashing exists
/// to make a *dot product* an unbiased estimator; on a lexical ranking lane a
/// negative term frequency is meaningless, and sign cancellation inside a
/// bucket would silently destroy the count BM25 saturates over.
pub(super) fn sparse_text_tf(bytes: &[u8], dim: u32) -> Result<SlotVector> {
    let dim = ensure_power_of_two("syn sparse text tf dim", dim)?;
    let tokens = lexical_tokens(bytes, MAX_TEXT_TOKENS, "syn sparse text tf")?;
    let mut hasher = blake3::Hasher::new();
    let mut counts = Vec::with_capacity(tokens.len());
    for token in &tokens {
        let digest =
            content_address_pair_reusing(&mut hasher, b"syn-sparse-text-tf-v1", token.as_bytes());
        let idx = hash_prefix(&digest) & (dim - 1);
        counts.push((idx, 1.0));
    }
    let entries = fold_sparse_entries(counts);
    Ok(SlotVector::Sparse { dim, entries })
}

/// `text_tokens`, minus tokens that carry no alphanumeric character.
///
/// `text_tokens` keeps `-` and `_` as word characters so intra-word hyphens
/// survive, which also makes the standalone `-` in `"a.txt - Notepad"` its own
/// token. On a normalized lane that token consumed normalization mass; on a
/// term-frequency lane it would inflate the document length and earn its own
/// IDF weight. It carries no lexical content either way, so it is dropped here
/// (#1900 ask 4). The frozen `text_tokens` behaviour is left untouched: every
/// already-measured lens depends on it byte for byte.
fn lexical_tokens(bytes: &[u8], max_tokens: usize, label: &str) -> Result<Vec<String>> {
    Ok(text_tokens(bytes, max_tokens, label)?
        .into_iter()
        .filter(|token| token.chars().any(char::is_alphanumeric))
        .collect())
}

pub(super) fn token_slots(bytes: &[u8], token_dim: u32) -> Result<SlotVector> {
    let token_dim = ensure_power_of_two("syn token slots token_dim", token_dim)?;
    let tokens = text_tokens(bytes, MAX_TOKEN_SLOTS, "syn token slots")?;
    if tokens.is_empty() {
        return Err(numerical("syn token slots input produced no tokens"));
    }
    Ok(SlotVector::Multi {
        token_dim,
        tokens: tokens
            .iter()
            .map(|token| {
                let seed = content_address([b"syn-token-slots-v1".as_slice(), token.as_bytes()]);
                token_vector(&seed, token_dim)
            })
            .collect(),
    })
}

pub(super) fn multi_hot(bytes: &[u8], dim: u32) -> Result<SlotVector> {
    let tokens = tokens_from_json_or_text(bytes, MAX_MULTI_HOT_TOKENS, "syn multihot")?;
    signed_sparse(tokens.iter().map(String::as_str), dim, b"syn-multihot-v1")
}

pub(super) fn record_vector(bytes: &[u8], dim: u32) -> Result<SlotVector> {
    ensure_positive("syn record vector dim", dim)?;
    let value = parse_json(bytes, "syn record vector input")?;
    let mut numbers = Vec::new();
    collect_numbers("", &value, &mut numbers)?;
    if numbers.is_empty() {
        return Err(numerical("syn record vector found no numeric fields"));
    }
    if numbers.len() > MAX_RECORD_NUMBERS {
        return Err(numerical(format!(
            "syn record vector has {} numeric fields, max {MAX_RECORD_NUMBERS}",
            numbers.len()
        )));
    }
    let mut data = vec![0.0_f32; dim as usize];
    for (path, number) in numbers {
        let digest = content_address([b"syn-record-vector-v1".as_slice(), path.as_bytes()]);
        let idx = hash_prefix(&digest) % dim;
        let signed = f64::from(signed_hash_value(&digest));
        data[idx as usize] += finite_f32(number * signed, "syn record vector field")?;
    }
    normalize_unit(&mut data, "syn record vector")?;
    dense(data)
}

/// The same placement as [`record_vector`], with the scale precondition that
/// makes the result a summary of the record instead of a re-encoding of its
/// largest unit (#1964).
///
/// See the sibling implementation in `calyx-registry` for the full rationale:
/// `record_vector` weights each field by its raw magnitude and unit-normalizes,
/// so a field handed over in its own units (a unix-millisecond timestamp beside
/// a click count) owns the direction and leaves every other field below `f32`
/// resolution. All nine built-in lanes that did this returned nearest-neighbour
/// cosine exactly `1.000000` for every record.
///
/// # Errors
///
/// Returns a numerical-invariant error naming the offending field path and its
/// value when any field falls outside `[-1, 1]`, plus the same shape and field
/// count errors [`record_vector`] returns.
pub(super) fn record_vector_unit_fields(bytes: &[u8], dim: u32) -> Result<SlotVector> {
    ensure_positive("syn record vector unit fields dim", dim)?;
    let value = parse_json(bytes, "syn record vector unit fields input")?;
    let mut numbers = Vec::new();
    collect_numbers("", &value, &mut numbers)?;
    if numbers.is_empty() {
        return Err(numerical(
            "syn record vector unit fields found no numeric fields",
        ));
    }
    if numbers.len() > MAX_RECORD_NUMBERS {
        return Err(numerical(format!(
            "syn record vector unit fields has {} numeric fields, max {MAX_RECORD_NUMBERS}",
            numbers.len()
        )));
    }
    // Report the *whole* scale fault, not the first field that trips it. The
    // alphabetically-first offender is rarely the informative one: a record
    // carrying `row_count: 12` beside `start_unix_ms: 1.77e12` fails on
    // `row_count`, which names a real problem but hides the one that is eleven
    // orders of magnitude worse. An operator needs the worst offender, and how
    // many fields are involved, to know whether this is one stray field or a
    // record that was never scaled at all.
    let out_of_scale: Vec<&(String, f64)> = numbers
        .iter()
        .filter(|(_, number)| !(-1.0..=1.0).contains(number))
        .collect();
    if let Some((worst_path, worst)) = out_of_scale
        .iter()
        .max_by(|(_, a), (_, b)| a.abs().total_cmp(&b.abs()))
    {
        let also = if out_of_scale.len() == 1 {
            String::new()
        } else {
            format!(
                " ({} further out-of-scale field(s): {})",
                out_of_scale.len() - 1,
                out_of_scale
                    .iter()
                    .filter(|(path, _)| path != worst_path)
                    .map(|(path, value)| format!("{path}={value}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        return Err(numerical(format!(
            "syn record vector unit fields requires every field on a comparable scale in \
             [-1, 1]; the largest offender is field `{worst_path}` at {worst}{also}. A record \
             vector weights each field by its raw magnitude and unit-normalizes, so an unscaled \
             field owns the direction and every other field falls below f32 resolution. Encode \
             each field first: a timestamp as a position within a cycle (day/week fraction), a \
             count or byte length as ln(1+n)/ln(1+scale) clamped to 1, a part as a ratio of its \
             whole."
        )));
    }
    let mut data = vec![0.0_f32; dim as usize];
    for (path, number) in numbers {
        let digest = content_address([
            b"syn-record-vector-unit-fields-v1".as_slice(),
            path.as_bytes(),
        ]);
        let idx = hash_prefix(&digest) % dim;
        let signed = f64::from(signed_hash_value(&digest));
        data[idx as usize] += finite_f32(number * signed, "syn record vector unit fields field")?;
    }
    normalize_unit(&mut data, "syn record vector unit fields")?;
    dense(data)
}

pub(super) fn bin(
    bytes: &[u8],
    buckets: u32,
    min_micros: i64,
    max_micros: i64,
) -> Result<SlotVector> {
    ensure_positive("syn bin buckets", buckets)?;
    if min_micros >= max_micros {
        return Err(numerical("syn bin requires min_micros < max_micros"));
    }
    let value = parse_number(bytes, "syn bin input")?;
    let min = min_micros as f64 / MICROS;
    let max = max_micros as f64 / MICROS;
    if value < min || value > max {
        return Err(numerical(format!(
            "syn bin input {value} outside frozen range [{min}, {max}]"
        )));
    }
    let mut data = vec![0.0_f32; buckets as usize];
    let span = max - min;
    let idx = if value == max {
        buckets - 1
    } else {
        (((value - min) / span) * f64::from(buckets)).floor() as u32
    };
    data[idx as usize] = 1.0;
    dense(data)
}

pub(super) fn ordinal(bytes: &[u8], levels: u32) -> Result<SlotVector> {
    if levels <= 1 {
        return Err(numerical("syn ordinal levels must be > 1"));
    }
    let value = parse_number(bytes, "syn ordinal input")?;
    let rounded = value.round();
    if (value - rounded).abs() > f64::EPSILON {
        return Err(numerical("syn ordinal input must be an integer level"));
    }
    if rounded < 0.0 || rounded > f64::from(levels - 1) {
        return Err(numerical(format!(
            "syn ordinal input {value} outside levels 0..{}",
            levels - 1
        )));
    }
    dense(vec![finite_f32(
        rounded / f64::from(levels - 1),
        "syn ordinal output",
    )?])
}

pub(super) fn frequency(count: u64, total: u64) -> Result<SlotVector> {
    if total == 0 {
        return Err(numerical("syn frequency total must be > 0"));
    }
    if count > total {
        return Err(numerical("syn frequency count must be <= total"));
    }
    dense(vec![finite_f32(
        count as f64 / total as f64,
        "syn frequency output",
    )?])
}

pub(super) fn target_mean(
    mean_micros: i64,
    fold_count: u32,
    _outcome_hash: u32,
) -> Result<SlotVector> {
    if fold_count <= 1 {
        return Err(numerical("syn target mean fold_count must be > 1"));
    }
    dense(vec![finite_f32(
        mean_micros as f64 / MICROS,
        "syn target mean output",
    )?])
}

pub(super) fn delta(bytes: &[u8], scale_micros: u64) -> Result<SlotVector> {
    scaled_tanh(bytes, scale_micros, "syn delta")
}

pub(super) fn rate(bytes: &[u8], scale_micros: u64) -> Result<SlotVector> {
    scaled_tanh(bytes, scale_micros, "syn rate")
}

pub(super) fn cross(bytes: &[u8], dim: u32) -> Result<SlotVector> {
    let tokens = tokens_from_json_or_text(bytes, MAX_CROSS_TOKENS, "syn cross")?;
    if tokens.len() > MAX_CROSS_TOKENS {
        return Err(numerical(format!(
            "syn cross has {} tokens, max {MAX_CROSS_TOKENS}",
            tokens.len()
        )));
    }
    let mut pairs = Vec::new();
    for left in 0..tokens.len() {
        for right in left + 1..tokens.len() {
            pairs.push(format!("{}|{}", tokens[left], tokens[right]));
        }
    }
    signed_sparse(pairs.iter().map(String::as_str), dim, b"syn-cross-v1")
}

pub(super) fn aggregation(bytes: &[u8], dim: u32) -> Result<SlotVector> {
    ensure_positive("syn aggregation dim", dim)?;
    let value = parse_json(bytes, "syn aggregation input")?;
    let mut numbers = Vec::new();
    collect_numbers("", &value, &mut numbers)?;
    if numbers.is_empty() {
        return Err(numerical("syn aggregation found no numeric fields"));
    }
    if numbers.len() > MAX_RECORD_NUMBERS {
        return Err(numerical(format!(
            "syn aggregation has {} numeric fields, max {MAX_RECORD_NUMBERS}",
            numbers.len()
        )));
    }
    let count = numbers.len() as f64;
    let sum = numbers.iter().map(|(_, value)| *value).sum::<f64>();
    let mean = sum / count;
    let min = numbers
        .iter()
        .map(|(_, value)| *value)
        .fold(f64::INFINITY, f64::min);
    let max = numbers
        .iter()
        .map(|(_, value)| *value)
        .fold(f64::NEG_INFINITY, f64::max);
    let variance = numbers
        .iter()
        .map(|(_, value)| (*value - mean).powi(2))
        .sum::<f64>()
        / count;
    let mut data = vec![0.0_f32; dim as usize];
    add_dense_feature(&mut data, dim, "count", count.ln_1p())?;
    add_dense_feature(&mut data, dim, "mean", mean.tanh())?;
    add_dense_feature(&mut data, dim, "min", min.tanh())?;
    add_dense_feature(&mut data, dim, "max", max.tanh())?;
    add_dense_feature(&mut data, dim, "std", variance.sqrt().tanh())?;
    for (path, value) in numbers {
        add_dense_feature(&mut data, dim, &path, value.tanh() / count)?;
    }
    dense(data)
}

/// Frozen dense structural-position signature of a node in a graph snapshot.
///
/// Input is the JSON structural signature produced by the `calyx-mincut`
/// substrate (`in_degree`, `out_degree`, `total_degree`, `betweenness`,
/// `eigenvector`, `pagerank`, `clustering`). Degrees are log1p-squashed into
/// `(0, 1)`; the four `[0, 1]` centralities pass through with a fail-closed
/// range check; the eighth channel is the in/out degree ratio (0.5 when
/// isolated). The `snapshot` fingerprint is not part of the value — it is folded
/// into the lens id upstream so a new snapshot yields a new frozen lens version.
pub(super) fn graph_signature(bytes: &[u8], _snapshot: u64) -> Result<SlotVector> {
    let value = parse_json(bytes, "syn graph signature input")?;
    let obj = require_object(&value, "syn graph signature")?;
    let in_degree = require_nonneg(obj, "in_degree", "syn graph signature")?;
    let out_degree = require_nonneg(obj, "out_degree", "syn graph signature")?;
    let total_degree = require_nonneg(obj, "total_degree", "syn graph signature")?;
    let betweenness = require_unit(obj, "betweenness", "syn graph signature")?;
    let eigenvector = require_unit(obj, "eigenvector", "syn graph signature")?;
    let pagerank = require_unit(obj, "pagerank", "syn graph signature")?;
    let clustering = require_unit(obj, "clustering", "syn graph signature")?;
    let in_ratio = if total_degree > 0.0 {
        in_degree / total_degree
    } else {
        0.5
    };
    dense(vec![
        finite_f32(in_degree.ln_1p().tanh(), "syn graph signature in_degree")?,
        finite_f32(out_degree.ln_1p().tanh(), "syn graph signature out_degree")?,
        finite_f32(
            total_degree.ln_1p().tanh(),
            "syn graph signature total_degree",
        )?,
        finite_f32(betweenness, "syn graph signature betweenness")?,
        finite_f32(eigenvector, "syn graph signature eigenvector")?,
        finite_f32(pagerank, "syn graph signature pagerank")?,
        finite_f32(clustering, "syn graph signature clustering")?,
        finite_f32(in_ratio, "syn graph signature in_ratio")?,
    ])
}

/// Frozen dense path/hierarchy-position signature of a record in a hierarchy
/// snapshot (document/URL path tree, process tree, agent spawn tree).
///
/// Input JSON fields: `depth`, `sibling_rank`, `sibling_count`, `subtree_size`,
/// `ancestor_count`, `path_len` (non-negative numbers) and optional booleans
/// `is_root`, `is_leaf`. Depth/subtree/ancestor/path-length are log1p-squashed;
/// sibling rank is normalized within its sibling group. As with the graph
/// signature, `snapshot` only participates in the lens id, never the value.
pub(super) fn path_signature(bytes: &[u8], _snapshot: u64) -> Result<SlotVector> {
    let value = parse_json(bytes, "syn path signature input")?;
    let obj = require_object(&value, "syn path signature")?;
    let depth = require_nonneg(obj, "depth", "syn path signature")?;
    let sibling_rank = require_nonneg(obj, "sibling_rank", "syn path signature")?;
    let sibling_count = require_nonneg(obj, "sibling_count", "syn path signature")?;
    let subtree_size = require_nonneg(obj, "subtree_size", "syn path signature")?;
    let ancestor_count = require_nonneg(obj, "ancestor_count", "syn path signature")?;
    let path_len = require_nonneg(obj, "path_len", "syn path signature")?;
    let is_root = optional_bool(obj, "is_root", "syn path signature")?;
    let is_leaf = optional_bool(obj, "is_leaf", "syn path signature")?;
    if sibling_count > 0.0 && sibling_rank > sibling_count - 1.0 {
        return Err(numerical(
            "syn path signature sibling_rank must be < sibling_count",
        ));
    }
    let sibling_norm = if sibling_count > 1.0 {
        sibling_rank / (sibling_count - 1.0)
    } else {
        0.0
    };
    dense(vec![
        finite_f32(depth.ln_1p().tanh(), "syn path signature depth")?,
        finite_f32(sibling_norm, "syn path signature sibling_rank")?,
        finite_f32(
            subtree_size.ln_1p().tanh(),
            "syn path signature subtree_size",
        )?,
        finite_f32(
            ancestor_count.ln_1p().tanh(),
            "syn path signature ancestor_count",
        )?,
        finite_f32(path_len.ln_1p().tanh(), "syn path signature path_len")?,
        finite_f32(is_root, "syn path signature is_root")?,
        finite_f32(is_leaf, "syn path signature is_leaf")?,
        finite_f32(
            sibling_count.ln_1p().tanh(),
            "syn path signature sibling_count",
        )?,
    ])
}

fn require_object<'a>(value: &'a Value, label: &str) -> Result<&'a serde_json::Map<String, Value>> {
    value
        .as_object()
        .ok_or_else(|| numerical(format!("{label} input must be a JSON object")))
}

fn require_number(obj: &serde_json::Map<String, Value>, key: &str, label: &str) -> Result<f64> {
    let field = obj
        .get(key)
        .ok_or_else(|| numerical(format!("{label} missing field {key}")))?;
    numeric_from_json(&format!("{label} field {key}"), field)
}

fn require_nonneg(obj: &serde_json::Map<String, Value>, key: &str, label: &str) -> Result<f64> {
    let value = require_number(obj, key, label)?;
    if value < 0.0 {
        return Err(numerical(format!("{label} field {key} must be >= 0")));
    }
    Ok(value)
}

fn require_unit(obj: &serde_json::Map<String, Value>, key: &str, label: &str) -> Result<f64> {
    let value = require_number(obj, key, label)?;
    if !(0.0..=1.0).contains(&value) {
        return Err(numerical(format!(
            "{label} field {key} must be within [0, 1]"
        )));
    }
    Ok(value)
}

fn optional_bool(obj: &serde_json::Map<String, Value>, key: &str, label: &str) -> Result<f64> {
    match obj.get(key) {
        None | Some(Value::Null) => Ok(0.0),
        Some(Value::Bool(flag)) => Ok(if *flag { 1.0 } else { 0.0 }),
        Some(Value::Number(_)) => {
            let value = require_number(obj, key, label)?;
            if value == 0.0 {
                Ok(0.0)
            } else if value == 1.0 {
                Ok(1.0)
            } else {
                Err(numerical(format!(
                    "{label} field {key} must be 0, 1, or a bool"
                )))
            }
        }
        Some(_) => Err(numerical(format!("{label} field {key} must be a bool"))),
    }
}

fn scaled_tanh(bytes: &[u8], scale_micros: u64, label: &str) -> Result<SlotVector> {
    if scale_micros == 0 {
        return Err(numerical(format!("{label} scale_micros must be > 0")));
    }
    let value = parse_number(bytes, &format!("{label} input"))?;
    let scale = scale_micros as f64 / MICROS;
    dense(vec![finite_f32(
        (value / scale).tanh(),
        format!("{label} output"),
    )?])
}

fn dense(data: Vec<f32>) -> Result<SlotVector> {
    Ok(SlotVector::Dense {
        dim: data.len() as u32,
        data,
    })
}

fn signed_sparse<'a>(
    tokens: impl Iterator<Item = &'a str>,
    dim: u32,
    namespace: &'static [u8],
) -> Result<SlotVector> {
    let dim = ensure_power_of_two("syn sparse dim", dim)?;
    let mut hasher = blake3::Hasher::new();
    let mut counts = Vec::new();
    let mut total = 0.0_f32;
    for token in tokens {
        let digest = content_address_pair_reusing(&mut hasher, namespace, token.as_bytes());
        let idx = hash_prefix(&digest) & (dim - 1);
        let val = signed_hash_value(&digest);
        counts.push((idx, val));
        total += val.abs();
    }
    let mut entries = fold_sparse_entries(counts);
    if total != 0.0 {
        for entry in &mut entries {
            entry.val /= total;
        }
        entries.retain(|entry| entry.val != 0.0);
    } else {
        entries.clear();
    }
    Ok(SlotVector::Sparse { dim, entries })
}

fn text_tokens(bytes: &[u8], max_tokens: usize, label: &str) -> Result<Vec<String>> {
    let text = std::str::from_utf8(bytes)
        .map_err(|err| numerical(format!("{label} input is not valid UTF-8: {err}")))?;
    let tokens = text
        .split(|ch: char| !(ch.is_alphanumeric() || ch == '_' || ch == '-'))
        .filter(|token| !token.is_empty())
        .map(|token| token.to_ascii_lowercase())
        .collect::<Vec<_>>();
    ensure_token_limit(label, tokens.len(), max_tokens)?;
    Ok(tokens)
}

fn tokens_from_json_or_text(bytes: &[u8], max_tokens: usize, label: &str) -> Result<Vec<String>> {
    let text = std::str::from_utf8(bytes)
        .map_err(|err| numerical(format!("{label} input is not valid UTF-8: {err}")))?;
    let trimmed = text.trim_start();
    if trimmed.starts_with('{') || trimmed.starts_with('[') {
        let value = parse_json(bytes, label)?;
        let mut tokens = Vec::new();
        collect_json_tokens("", &value, &mut tokens)?;
        ensure_token_limit(label, tokens.len(), max_tokens)?;
        Ok(tokens)
    } else {
        text_tokens(bytes, max_tokens, label)
    }
}

fn collect_json_tokens(path: &str, value: &Value, out: &mut Vec<String>) -> Result<()> {
    match value {
        Value::Null => Ok(()),
        Value::Bool(value) => {
            out.push(format!("{path}=bool:{value}"));
            Ok(())
        }
        Value::Number(number) => {
            let value = number
                .as_f64()
                .ok_or_else(|| numerical(format!("numeric token at {path} is not finite")))?;
            if !value.is_finite() {
                return Err(numerical(format!("numeric token at {path} is not finite")));
            }
            out.push(format!("{path}=num:{value:.12}"));
            Ok(())
        }
        Value::String(value) => {
            let mut token_path = path.to_string();
            if token_path.is_empty() {
                token_path.push('$');
            }
            out.push(format!("{token_path}=str:{}", value.to_ascii_lowercase()));
            Ok(())
        }
        Value::Array(items) => {
            for (index, item) in items.iter().enumerate() {
                collect_json_tokens(&format!("{path}[{index}]"), item, out)?;
            }
            Ok(())
        }
        Value::Object(map) => {
            let mut keys = map.keys().collect::<Vec<_>>();
            keys.sort();
            for key in keys {
                let next_path = if path.is_empty() {
                    key.to_string()
                } else {
                    format!("{path}.{key}")
                };
                collect_json_tokens(&next_path, &map[key], out)?;
            }
            Ok(())
        }
    }
}

fn collect_numbers(path: &str, value: &Value, out: &mut Vec<(String, f64)>) -> Result<()> {
    match value {
        Value::Null | Value::Bool(_) | Value::String(_) => Ok(()),
        Value::Number(number) => {
            let value = number
                .as_f64()
                .ok_or_else(|| numerical(format!("number at {path} is not representable")))?;
            if !value.is_finite() {
                return Err(numerical(format!("number at {path} is not finite")));
            }
            let path = if path.is_empty() {
                "$".to_string()
            } else {
                path.to_string()
            };
            out.push((path, value));
            Ok(())
        }
        Value::Array(items) => {
            for (index, item) in items.iter().enumerate() {
                collect_numbers(&format!("{path}[{index}]"), item, out)?;
            }
            Ok(())
        }
        Value::Object(map) => {
            let mut keys = map.keys().collect::<Vec<_>>();
            keys.sort();
            for key in keys {
                let next_path = if path.is_empty() {
                    key.to_string()
                } else {
                    format!("{path}.{key}")
                };
                collect_numbers(&next_path, &map[key], out)?;
            }
            Ok(())
        }
    }
}

fn parse_number(bytes: &[u8], label: &str) -> Result<f64> {
    let text = std::str::from_utf8(bytes)
        .map_err(|err| numerical(format!("{label} is not valid UTF-8: {err}")))?;
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err(numerical(format!("{label} is empty")));
    }
    let value = if trimmed.starts_with('{') || trimmed.starts_with('[') {
        let json = parse_json(bytes, label)?;
        numeric_from_json("", &json)?
    } else {
        trimmed
            .parse::<f64>()
            .map_err(|err| numerical(format!("{label} is not a number: {err}")))?
    };
    if value.is_finite() {
        Ok(value)
    } else {
        Err(numerical(format!("{label} is NaN or Inf")))
    }
}

fn numeric_from_json(path: &str, value: &Value) -> Result<f64> {
    match value {
        Value::Number(number) => number
            .as_f64()
            .filter(|value| value.is_finite())
            .ok_or_else(|| numerical(format!("{path} is not a finite JSON number"))),
        Value::String(value) => {
            let parsed = value
                .parse::<f64>()
                .map_err(|err| numerical(format!("{path} is not a numeric string: {err}")))?;
            if parsed.is_finite() {
                Ok(parsed)
            } else {
                Err(numerical(format!("{path} numeric string is NaN or Inf")))
            }
        }
        other => Err(numerical(format!(
            "{path} must be a JSON number or numeric string, got {other:?}"
        ))),
    }
}

fn parse_json(bytes: &[u8], label: &str) -> Result<Value> {
    serde_json::from_slice(bytes)
        .map_err(|err| numerical(format!("{label} is invalid JSON: {err}")))
}

fn add_dense_feature(data: &mut [f32], dim: u32, key: &str, value: f64) -> Result<()> {
    let digest = content_address([b"syn-aggregation-v1".as_slice(), key.as_bytes()]);
    let idx = hash_prefix(&digest) % dim;
    data[idx as usize] += finite_f32(value, format!("syn aggregation feature {key}"))?;
    Ok(())
}

fn normalize_unit(data: &mut [f32], label: &str) -> Result<()> {
    let norm = data
        .iter()
        .map(|value| f64::from(*value).powi(2))
        .sum::<f64>()
        .sqrt();
    if !norm.is_finite() || norm == 0.0 {
        return Err(numerical(format!(
            "{label} produced a zero or non-finite norm"
        )));
    }
    for value in data {
        *value = finite_f32(f64::from(*value) / norm, label)?;
    }
    Ok(())
}

fn finite_f32(value: f64, label: impl AsRef<str>) -> Result<f32> {
    let value = value as f32;
    if value.is_finite() {
        Ok(value)
    } else {
        Err(numerical(format!("{} produced NaN or Inf", label.as_ref())))
    }
}

fn ensure_positive(label: &str, value: u32) -> Result<u32> {
    if value > 0 {
        Ok(value)
    } else {
        Err(numerical(format!("{label} must be > 0")))
    }
}

fn ensure_power_of_two(label: &str, value: u32) -> Result<u32> {
    ensure_positive(label, value)?;
    if value.is_power_of_two() {
        Ok(value)
    } else {
        Err(numerical(format!("{label} must be a power of two")))
    }
}

/// Refuses an input above the lens's declared token bound.
///
/// This is `CALYX_LENS_INPUT_TOO_LARGE`, not `CALYX_LENS_NUMERICAL_INVARIANT`,
/// and the distinction is load-bearing (#1924). An over-long input is a
/// property of the *data*: the correct response is to leave this one slot
/// `Absent{Error}` and keep measuring the rest of the panel. A numerical
/// invariant is a property of the *code*: a NaN or a wrong output dimension
/// means the lens is broken and the record must not be published at all.
///
/// While both refusals shared one code, a caller could only tell them apart by
/// matching on message text, so the only safe blast radius was "abort the whole
/// constellation" — which is how one over-long row came to lose its role
/// one-hot, its token scalars and its temporal lenses along with its text lane.
fn ensure_token_limit(label: &str, got: usize, max: usize) -> Result<()> {
    if got <= max {
        Ok(())
    } else {
        Err(CalyxError::lens_input_too_large(format!(
            "{label} has {got} tokens, max {max}"
        )))
    }
}

fn hash_prefix(digest: &[u8; 16]) -> u32 {
    u32::from_be_bytes(
        digest[..4]
            .try_into()
            .expect("digest has four prefix bytes"),
    )
}

fn signed_hash_value(digest: &[u8; 16]) -> f32 {
    if digest[4] & 1 == 0 { 1.0 } else { -1.0 }
}

/// Two-part `content_address` with caller-owned state. `Hasher::reset` is
/// specified to be equivalent to replacing it with a new unkeyed hasher; the
/// same length framing and 16-byte prefix therefore preserve the frozen lens
/// digest while avoiding one hasher construction per token.
fn content_address_pair_reusing(
    hasher: &mut blake3::Hasher,
    first: &[u8],
    second: &[u8],
) -> [u8; 16] {
    hasher.reset();
    for part in [first, second] {
        hasher.update(&(part.len() as u64).to_be_bytes());
        hasher.update(part);
    }
    let mut digest = [0_u8; 16];
    digest.copy_from_slice(&hasher.finalize().as_bytes()[..16]);
    digest
}

/// Turns token-order hash hits into the same index-ordered sums a `BTreeMap`
/// produced, without a tree allocation and pointer chase per token. Every
/// summand is exactly +/-1 and token counts are capped far below f32's exact
/// integer range, so grouping equal indices cannot change a value bit.
fn fold_sparse_entries(mut counts: Vec<(u32, f32)>) -> Vec<SparseEntry> {
    counts.sort_unstable_by_key(|(idx, _)| *idx);
    let mut entries: Vec<SparseEntry> = Vec::with_capacity(counts.len());
    for (idx, val) in counts {
        if let Some(last) = entries.last_mut()
            && last.idx == idx
        {
            last.val += val;
        } else {
            entries.push(SparseEntry { idx, val });
        }
    }
    entries
}

fn token_vector(seed: &[u8], dim: u32) -> Vec<f32> {
    let mut out = Vec::with_capacity(dim as usize);
    let mut counter = 0_u32;
    let mut hasher = blake3::Hasher::new();
    while out.len() < dim as usize {
        hasher.reset();
        hasher.update(b"calyx-algorithmic-token-hash-v1");
        hasher.update(seed);
        hasher.update(&counter.to_be_bytes());
        for chunk in hasher.finalize().as_bytes().chunks_exact(4) {
            let raw = u32::from_be_bytes(chunk.try_into().expect("blake3 chunk is 4 bytes"));
            out.push(hash_part(raw));
            if out.len() == dim as usize {
                break;
            }
        }
        counter = counter.saturating_add(1);
    }
    out
}

fn hash_part(value: u32) -> f32 {
    (value as f32 / u32::MAX as f32) * 2.0 - 1.0
}

fn numerical(message: impl Into<String>) -> CalyxError {
    CalyxError::lens_numerical_invariant(message)
}
