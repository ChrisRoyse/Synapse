//! Manual FSV instrument for #1902: is BM25's document-length saturation inert
//! on an L1-normalized lane, and does the raw-count lane restore it?
//!
//! ## The discriminator
//!
//! BM25 scores a term as
//!
//! ```text
//!   idf * tf*(k1+1) / (tf + k1*(1 - b + b*dl/avgdl))
//! ```
//!
//! `b` and `avgdl` exist to stop a short document from winning on a term it
//! shares with a long one. They act only through `dl/avgdl`. An encoder that
//! L1-normalizes makes every row's weight sum exactly 1.0, so `dl == avgdl` for
//! **every** document, `dl/avgdl == 1`, and the whole correction collapses to
//! the constant `k1`. That is the #1902 claim, and it is checkable directly:
//! **run the same corpus at `b=0.75` and at `b=0.0` and compare the scores.**
//! On a working lane they must differ. On an inert lane they are identical —
//! which is exactly what "`b` is operationally 0" means.
//!
//! ## The corpus
//!
//! Three documents of deliberately different lengths sharing one common term,
//! per the issue's verification note. Every token is chosen so the expected
//! answer is known before anything is measured:
//!
//! | doc    | text                      | tokens | count of `alpha` |
//! |--------|---------------------------|--------|------------------|
//! | short  | `alpha`                   | 1      | 1                |
//! | medium | `alpha` + 9 filler words  | 10     | 1                |
//! | long   | `alpha` + 99 filler words | 100    | 1                |
//!
//! All three contain `alpha` exactly once, so `tf` is equal across them and the
//! *only* thing that can separate them is the length term. A working BM25 must
//! rank `short > medium > long`; an inert one must tie all three exactly.
//!
//! ## What is read, and from where
//!
//! Both encoders are the **production** ones, called through the real
//! `AlgorithmicLens::measure` path, and the scorer is the **production**
//! `calyx_sextant::index::bm25::Bm25`. Nothing here reimplements either.
//!
//! Usage:
//! `cargo run -p synapse-calyx --example bm25_length_saturation_fsv`

use std::error::Error;

use calyx_core::{Input, Lens as _, Modality, SlotVector};
use calyx_registry::AlgorithmicLens;
use calyx_sextant::index::bm25::Bm25;

const DIM: u32 = 2048;
const SHARED_TERM: &str = "alpha";

/// A document of `filler` extra unique words plus the one shared term.
fn document(filler: usize) -> String {
    use std::fmt::Write as _;
    let mut text = String::from(SHARED_TERM);
    for i in 0..filler {
        let _ = write!(text, " w{i:04}");
    }
    text
}

/// `usize -> f32` for a document count. The corpus is three documents, far
/// inside f32's exact-integer range, so this is lossless here; `u16` bounds it
/// where the compiler can see that.
fn f32_len(count: usize) -> f32 {
    f32::from(u16::try_from(count).unwrap_or(u16::MAX))
}

fn measure(lens: &AlgorithmicLens, text: &str) -> Result<Vec<(u32, f32)>, Box<dyn Error>> {
    let vector = lens.measure(&Input::new(Modality::Text, text.as_bytes().to_vec()))?;
    let SlotVector::Sparse { entries, .. } = vector else {
        return Err("expected a sparse vector".into());
    };
    Ok(entries.into_iter().map(|e| (e.idx, e.val)).collect())
}

/// The bucket the shared term lands in, taken from a document that contains
/// *only* that term — so the cell is identified without assuming a hash.
fn shared_cell(lens: &AlgorithmicLens) -> Result<u32, Box<dyn Error>> {
    let only = measure(lens, SHARED_TERM)?;
    if only.len() != 1 {
        return Err(format!("single-term document produced {} cells", only.len()).into());
    }
    Ok(only[0].0)
}

struct Lane {
    label: &'static str,
    docs: Vec<(&'static str, Vec<(u32, f32)>)>,
    cell: u32,
}

impl Lane {
    fn build(label: &'static str, lens: &AlgorithmicLens) -> Result<Self, Box<dyn Error>> {
        let cell = shared_cell(lens)?;
        let docs = vec![
            ("short_1tok", measure(lens, &document(0))?),
            ("medium_10tok", measure(lens, &document(9))?),
            ("long_100tok", measure(lens, &document(99))?),
        ];
        Ok(Self { label, docs, cell })
    }

