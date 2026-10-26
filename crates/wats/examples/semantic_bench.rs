//! Semantic search's embedding speed (ADR 0053): ms/post for a model file,
//! one post at a time (queries, 0.5.0's backfill) and in batches
//! (`semantic::EMBED_BATCH`, or `BATCH=`; the backfill since 0.5.1), one
//! thread, 256 tokens, and the process's RSS.
//!
//! ```sh
//! cargo run --release -p wats --example semantic_bench -- <model file> <posts.jsonl> [N]
//! ```
//! `posts.jsonl`: one `{"text": …}` per line.

use std::time::Instant;

use anyhow::{Context, Result};
use wats::semantic::bert::Bert;
use wats::semantic::{MAX_TOKENS, QUERY_PREFIX, tokenizer};

fn rss_mb() -> f64 {
    std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .ok()
        .and_then(|o| {
            String::from_utf8_lossy(&o.stdout)
                .trim()
                .parse::<f64>()
                .ok()
        })
        .map_or(0.0, |kb| kb / 1024.0)
}

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let usage = "usage: semantic_bench <model file> <posts.jsonl> [N]";
    let model_file = args.next().context(usage)?;
    let posts_file = args.next().context(usage)?;
    let n: usize = args.next().map_or(Ok(200), |s| s.parse())?;
    let t = Instant::now();
    // Any model file (the pinned one or another to compare): straight to
    // the forward pass, as `Model` does.
    let model = Bert::load(&std::fs::read(&model_file)?)?;
    let ids = |text: &str| tokenizer::encode(text, MAX_TOKENS);
    println!("loaded in {:?}, RSS {:.0} MB", t.elapsed(), rss_mb());
    let texts: Vec<String> = std::fs::read_to_string(&posts_file)?
        .lines()
        .take(n)
        .map(|l| {
            let v: serde_json::Value = serde_json::from_str(l)?;
            Ok(v["text"].as_str().unwrap_or_default().to_string())
        })
        .collect::<Result<_>>()?;
    let t = Instant::now();
    for text in &texts {
        model.embed(&ids(text));
    }
    let single = t.elapsed().as_secs_f64() * 1000.0 / texts.len() as f64;
    let rss_single = rss_mb();
    let t = Instant::now();
    let batch_size: usize = std::env::var("BATCH")
        .ok()
        .and_then(|b| b.parse().ok())
        .unwrap_or(wats::semantic::EMBED_BATCH);
    for chunk in texts.chunks(batch_size) {
        let batch: Vec<Vec<u32>> = chunk.iter().map(|x| ids(x)).collect();
        let refs: Vec<&[u32]> = batch.iter().map(Vec::as_slice).collect();
        model.embed_batch(&refs);
    }
    let batched = t.elapsed().as_secs_f64() * 1000.0 / texts.len() as f64;
    let t = Instant::now();
    for _ in 0..20 {
        model.embed(&ids(&format!(
            "{QUERY_PREFIX}how do I fix borrow checker error E0502"
        )));
    }
    let query = t.elapsed().as_secs_f64() * 1000.0 / 20.0;
    println!(
        "{} posts: {single:.1} ms/post one at a time, {batched:.1} ms/post in batches of {batch_size}, \
         query {query:.1} ms; RSS {rss_single:.0} MB after the single run, {:.0} MB after the batches",
        texts.len(),
        rss_mb()
    );
    Ok(())
}
