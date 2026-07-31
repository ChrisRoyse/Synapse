//! Manual FSV instrument: does `calyx-forge`'s fused finiteness guard actually
//! detect everything the pre-scan it replaced detected — and the case it missed?
//!
//! ## Why this exists
//!
//! #1912's fix removed a guard. `cosine_batch`/`dot_batch`/`l2_batch` used to
//! scan the entire candidate matrix for non-finite elements before reducing it,
//! a second full pass over memory that was most of why `calyx-forge` could not
//! reach a plain scalar loop. The scan is gone, replaced by a check on the
//! reduction result plus a failure-path scan that recovers the exact index.
//!
//! Removing a guard on a performance argument is exactly the kind of change that
//! deserves to be disbelieved, so this instrument does not take the reasoning on
//! trust. It drives the real kernels with inputs whose correct answer is known by
//! construction, and prints the observed outcome next to the expected one.
//!
//! The argument being tested is:
//!
//! > A non-finite input poisons its lane accumulator and stays poisoned. `NaN`
//! > propagates through every add; `+inf` stays infinite unless it meets `-inf`,
//! > which yields `NaN`. Therefore `result.is_finite()` is false whenever any
//! > contributing element was non-finite, and the pre-scan added nothing the
//! > result did not already carry.
//!
//! Claim 5 is the interesting one: it is a case the *old* pre-scan passed
//! silently, so the fused guard is strictly stronger, not merely equivalent.
//!
//! ```text
//! cargo run --release -p synapse-storage --example forge_guard_edge_fsv
//! ```

use std::error::Error;

use calyx_forge::cpu::{cosine_batch, dot_batch, l2_batch, paired_cosine_batch};

/// One claim: drive a kernel and compare the outcome to what is known to be correct.
struct Claim {
    id: usize,
    what: String,
    expected: String,
    observed: String,
    ok: bool,
}

fn claim(id: usize, what: &str, expected: &str, observed: String, ok: bool) -> Claim {
    Claim {
        id,
        what: what.to_owned(),
        expected: expected.to_owned(),
        observed,
        ok,
    }
}

/// Render whatever a kernel returned as a single comparable line.
fn outcome(result: Result<(), calyx_forge::ForgeError>, scores: &[f32]) -> String {
    match result {
        Ok(()) => format!("Ok scores={scores:?}"),
        Err(error) => format!("Err {error}"),
    }
}

fn err_contains(result: &Result<(), calyx_forge::ForgeError>, needles: &[&str]) -> bool {
    let Err(error) = result else {
        return false;
    };
    let text = error.to_string();
    needles.iter().all(|needle| text.contains(needle))
}

