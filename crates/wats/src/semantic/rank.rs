//! Which vector hits count, and how they fuse with full-text search (ADR
//! 0053 §2). Pure functions: the store and the model are elsewhere.
//!
//! - **The gate, per query.** The node compares the query with every
//!   stored vector. A hit counts when its cosine's z-score in that
//!   distribution is at least [`Z_MIN`], and the cosine at least
//!   [`FLOOR`].
//! - **Fusion.** Reciprocal rank fusion over the full-text list and the
//!   gated vector list, the full-text list counting [`CODE_WEIGHT`] times
//!   when the query has a code-like token ([`code_like`]).
//! - **Labels.** Each hit says how it matched ([`Match`]).
//!
//! The constants are tied to the model ([`super::MODEL_SHA256`]),
//! calibrated on `tests/data/semantic-pairs.jsonl`.

/// A vector hit counts from this z-score.
pub const Z_MIN: f32 = 4.0;
/// Below this z-score, a hit by meaning is weak.
pub const Z_WEAK: f32 = 5.0;
/// No hit under this cosine: only drops absurd ones.
pub const FLOOR: f32 = 0.55;
/// Reciprocal rank fusion's constant.
pub const RRF_K: f64 = 60.0;
/// How much the full-text list counts for a query with a code-like token.
pub const CODE_WEIGHT: f64 = 2.0;

/// How a hit matched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Match {
    /// Full-text search only.
    Words,
    /// Vectors only, with a z-score of at least [`Z_WEAK`].
    Meaning,
    /// Vectors only, below [`Z_WEAK`].
    MeaningWeak,
    /// Both lists.
    Both,
}

impl Match {
    pub fn as_str(self) -> &'static str {
        match self {
            Match::Words => "words",
            Match::Meaning => "meaning",
            Match::MeaningWeak => "meaning (weak)",
            Match::Both => "both",
        }
    }
}

/// A post the vectors found.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VectorHit {
    pub seq: i64,
    pub cos: f32,
    pub z: f32,
}

/// The gated vector hits among `scores` (each post's cosine with the
/// query), best first, at most `k`.
pub fn gate(scores: &[(i64, f32)], k: usize) -> Vec<VectorHit> {
    let n = scores.len();
    if n < 2 {
        return Vec::new();
    }
    let mean = scores.iter().map(|s| f64::from(s.1)).sum::<f64>() / n as f64;
    let var = scores
        .iter()
        .map(|s| (f64::from(s.1) - mean).powi(2))
        .sum::<f64>()
        / n as f64;
    let std = var.sqrt();
    if std <= f64::EPSILON {
        return Vec::new();
    }
    let mut hits: Vec<VectorHit> = scores
        .iter()
        .map(|&(seq, cos)| VectorHit {
            seq,
            cos,
            z: ((f64::from(cos) - mean) / std) as f32,
        })
        .filter(|h| h.z >= Z_MIN && h.cos >= FLOOR)
        .collect();
    hits.sort_by(|a, b| b.cos.total_cmp(&a.cos).then(a.seq.cmp(&b.seq)));
    hits.truncate(k);
    hits
}

/// A token that full-text search should keep strong: letters and digits
/// mixed (`E0502`, `v1.2.3`), `::`, `_`, a leading `-`, or inner
/// capitals (`HashMap`).
pub fn code_like(query: &str) -> bool {
    query.split_whitespace().any(|token| {
        let token = token
            .trim_matches(|c: char| matches!(c, ',' | ';' | '?' | '!' | '"' | '\'' | '(' | ')'));
        let letters = token.chars().any(char::is_alphabetic);
        let digits = token.chars().any(|c| c.is_ascii_digit());
        let inner_capital = token.char_indices().any(|(i, c)| i > 0 && c.is_uppercase())
            && token.chars().any(char::is_lowercase);
        (letters && digits)
            || token.contains("::")
            || token.contains('_')
            || (token.starts_with('-') && token.len() > 1)
            || inner_capital
    })
}

/// A fused hit: the post, its fusion score, how it matched, and its
/// vector hit when it has one.
#[derive(Debug, Clone, PartialEq)]
pub struct Fused {
    pub seq: i64,
    pub score: f64,
    pub matched: Match,
    pub vector: Option<VectorHit>,
}

