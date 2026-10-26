//! Builds semantic search's background (ADR 0053 §2): the embeddings of a
//! fixed set of texts (paragraphs of wats's own docs), against which each
//! query's cosines are normalised, so the gate means the same in a store
//! of 50 posts and of 50,000. Built with the pinned model; rebuilt only
//! when the model changes.
//!
//! ```sh
//! cargo run --release -p wats --example semantic_background -- \
//!     <models dir>/bge-small-en-v1.5-int8.safetensors \
//!     crates/wats/tests/data/semantic-background.jsonl \
//!     crates/wats/src/semantic/background.bin
//! ```

use anyhow::{Context, Result};
use wats::semantic::{Model, to_blob};

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let usage = "usage: semantic_background <model> <texts.jsonl> <out.bin>";
    let (model, texts, out) = (
        args.next().context(usage)?,
        args.next().context(usage)?,
        args.next().context(usage)?,
    );
    let model = Model::load(&std::fs::read(model)?)?;
    let mut bytes = Vec::new();
    let mut n = 0;
    for line in std::fs::read_to_string(&texts)?.lines() {
        let v: serde_json::Value = serde_json::from_str(line)?;
        let text = v["text"].as_str().context("text")?;
        bytes.extend(to_blob(&model.passage("", text)));
        n += 1;
    }
    std::fs::write(&out, &bytes)?;
    println!("{out}: {n} vectors, {} bytes", bytes.len());
    Ok(())
}