    fn doc_len(&self, index: usize) -> f32 {
        self.docs[index].1.iter().map(|(_, val)| val).sum()
    }

    fn avg_doc_len(&self) -> f32 {
        let total: f32 = (0..self.docs.len()).map(|i| self.doc_len(i)).sum();
        total / f32_len(self.docs.len())
    }

    fn shared_tf(&self, index: usize) -> f32 {
        self.docs[index]
            .1
            .iter()
            .find(|(idx, _)| *idx == self.cell)
            .map_or(0.0, |(_, val)| *val)
    }

    fn report(&self) {
        println!("\n=== lane {} ===", self.label);
        println!("shared term {SHARED_TERM:?} -> cell {}", self.cell);
        for (i, (name, entries)) in self.docs.iter().enumerate() {
            println!(
                "  {name:<13} cells={:<4} doc_len={:<10} tf({SHARED_TERM})={}",
                entries.len(),
                self.doc_len(i),
                self.shared_tf(i)
            );
        }
        println!("  avg_doc_len={}", self.avg_doc_len());

        let avg = self.avg_doc_len();
        let total_docs = self.docs.len();
        // Every document carries the shared term, so df == N for that term.
        let df = total_docs;
        for (b_label, scorer) in [
            ("b=0.75", Bm25 { k1: 1.2, b: 0.75 }),
            ("b=0.00", Bm25 { k1: 1.2, b: 0.0 }),
        ] {
            let scores = (0..self.docs.len())
                .map(|i| scorer.score_term(self.shared_tf(i), self.doc_len(i), avg, total_docs, df))
                .collect::<Vec<_>>();
            println!(
                "  {b_label}: short={:.8} medium={:.8} long={:.8}",
                scores[0], scores[1], scores[2]
            );
        }

        let scores_b = |b: f32| {
            let scorer = Bm25 { k1: 1.2, b };
            (0..self.docs.len())
                .map(|i| scorer.score_term(self.shared_tf(i), self.doc_len(i), avg, total_docs, df))
                .collect::<Vec<f32>>()
        };
        let at_default = scores_b(0.75);
        let at_zero = scores_b(0.0);

        // A bare "did any bit change" is NOT a discriminator here: f32 rounding
        // alone flips a last-ULP bit on the inert lane, so that test reports
        // "b acts" on precisely the lane where it does not. The magnitude is
        // the discriminator. On a working lane `b` is the whole difference
        // between "every document tied" and "ordered by length"; on an inert
        // lane it is float noise.
        let max_relative_change = at_default
            .iter()
            .zip(&at_zero)
            .map(|(with_b, without_b)| {
                if *without_b == 0.0 {
                    0.0
                } else {
                    ((with_b - without_b) / without_b).abs()
                }
            })
            .fold(0.0_f32, f32::max);
        let b_acts = max_relative_change > 1e-3;

        // How badly a 1-token document beats a 100-token one on their shared
        // term. Unbounded short-document dominance is the failure #1902 and
        // #1900 both describe; `b` exists to bound it.
        let short_over_long = if at_default[2] == 0.0 {
            f32::INFINITY
        } else {
            at_default[0] / at_default[2]
        };
        let all_tied_without_b = at_zero
            .windows(2)
            .all(|pair| pair[0].to_bits() == pair[1].to_bits());

        println!(
            "  VERDICT b_acts={b_acts} (max_relative_score_change_from_b={max_relative_change:.3e}) \
             short_over_long={short_over_long:.2}x tied_at_b0={all_tied_without_b}"
        );
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    println!("bm25_length_saturation_fsv: does `b` act on each lane?");
    println!("corpus: 1 / 10 / 100 tokens, each containing {SHARED_TERM:?} exactly once");

    let normalized =
        AlgorithmicLens::sparse_keywords("fsv.sparse_keywords.v1", Modality::Text, DIM);
    let raw_count =
        AlgorithmicLens::sparse_keywords_tf("fsv.sparse_keywords_tf.v1", Modality::Text, DIM);

    Lane::build("sparse_keywords (L1-normalized)", &normalized)?.report();
    Lane::build("sparse_keywords_tf (raw counts)", &raw_count)?.report();

    Ok(())
}