/// Reciprocal rank fusion of the full-text list (best first) and the
/// gated vector hits; `code` when the query is code-like. Best first.
pub fn fuse(words: &[i64], vectors: &[VectorHit], code: bool) -> Vec<Fused> {
    let weight = if code { CODE_WEIGHT } else { 1.0 };
    let mut out: Vec<Fused> = Vec::with_capacity(words.len() + vectors.len());
    for (rank, &seq) in words.iter().enumerate() {
        if out.iter().any(|f| f.seq == seq) {
            continue;
        }
        out.push(Fused {
            seq,
            score: weight / (RRF_K + rank as f64 + 1.0),
            matched: Match::Words,
            vector: None,
        });
    }
    for (rank, hit) in vectors.iter().enumerate() {
        let score = 1.0 / (RRF_K + rank as f64 + 1.0);
        match out.iter_mut().find(|f| f.seq == hit.seq) {
            Some(f) => {
                f.score += score;
                f.matched = Match::Both;
                f.vector = Some(*hit);
            }
            None => out.push(Fused {
                seq: hit.seq,
                score,
                matched: if hit.z >= Z_WEAK {
                    Match::Meaning
                } else {
                    Match::MeaningWeak
                },
                vector: Some(*hit),
            }),
        }
    }
    out.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.seq.cmp(&b.seq)));
    out
}

/// Whether two fusion scores are a near-tie (within 5 %), where the
/// quality order may swap them (ADR 0053 §2).
pub fn near_tie(a: f64, b: f64) -> bool {
    let (hi, lo) = if a >= b { (a, b) } else { (b, a) };
    hi <= 0.0 || lo / hi >= 0.95
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 1000 posts around 0.6, one at 0.85 (stands out), one at 0.66
    /// (does not).
    #[test]
    fn the_gate_keeps_what_stands_out() {
        let mut scores: Vec<(i64, f32)> = (0..1000)
            .map(|i| (i, 0.6 + ((i % 7) as f32 - 3.0) * 0.01))
            .collect();
        scores.push((5000, 0.85));
        scores.push((5001, 0.66));
        let hits = gate(&scores, 50);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].seq, 5000);
        assert!(hits[0].z > Z_WEAK);
        // Everything alike: nothing stands out.
        assert!(gate(&[(1, 0.7), (2, 0.7), (3, 0.7)], 50).is_empty());
        assert!(gate(&[(1, 0.9)], 50).is_empty());
    }

    #[test]
    fn the_floor_drops_absurd_hits() {
        // A z-score that stands out, but a cosine under the floor.
        let mut scores: Vec<(i64, f32)> = (0..1000)
            .map(|i| (i, 0.2 + (i % 5) as f32 * 0.005))
            .collect();
        scores.push((9, 0.5));
        assert!(gate(&scores, 50).is_empty());
    }

    #[test]
    fn code_like_tokens() {
        for q in [
            "error E0502 in borrow",
            "upgrade to v1.2.3",
            "std::mem::take",
            "snake_case name",
            "the --release flag",
            "a HashMap of",
        ] {
            assert!(code_like(q), "{q}");
        }
        for q in [
            "how do I sort a list",
            "What is Kotlin?",
            "Rust's latest version",
            "Hello World",
            "a - b",
        ] {
            assert!(!code_like(q), "{q}");
        }
    }

    #[test]
    fn fusion_labels_and_orders() {
        let hit = |seq, z| VectorHit { seq, cos: 0.8, z };
        let fused = fuse(&[1, 2, 3], &[hit(2, 6.0), hit(4, 4.5), hit(5, 7.0)], false);
        let order: Vec<i64> = fused.iter().map(|f| f.seq).collect();
        // 2 is in both lists: first.
        assert_eq!(order[0], 2);
        assert_eq!(fused[0].matched, Match::Both);
        let label = |seq| fused.iter().find(|f| f.seq == seq).unwrap().matched;
        assert_eq!(label(1), Match::Words);
        assert_eq!(label(4), Match::MeaningWeak);
        assert_eq!(label(5), Match::Meaning);
        // A code-like query: the words list counts twice, so the first
        // word hit beats the first vector-only hit.
        let fused = fuse(&[1], &[hit(9, 8.0)], true);
        assert_eq!(fused[0].seq, 1);
        let fused = fuse(&[1], &[hit(9, 8.0)], false);
        assert_eq!(fused[0].score, fused[1].score);
    }

    #[test]
    fn near_ties() {
        assert!(near_tie(1.0 / 61.0, 1.0 / 62.0));
        assert!(!near_tie(2.0 / 61.0, 1.0 / 61.0));
    }
}
