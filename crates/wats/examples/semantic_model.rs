//! Builds semantic search's model file (ADR 0053 §3, §4) from the
//! upstream bge-small-en-v1.5 weights, reproducibly. The file holds:
//! - the matrices of the linear layers and the word embeddings in int8,
//!   per-row symmetric (`semantic::bert::quantize_rows`), each with a
//!   `<name>_scale` tensor (one f32 per row);
//! - the rest in fp16, rounded to nearest even;
//! - tensors sorted by name, no metadata.
//!
//! Anyone can rebuild it and compare its SHA-256 with the one the binary
//! pins (`semantic::MODEL_SHA256`).
//!
//! ```sh
//! curl -LO https://huggingface.co/BAAI/bge-small-en-v1.5/resolve/5c38ec7c405ec4b44b94cc5a9bb96e735b38267a/model.safetensors
//! cargo run --release -p wats --example semantic_model -- model.safetensors bge-small-en-v1.5-int8.safetensors
//! shasum -a 256 bge-small-en-v1.5-int8.safetensors
//! ```

use std::collections::BTreeMap;

use anyhow::{Context, Result, ensure};
use sha2::{Digest, Sha256};
use wats::semantic::bert::{Values, f32_to_f16, quantize_rows, read_safetensors};

/// The upstream file (huggingface.co/BAAI/bge-small-en-v1.5, model.safetensors).
const UPSTREAM_SHA256: &str = "3c9f31665447c8911517620762200d2245a2518d6e7208acc78cd9db317e21ad";

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let (input, output) = (
        args.next()
            .context("usage: semantic_model <upstream> <out>")?,
        args.next()
            .context("usage: semantic_model <upstream> <out>")?,
    );
    let bytes = std::fs::read(&input)?;
    let sha = hex::encode(Sha256::digest(&bytes));
    ensure!(
        sha == UPSTREAM_SHA256,
        "{input}: SHA-256 {sha}, not upstream's"
    );
    // Each tensor's dtype, shape and bytes, sorted by name.
    let mut tensors: BTreeMap<String, (&str, Vec<usize>, Vec<u8>)> = BTreeMap::new();
    for (name, (shape, values)) in read_safetensors(&bytes)? {
        // The pooler is not used: the embedding is the [CLS] output.
        if name.starts_with("pooler.") {
            continue;
        }
        let Values::F32(values) = values else {
            continue;
        };
        let int8 = name == "embeddings.word_embeddings.weight"
            || (name.starts_with("encoder.") && name.ends_with(".weight") && shape.len() == 2);
        if int8 {
            let cols = shape[1];
            let (q, scales) = quantize_rows(&values, cols);
            tensors.insert(
                format!("{name}_scale"),
                (
                    "F32",
                    vec![scales.len()],
                    scales.iter().flat_map(|s| s.to_le_bytes()).collect(),
                ),
            );
            tensors.insert(name, ("I8", shape, q.iter().map(|v| *v as u8).collect()));
        } else {
            let bytes = values
                .iter()
                .flat_map(|v| f32_to_f16(*v).to_le_bytes())
                .collect();
            tensors.insert(name, ("F16", shape, bytes));
        }
    }
    let mut header = serde_json::Map::new();
    let mut data = Vec::new();
    for (name, (dtype, shape, bytes)) in &tensors {
        let start = data.len();
        data.extend_from_slice(bytes);
        header.insert(
            name.clone(),
            serde_json::json!({"dtype": dtype, "shape": shape, "data_offsets": [start, data.len()]}),
        );
    }
    // serde_json's map is sorted: the header is canonical.
    let mut json = serde_json::to_vec(&header)?;
    // Padded with spaces to 8 bytes, as the format recommends.
    while json.len() % 8 != 0 {
        json.push(b' ');
    }
    let mut out = Vec::with_capacity(8 + json.len() + data.len());
    out.extend_from_slice(&(json.len() as u64).to_le_bytes());
    out.extend_from_slice(&json);
    out.extend_from_slice(&data);
    std::fs::write(&output, &out)?;
    println!(
        "{output}: {} tensors, {} bytes, SHA-256 {}",
        tensors.len(),
        out.len(),
        hex::encode(Sha256::digest(&out))
    );
    Ok(())
}
