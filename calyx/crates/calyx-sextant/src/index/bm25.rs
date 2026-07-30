//! BM25 scorer using Lucene-like defaults.

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Bm25 {
    pub k1: f32,
    pub b: f32,
}

impl Default for Bm25 {
    fn default() -> Self {
        Self { k1: 1.2, b: 0.75 }
    }
}

impl Bm25 {
    /// The exact scoring law this instance applies, with its live parameters
    /// interpolated.
    ///
    /// A caller must be able to read which law produced a rank rather than
    /// infer it from an index kind, the same way a fused report states its RRF
    /// formula (#1900).
    #[must_use]
    pub fn law(self) -> String {
        format!(
            "bm25: score(d) = SUM_t qtf_t * idf_t * (tf_td*(k1+1)) / (tf_td + k1*(1-b+b*dl_d/avgdl)) with k1={}, b={}, idf_t = ln(1 + (N-df_t+0.5)/(df_t+0.5))",
            self.k1, self.b
        )
    }

    pub fn idf(self, total_docs: usize, doc_freq: usize) -> f32 {
        (((total_docs as f32 - doc_freq as f32 + 0.5) / (doc_freq as f32 + 0.5)) + 1.0).ln()
    }

    pub fn score_term(
        self,
        tf: f32,
        doc_len: f32,
        avg_doc_len: f32,
        total_docs: usize,
        doc_freq: usize,
    ) -> f32 {
        if !tf.is_finite()
            || tf <= 0.0
            || !doc_len.is_finite()
            || doc_len < 0.0
            || !avg_doc_len.is_finite()
            || avg_doc_len < 0.0
            || total_docs == 0
            || doc_freq == 0
        {
            return 0.0;
        }
        let len_norm = if avg_doc_len <= 0.0 {
            1.0
        } else {
            doc_len / avg_doc_len
        };
        let denom = tf + self.k1 * (1.0 - self.b + self.b * len_norm);
        self.idf(total_docs, doc_freq) * (tf * (self.k1 + 1.0)) / denom
    }
}