fn main() -> Result<(), Box<dyn Error>> {
    println!("forge_guard_edge_fsv");
    println!(
        "  dispatched cpu kernel backend = {}",
        calyx_forge::cpu::simd::backend_name()
    );
    println!(
        "  lane width = {}  (inputs below straddle it deliberately)",
        calyx_forge::cpu::simd::LANES
    );

    let mut claims = Vec::new();

    // ---------------------------------------------------------------- claim 1
    // Happy path with an analytically known answer, at a dim that is an exact
    // multiple of the lane width so the whole reduction runs in the vector body.
    //
    // query = [1,1,...,1] (16), candidate row 0 = [1..16], row 1 = [2,2,...,2].
    //   dot(row0)  = 1+2+...+16 = 136
    //   |row0|     = sqrt(1^2+...+16^2) = sqrt(1496)
    //   |query|    = sqrt(16) = 4
    //   cos(row0)  = 136 / (4 * sqrt(1496)) = 0.8791277...
    //   dot(row1)  = 32, |row1| = sqrt(64) = 8, cos = 32/(4*8) = 1.0 exactly
    {
        let dim = 16;
        let query = vec![1.0_f32; dim];
        let mut candidates: Vec<f32> = (1..=16).map(|v| v as f32).collect();
        candidates.extend(std::iter::repeat_n(2.0_f32, dim));
        let mut scores = vec![0.0_f32; 2];
        println!("\n[claim 1] before: scores={scores:?}");
        let result = cosine_batch(&query, &candidates, dim, &mut scores);
        println!("[claim 1] after:  scores={scores:?}");
        let expected_row0 = 136.0_f64 / (4.0 * 1496.0_f64.sqrt());
        let row0_close = (f64::from(scores[0]) - expected_row0).abs() < 1e-6;
        // Row 1 is a positive multiple of the query, so cosine is exactly 1.0 and
        // may be compared for equality rather than closeness.
        let row1_exact = scores[1] == 1.0;
        claims.push(claim(
            1,
            "cosine of known vectors (dim=16, whole vector body)",
            &format!("Ok scores=[{expected_row0:.7}, 1.0 exactly]"),
            format!(
                "{} (row0 delta={:.2e}, row1 == 1.0 is {row1_exact})",
                outcome(result, &scores),
                (f64::from(scores[0]) - expected_row0).abs()
            ),
            row0_close && row1_exact,
        ));
    }

    // ---------------------------------------------------------------- claim 2
    // A dim shorter than the lane width: the reduction is entirely scalar tail.
    // query=[3,4,0], candidate=[3,4,0] -> identical vectors, cosine exactly 1.0.
    // Second row [0,0,5]: dot=0, so cosine is exactly 0.0.
    {
        let dim = 3;
        let query = vec![3.0_f32, 4.0, 0.0];
        let candidates = vec![3.0_f32, 4.0, 0.0, 0.0, 0.0, 5.0];
        let mut scores = vec![0.0_f32; 2];
        println!("\n[claim 2] before: scores={scores:?}");
        let result = cosine_batch(&query, &candidates, dim, &mut scores);
        println!("[claim 2] after:  scores={scores:?}");
        let ok = scores[0] == 1.0 && scores[1] == 0.0;
        claims.push(claim(
            2,
            "cosine at dim=3 (scalar tail only, no vector body)",
            "Ok scores=[1.0, 0.0] both exact",
            outcome(result, &scores),
            ok,
        ));
    }

    // ---------------------------------------------------------------- claim 3
    // NaN in a candidate, placed in the VECTOR BODY (element 5 of 16). The old
    // pre-scan caught this. The fused guard must catch it too, and must name the
    // exact element rather than merely reporting that something went wrong.
    {
        let dim = 16;
        let query = vec![1.0_f32; dim];
        let mut candidates = vec![1.0_f32; dim * 2];
        candidates[dim + 5] = f32::NAN; // row 1, element 5
        let mut scores = vec![0.0_f32; 2];
        println!(
            "\n[claim 3] before: candidates[row=1][elem=5]={}",
            candidates[dim + 5]
        );
        let result = cosine_batch(&query, &candidates, dim, &mut scores);
        println!("[claim 3] after:  {}", outcome_str(&result));
        let ok = err_contains(&result, &["row 1", "candidate element 5", "NaN"]);
        claims.push(claim(
            3,
            "NaN in the vector body is refused and located",
            "Err naming row 1, candidate element 5, NaN",
            outcome_str(&result),
            ok,
        ));
    }

    // ---------------------------------------------------------------- claim 4
    // +inf in the SCALAR TAIL (element 18 of a dim=19 row, which is two full
    // lanes plus a 3-element tail). The tail is a separate code path from the
    // vector body, so it needs its own claim.
    {
        let dim = 19;
        let query = vec![1.0_f32; dim];
        let mut candidates = vec![1.0_f32; dim];
        candidates[18] = f32::INFINITY;
        let mut scores = vec![0.0_f32; 1];
        println!(
            "\n[claim 4] before: candidates[row=0][elem=18]={}",
            candidates[18]
        );
        let result = cosine_batch(&query, &candidates, dim, &mut scores);
        println!("[claim 4] after:  {}", outcome_str(&result));
        let ok = err_contains(&result, &["row 0", "candidate element 18", "inf"]);
        claims.push(claim(
            4,
            "+inf in the scalar tail is refused and located",
            "Err naming row 0, candidate element 18, inf",
            outcome_str(&result),
            ok,
        ));
    }

    // ---------------------------------------------------------------- claim 5
    // The case the OLD pre-scan passed silently. Every element is finite, but
    // 1e20^2 = 1e40 overflows f32 (max ~3.4e38), so the norm accumulator becomes
    // +inf and the score becomes NaN. The pre-scan checked the INPUTS and was
    // happy; nothing checked the OUTPUT. This must now be a named error.
    {
        let dim = 16;
        let query = vec![1.0_f32; dim];
        let candidates = vec![1e20_f32; dim];
        let all_finite = candidates.iter().all(|v| v.is_finite());
        let mut scores = vec![0.0_f32; 1];
        println!("\n[claim 5] before: every candidate element finite = {all_finite}, value=1e20");
        let result = cosine_batch(&query, &candidates, dim, &mut scores);
        println!("[claim 5] after:  {}", outcome_str(&result));
        let ok = all_finite && err_contains(&result, &["row 0", "finite", "overflowed"]);
        claims.push(claim(
            5,
            "finite inputs that overflow f32 are refused (the pre-scan passed these)",
            "Err naming row 0 and an overflow of finite inputs",
            outcome_str(&result),
            ok,
        ));
    }

    // ---------------------------------------------------------------- claim 6
    // A non-finite element in the QUERY is still pre-scanned, because the query
    // is a single row and attributing the failure to the query is more useful
    // than attributing it to whichever candidate row reduced first.
    {
        let dim = 16;
        let mut query = vec![1.0_f32; dim];
        query[2] = f32::NAN;
        let candidates = vec![1.0_f32; dim];
        let mut scores = vec![0.0_f32; 1];
        println!("\n[claim 6] before: query[2]={}", query[2]);
        let result = cosine_batch(&query, &candidates, dim, &mut scores);
        println!("[claim 6] after:  {}", outcome_str(&result));
        let ok = err_contains(&result, &["index 2"]);
        claims.push(claim(
            6,
            "non-finite in the query is attributed to the query",
            "Err naming index 2 of the query",
            outcome_str(&result),
            ok,
        ));
    }

    // ---------------------------------------------------------------- claim 7
    // A zero row has no direction, so cosine against it is undefined. This is
    // `check_norm_positive`, which the fused guard must not have displaced: a
    // zero norm is finite, so the finiteness check cannot catch it.
    {
        let dim = 16;
        let query = vec![1.0_f32; dim];
        let candidates = vec![0.0_f32; dim];
        let mut scores = vec![0.0_f32; 1];
        println!("\n[claim 7] before: candidate row is all zeros, norm=0 (finite)");
        let result = cosine_batch(&query, &candidates, dim, &mut scores);
        println!("[claim 7] after:  {}", outcome_str(&result));
        let ok = err_contains(&result, &["zero or non-finite norm", "row 0"]);
        claims.push(claim(
            7,
            "zero-norm row is still refused (finiteness cannot catch it)",
            "Err naming a zero norm at row 0",
            outcome_str(&result),
            ok,
        ));
    }

    // ---------------------------------------------------------------- claim 8
    // Empty batch: zero rows is a legal request, not an error, and must not be
    // turned into one by a guard that assumes at least one row.
    {
        let dim = 16;
        let query = vec![1.0_f32; dim];
        let candidates: Vec<f32> = Vec::new();
        let mut scores: Vec<f32> = Vec::new();
        println!("\n[claim 8] before: 0 candidate rows, 0 output slots");
        let result = cosine_batch(&query, &candidates, dim, &mut scores);
        println!("[claim 8] after:  {}", outcome_str(&result));
        let ok = result.is_ok() && scores.is_empty();
        claims.push(claim(
            8,
            "empty batch is a legal no-op",
            "Ok scores=[]",
            outcome_str(&result),
            ok,
        ));
    }

    // ---------------------------------------------------------------- claim 9
    // dot_batch and l2_batch reduce differently from cosine and have their own
    // result checks, so they get their own known-answer claim.
    //   query=[1..8], candidate=[1..8]
    //   dot = 1+4+9+16+25+36+49+64 = 204
    //   l2  = 0 exactly (identical vectors)
    // Row 1 = query + 1 elementwise -> l2 = 8 * 1^2 = 8 exactly.
    {
        let dim = 8;
        let query: Vec<f32> = (1..=8).map(|v| v as f32).collect();
        let mut candidates = query.clone();
        candidates.extend(query.iter().map(|v| v + 1.0));
        let mut dots = vec![0.0_f32; 2];
        let mut l2s = vec![0.0_f32; 2];
        println!("\n[claim 9] before: dots={dots:?} l2s={l2s:?}");
        let dot_result = dot_batch(&query, &candidates, dim, &mut dots);
        let l2_result = l2_batch(&query, &candidates, dim, &mut l2s);
        println!("[claim 9] after:  dots={dots:?} l2s={l2s:?}");
        let ok = dot_result.is_ok()
            && l2_result.is_ok()
            && dots[0] == 204.0
            && l2s[0] == 0.0
            && l2s[1] == 8.0;
        claims.push(claim(
            9,
            "dot and l2 known answers (dim=8, exactly one lane)",
            "Ok dots[0]=204 l2s[0]=0 l2s[1]=8, all exact",
            format!(
                "dots={dots:?} l2s={l2s:?} dot_ok={} l2_ok={}",
                dot_result.is_ok(),
                l2_result.is_ok()
            ),
            ok,
        ));
    }

    // --------------------------------------------------------------- claim 10
    // paired_cosine_batch reduces three accumulators at once and has two
    // candidate sides, so a non-finite on the RIGHT side must be attributed to
    // the right side, not silently blamed on the left.
    {
        let dim = 16;
        let left = vec![1.0_f32; dim];
        let mut right = vec![1.0_f32; dim];
        right[9] = f32::NEG_INFINITY;
        let mut scores = vec![0.0_f32; 1];
        println!("\n[claim 10] before: right[9]={}", right[9]);
        let result = paired_cosine_batch(&left, &right, 1, dim, &mut scores);
        println!("[claim 10] after:  {}", outcome_str(&result));
        let ok = err_contains(&result, &["candidate element 9", "inf"]);
        claims.push(claim(
            10,
            "paired cosine attributes a non-finite to the side that carries it",
            "Err naming candidate element 9 and inf",
            outcome_str(&result),
            ok,
        ));
    }

    println!("\n--- claims ---");
    let mut all_ok = true;
    for entry in &claims {
        let verdict = if entry.ok { "OK" } else { "FAIL" };
        println!("{:>2} {:<62} {verdict}", entry.id, entry.what);
        if !entry.ok {
            println!("     expected: {}", entry.expected);
            println!("     observed: {}", entry.observed);
        }
        all_ok &= entry.ok;
    }

    println!();
    if all_ok {
        println!(
            "PASS: the fused finiteness guard refuses everything the pre-scan refused, names the \
             exact element, and additionally refuses finite inputs that overflow"
        );
        Ok(())
    } else {
        Err("forge_guard_edge_fsv: a claim failed; see the FAIL rows above".into())
    }
}

fn outcome_str(result: &Result<(), calyx_forge::ForgeError>) -> String {
    match result {
        Ok(()) => "Ok".to_owned(),
        Err(error) => format!("Err {error}"),
    }
}
