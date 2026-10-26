//! The tokenizer of bge-small-en-v1.5: BERT's uncased WordPiece, the
//! steps and rules of Hugging Face's `BertNormalizer`, `BertPreTokenizer`
//! and `WordPiece` (ADR 0053 §3), checked against them on a fixture.
//!
//! One deliberate difference: Hugging Face reads `[CLS]`, `[SEP]` and the
//! other specials written in a text as the special tokens. Here they are
//! text like any other, so a post cannot inject structure.
//!
//! The vocabulary (`vocab.txt`, 30,522 pieces, one per line in id order)
//! is bert-base-uncased's, Apache-2.0, as bge-small-en-v1.5 ships it.

use std::collections::HashMap;
use std::sync::OnceLock;

use icu_normalizer::DecomposingNormalizerBorrowed;
use icu_properties::CodePointMapData;
use icu_properties::props::{GeneralCategory, GeneralCategoryGroup};

const VOCAB: &str = include_str!("vocab.txt");

const CLS: u32 = 101;
const SEP: u32 = 102;
const UNK: u32 = 100;
/// A longer word is `[UNK]` whole.
const MAX_WORD_CHARS: usize = 100;

fn vocab() -> &'static HashMap<&'static str, u32> {
    static VOCAB_IDS: OnceLock<HashMap<&'static str, u32>> = OnceLock::new();
    VOCAB_IDS.get_or_init(|| VOCAB.lines().zip(0u32..).collect())
}

fn category(c: char) -> GeneralCategory {
    CodePointMapData::<GeneralCategory>::new().get(c)
}

/// `\t`, `\n` and `\r` are control characters that count as whitespace.
fn is_whitespace(c: char) -> bool {
    matches!(c, '\t' | '\n' | '\r') || c.is_whitespace()
}

/// Categories C* (control, format, surrogate, private use, unassigned),
/// except the three whitespace controls.
fn is_control(c: char) -> bool {
    !matches!(c, '\t' | '\n' | '\r') && GeneralCategoryGroup::Other.contains(category(c))
}

fn is_punctuation(c: char) -> bool {
    c.is_ascii_punctuation() || GeneralCategoryGroup::Punctuation.contains(category(c))
}

/// The CJK blocks BERT splits into single characters.
fn is_chinese(c: char) -> bool {
    matches!(c as u32,
        0x4E00..=0x9FFF
        | 0x3400..=0x4DBF
        | 0x20000..=0x2A6DF
        | 0x2A700..=0x2B73F
        | 0x2B740..=0x2B81F
        | 0x2B920..=0x2CEAF
        | 0xF900..=0xFAFF
        | 0x2F800..=0x2FA1F)
}

/// `BertNormalizer` (clean text, CJK, strip accents, lowercase).
fn normalize(text: &str) -> String {
    let mut clean = String::with_capacity(text.len());
    for c in text.chars() {
        if c == '\0' || c == '\u{fffd}' || is_control(c) {
            continue;
        }
        if is_chinese(c) {
            clean.push(' ');
            clean.push(c);
            clean.push(' ');
        } else if is_whitespace(c) {
            clean.push(' ');
        } else {
            clean.push(c);
        }
    }
    let decomposed = DecomposingNormalizerBorrowed::new_nfd().normalize(&clean);
    decomposed
        .chars()
        .filter(|&c| category(c) != GeneralCategory::NonspacingMark)
        .flat_map(char::to_lowercase)
        .collect()
}

/// `BertPreTokenizer`: split on whitespace (dropped) and punctuation (kept,
/// one character each).
fn words(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = None;
    for (i, c) in text.char_indices() {
        if is_whitespace(c) || is_punctuation(c) {
            if let Some(s) = start.take() {
                out.push(&text[s..i]);
            }
            if !is_whitespace(c) {
                out.push(&text[i..i + c.len_utf8()]);
            }
        } else if start.is_none() {
            start = Some(i);
        }
    }
    if let Some(s) = start {
        out.push(&text[s..]);
    }
    out
}

/// `WordPiece`, greedy longest match first; `[UNK]` for a word with a piece
/// that is not in the vocabulary.
fn pieces(word: &str, out: &mut Vec<u32>) {
    let vocab = vocab();
    if word.chars().count() > MAX_WORD_CHARS {
        out.push(UNK);
        return;
    }
    let mut found = Vec::new();
    let mut start = 0;
    let mut piece = String::new();
    while start < word.len() {
        let mut end = word.len();
        let mut id = None;
        while start < end {
            piece.clear();
            if start > 0 {
                piece.push_str("##");
            }
            piece.push_str(&word[start..end]);
            if let Some(&i) = vocab.get(piece.as_str()) {
                id = Some(i);
                break;
            }
            // Back one character.
            end = word[..end]
                .char_indices()
                .next_back()
                .map_or(start, |(i, _)| i);
        }
        match id {
            Some(i) => {
                found.push(i);
                start = end;
            }
            None => {
                out.push(UNK);
                return;
            }
        }
    }
    out.extend(found);
}

/// The ids of `text`: `[CLS]`, its pieces, `[SEP]`, at most `max` in all.
pub fn encode(text: &str, max: usize) -> Vec<u32> {
    let normal = normalize(text);
    let mut ids = vec![CLS];
    for word in words(&normal) {
        pieces(word, &mut ids);
        if ids.len() >= max.saturating_sub(1) {
            break;
        }
    }
    ids.truncate(max.saturating_sub(1).max(1));
    ids.push(SEP);
    ids
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The ids Hugging Face's tokenizer gives (truncation at 256) for 100
    /// texts: 40 posts, 40 doc paragraphs and 20 hard cases (accents, CJK,
    /// emoji, controls, long words), except the one with specials written
    /// as text (see the module documentation).
    #[test]
    fn matches_the_reference_tokenizer() {
        let fixture = include_str!("../../tests/data/semantic-fixture.jsonl");
        let mut checked = 0;
        for line in fixture.lines() {
            let v: serde_json::Value = serde_json::from_str(line).unwrap();
            let text = v["text"].as_str().unwrap();
            if text.contains("[CLS]") {
                continue;
            }
            let want: Vec<u32> = v["ids"]
                .as_array()
                .unwrap()
                .iter()
                .map(|i| i.as_u64().unwrap() as u32)
                .collect();
            assert_eq!(encode(text, 256), want, "{text:?}");
            checked += 1;
        }
        assert_eq!(checked, 99);
    }

    #[test]
    fn specials_in_a_text_are_text() {
        let ids = encode("[SEP] x", 256);
        assert_eq!(ids.iter().filter(|&&i| i == SEP).count(), 1);
        assert_eq!(ids.last(), Some(&SEP));
    }

    #[test]
    fn the_vocabulary_is_bert_uncased() {
        assert_eq!(vocab().len(), 30_522);
        assert_eq!(vocab()["[CLS]"], CLS);
        assert_eq!(vocab()["[SEP]"], SEP);
        assert_eq!(vocab()["[UNK]"], UNK);
    }

    #[test]
    fn truncates_with_the_specials() {
        let long = "word ".repeat(400);
        let ids = encode(&long, 256);
        assert_eq!(ids.len(), 256);
        assert_eq!((ids[0], ids[255]), (CLS, SEP));
        assert_eq!(encode("", 256), [CLS, SEP]);
    }
}
