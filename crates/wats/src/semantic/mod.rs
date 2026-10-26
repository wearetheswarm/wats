//! Semantic search (ADR 0053): bge-small-en-v1.5 embeddings, computed on
//! this node and never shared, fused with full-text search.
//!
//! - [`tokenizer`]: BERT's uncased WordPiece.
//! - [`bert`]: the model's forward pass.
//! - [`rank`]: the per-query gate, the fusion and the labels.
//! - [`Model`] and [`Index`] (here): a loaded model and the vectors in
//!   memory.

pub mod bert;
pub mod engine;
pub mod find;
pub mod rank;
pub mod tokenizer;

use std::sync::OnceLock;

use anyhow::{Result, ensure};
use sha2::{Digest, Sha256};

pub use bert::DIM;

/// SHA-256 of the model file, built from upstream by
/// `examples/semantic_model.rs`, and published as a release asset.
pub const MODEL_SHA256: &str = "2d3e18fede3cfbf614b9ec4de9c3a933c0fd94e481f6a3c936c51d49d0e094c0";
/// Its size in bytes.
pub const MODEL_BYTES: u64 = 33_788_328;
/// Its file name, locally and as a release asset.
pub const MODEL_FILE: &str = "bge-small-en-v1.5-int8.safetensors";
/// A post is embedded from its first tokens: title, then body.
pub const MAX_TOKENS: usize = 256;
/// Posts embedded in one pass: each int8 layer is dequantized once per
/// pass. 8 is as fast as 16 (23 ms/post on an M4 Pro), with 70 MB less
/// RAM for the activations.
pub const EMBED_BATCH: usize = 8;
/// bge's instruction for queries; never for passages.
pub const QUERY_PREFIX: &str = "Represent this sentence for searching relevant passages: ";

/// The loaded model.
pub struct Model {
    bert: bert::Bert,
}

impl Model {
    /// Loads the model from the bytes of its file, refused unless they
    /// are the pinned file.
    pub fn load(bytes: &[u8]) -> Result<Self> {
        let sha = hex::encode(Sha256::digest(bytes));
        ensure!(
            sha == MODEL_SHA256,
            "model file: SHA-256 {sha}, not {MODEL_SHA256}"
        );
        Ok(Model {
            bert: bert::Bert::load(bytes)?,
        })
    }

    /// The embedding of a post: its title, then its body.
    pub fn passage(&self, title: &str, text: &str) -> Vec<f32> {
        self.passages(&[(title, text)]).pop().unwrap_or_default()
    }

    /// The embeddings of several posts in one pass (the backfill's
    /// batches): each int8 layer is dequantized once for all of them.
    pub fn passages(&self, posts: &[(&str, &str)]) -> Vec<Vec<f32>> {
        let ids: Vec<Vec<u32>> = posts
            .iter()
            .map(|(title, text)| {
                let input = if title.trim().is_empty() {
                    (*text).to_string()
                } else {
                    format!("{title}\n{text}")
                };
                tokenizer::encode(&input, MAX_TOKENS)
            })
            .collect();
        ids.chunks(EMBED_BATCH)
            .flat_map(|chunk| {
                let refs: Vec<&[u32]> = chunk.iter().map(Vec::as_slice).collect();
                self.bert.embed_batch(&refs)
            })
            .collect()
    }

    /// The embedding of a search query.
    pub fn query(&self, query: &str) -> Vec<f32> {
        self.bert.embed(&tokenizer::encode(
            &format!("{QUERY_PREFIX}{query}"),
            MAX_TOKENS,
        ))
    }
}

/// A vector as stored: 384 fp16 values, little-endian (768 bytes).
pub fn to_blob(v: &[f32]) -> Vec<u8> {
    v.iter()
        .flat_map(|x| bert::f32_to_f16(*x).to_le_bytes())
        .collect()
}

/// The background of the gate (ADR 0053 §2): 596 paragraphs of wats's
/// docs (`tests/data/semantic-background.jsonl`), embedded with the
/// pinned model by `examples/semantic_background.rs`.
pub fn background() -> &'static Index {
    static BACKGROUND: OnceLock<Index> = OnceLock::new();
    BACKGROUND.get_or_init(|| {
        let bytes = include_bytes!("background.bin");
        let mut index = Index::default();
        for (i, blob) in bytes.as_chunks::<{ DIM * 2 }>().0.iter().enumerate() {
            index.push(i as i64, blob);
        }
        index
    })
}

/// fp16 bits to f32, through a table (search reads every vector).
fn f16_table() -> &'static [f32] {
    static TABLE: OnceLock<Vec<f32>> = OnceLock::new();
    TABLE.get_or_init(|| (0..=u16::MAX).map(bert::f16_to_f32).collect())
}

/// The vectors in memory: each post's `seq` and its fp16 values.
#[derive(Default)]
pub struct Index {
    seqs: Vec<i64>,
    values: Vec<u16>,
}

impl Index {
    pub fn len(&self) -> usize {
        self.seqs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.seqs.is_empty()
    }

    /// Adds a stored vector (`to_blob`'s bytes); a blob of the wrong size
    /// is skipped.
    pub fn push(&mut self, seq: i64, blob: &[u8]) -> bool {
        if blob.len() != DIM * 2 {
            return false;
        }
        self.seqs.push(seq);
        self.values.extend(
            blob.as_chunks::<2>()
                .0
                .iter()
                .map(|c| u16::from_le_bytes(*c)),
        );
        true
    }

    /// Drops the vectors of these posts.
    pub fn remove(&mut self, seqs: &std::collections::HashSet<i64>) {
        let mut keep_seqs = Vec::with_capacity(self.seqs.len());
        let mut keep_values = Vec::with_capacity(self.values.len());
        for (seq, v) in self.seqs.iter().zip(self.values.as_chunks::<DIM>().0) {
            if !seqs.contains(seq) {
                keep_seqs.push(*seq);
                keep_values.extend_from_slice(v);
            }
        }
        (self.seqs, self.values) = (keep_seqs, keep_values);
    }

    /// Each post's cosine with `query` (an embedding, normalised).
    pub fn scores(&self, query: &[f32]) -> Vec<(i64, f32)> {
        let table = f16_table();
        self.seqs
            .iter()
            .zip(self.values.as_chunks::<DIM>().0)
            .map(|(seq, v)| {
                let dot = v
                    .iter()
                    .zip(query)
                    .map(|(h, q)| table[usize::from(*h)] * q)
                    .sum::<f32>();
                (*seq, dot)
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_index_scores_and_forgets() {
        let unit = |i: usize| {
            let mut v = vec![0.0f32; DIM];
            v[i] = 1.0;
            v
        };
        let mut index = Index::default();
        assert!(index.push(1, &to_blob(&unit(0))));
        assert!(index.push(2, &to_blob(&unit(1))));
        assert!(!index.push(3, &[0u8; 10]));
        let scores = index.scores(&unit(1));
        assert_eq!(scores, [(1, 0.0), (2, 1.0)]);
        index.remove(&[1].into_iter().collect());
        assert_eq!(index.len(), 1);
        assert_eq!(index.scores(&unit(1)), [(2, 1.0)]);
    }

    #[test]
    fn the_background_is_there() {
        assert_eq!(background().len(), 596);
        // Unit vectors.
        let first = background().scores(&[0.0; DIM]);
        assert_eq!(first.len(), 596);
    }

    #[test]
    fn a_wrong_model_file_is_refused() {
        let err = Model::load(b"not the model").err().unwrap();
        assert!(err.to_string().contains("not 2d3e18fe"), "{err}");
    }
}
