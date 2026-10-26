//! A BERT forward pass on gemm (or Accelerate), compared with candle.
//! bertproto <model.safetensors> <tokenizer.json> <corpus.jsonl> <candle.vec> [N]

use std::collections::HashMap;
use std::io::BufRead;
use std::time::Instant;

use anyhow::{Context, Result};

const H: usize = 384;
const HEADS: usize = 12;
const HD: usize = 32;
const FF: usize = 1536;
const LAYERS: usize = 12;
const EPS: f32 = 1e-12;

fn f16_to_f32(h: u16) -> f32 {
    let sign = ((h >> 15) as u32) << 31;
    let exp = ((h >> 10) & 0x1f) as u32;
    let frac = (h & 0x3ff) as u32;
    let bits = if exp == 0 {
        if frac == 0 {
            sign
        } else {
            // Subnormal: normalise.
            let mut e = 127 - 15 + 1;
            let mut f = frac;
            while f & 0x400 == 0 {
                f <<= 1;
                e -= 1;
            }
            sign | ((e as u32) << 23) | ((f & 0x3ff) << 13)
        }
    } else if exp == 0x1f {
        sign | 0x7f80_0000 | (frac << 13)
    } else {
        sign | ((exp + 127 - 15) << 23) | (frac << 13)
    };
    f32::from_bits(bits)
}

fn load(path: &str) -> Result<HashMap<String, Vec<f32>>> {
    let bytes = std::fs::read(path)?;
    let n = u64::from_le_bytes(bytes[..8].try_into()?) as usize;
    let header: serde_json::Value = serde_json::from_slice(&bytes[8..8 + n])?;
    let data = &bytes[8 + n..];
    let mut out = HashMap::new();
    for (name, t) in header.as_object().context("header")? {
        if name == "__metadata__" {
            continue;
        }
        let dtype = t["dtype"].as_str().context("dtype")?;
        let off = &t["data_offsets"];
        let (a, b) = (off[0].as_u64().context("off")? as usize, off[1].as_u64().context("off")? as usize);
        let raw = &data[a..b];
        let v: Vec<f32> = match dtype {
            "F32" => raw.chunks_exact(4).map(|c| f32::from_le_bytes(c.try_into().unwrap())).collect(),
            "F16" => raw.chunks_exact(2).map(|c| f16_to_f32(u16::from_le_bytes(c.try_into().unwrap()))).collect(),
            _ => continue,
        };
        out.insert(name.clone(), v);
    }
    Ok(out)
}

/// c[m×n] = a[m×k] (row stride lda) × b, b given as [n×k] rows (transposed,
/// stride ldb) when `bt`, else [k×n] rows (stride ldb). c row stride ldc.
#[allow(clippy::too_many_arguments)]
fn sgemm(m: usize, n: usize, k: usize, a: &[f32], lda: usize, b: &[f32], ldb: usize, bt: bool, c: &mut [f32], ldc: usize) {
    if std::env::var("ACCEL").is_ok() {
        #[cfg(target_os = "macos")]
        unsafe {
            cblas_sgemm(101, 111, if bt { 112 } else { 111 }, m as i32, n as i32, k as i32, 1.0, a.as_ptr(), lda as i32, b.as_ptr(), ldb as i32, 0.0, c.as_mut_ptr(), ldc as i32);
            return;
        }
    }
    let (rhs_cs, rhs_rs) = if bt { (ldb as isize, 1) } else { (1, ldb as isize) };
    unsafe {
        gemm::gemm(
            m, n, k, c.as_mut_ptr(), 1, ldc as isize, false, a.as_ptr(), 1, lda as isize, b.as_ptr(), rhs_cs, rhs_rs,
            0.0, 1.0, false, false, false, gemm::Parallelism::None,
        );
    }
}

#[cfg(target_os = "macos")]
#[link(name = "Accelerate", kind = "framework")]
unsafe extern "C" {
    fn cblas_sgemm(order: i32, ta: i32, tb: i32, m: i32, n: i32, k: i32, alpha: f32, a: *const f32, lda: i32, b: *const f32, ldb: i32, beta: f32, c: *mut f32, ldc: i32);
}

fn layer_norm(x: &mut [f32], w: &[f32], b: &[f32]) {
    for row in x.chunks_exact_mut(H) {
        let mean = row.iter().sum::<f32>() / H as f32;
        let var = row.iter().map(|v| (v - mean) * (v - mean)).sum::<f32>() / H as f32;
        let inv = 1.0 / (var + EPS).sqrt();
        for (i, v) in row.iter_mut().enumerate() {
            *v = (*v - mean) * inv * w[i] + b[i];
        }
    }
}

/// erf, max error ~1.2e-7 (Numerical Recipes erfc, Chebyshev).
fn erf(x: f32) -> f32 {
    let x = x as f64;
    let z = x.abs();
    let t = 1.0 / (1.0 + 0.5 * z);
    let r = t * (-z * z - 1.26551223
        + t * (1.00002368 + t * (0.37409196 + t * (0.09678418 + t * (-0.18628806
        + t * (0.27886807 + t * (-1.13520398 + t * (1.48851587 + t * (-0.82215223 + t * 0.17087277)))))))))
        .exp();
    (if x >= 0.0 { 1.0 - r } else { r - 1.0 }) as f32
}

struct Bert {
    w: HashMap<String, Vec<f32>>,
    q: HashMap<String, (Vec<i8>, Vec<f32>)>,
}

impl Bert {
    fn g(&self, k: &str) -> &[f32] {
        self.w.get(k).unwrap_or_else(|| panic!("{k}"))
    }

    fn linear(&self, x: &[f32], n: usize, inp: usize, out: usize, name: &str) -> Vec<f32> {
        // int8 on the fly: dequantize the row-quantized weights each call.
        let deq;
        let w = match self.q.get(name) {
            Some((q, scale)) => {
                deq = q.chunks_exact(inp).zip(scale).flat_map(|(r, s)| r.iter().map(move |v| *v as f32 * s)).collect::<Vec<f32>>();
                &deq[..]
            }
            None => self.g(&format!("{name}.weight")),
        };
        let b = self.g(&format!("{name}.bias"));
        let mut y = vec![0.0; n * out];
        sgemm(n, out, inp, x, inp, w, inp, true, &mut y, out);
        for row in y.chunks_exact_mut(out) {
            for (v, bb) in row.iter_mut().zip(b) {
                *v += bb;
            }
        }
        y
    }

    fn embed(&self, ids: &[u32]) -> Vec<f32> {
        let n = ids.len();
        let (we, pe, te) = (self.g("embeddings.word_embeddings.weight"), self.g("embeddings.position_embeddings.weight"), self.g("embeddings.token_type_embeddings.weight"));
        let mut x = vec![0.0f32; n * H];
        for (i, &id) in ids.iter().enumerate() {
            let id = id as usize;
            for d in 0..H {
                x[i * H + d] = we[id * H + d] + pe[i * H + d] + te[d];
            }
        }
        layer_norm(&mut x, self.g("embeddings.LayerNorm.weight"), self.g("embeddings.LayerNorm.bias"));
        let scale = 1.0 / (HD as f32).sqrt();
        let mut scores = vec![0.0f32; n * n];
        for l in 0..LAYERS {
            let p = format!("encoder.layer.{l}");
            let q = self.linear(&x, n, H, H, &format!("{p}.attention.self.query"));
            let k = self.linear(&x, n, H, H, &format!("{p}.attention.self.key"));
            let v = self.linear(&x, n, H, H, &format!("{p}.attention.self.value"));
            let mut ctx = vec![0.0f32; n * H];
            for h in 0..HEADS {
                let o = h * HD;
                sgemm(n, n, HD, &q[o..], H, &k[o..], H, true, &mut scores, n);
                for row in scores.chunks_exact_mut(n) {
                    let max = row.iter().cloned().fold(f32::MIN, f32::max);
                    let mut sum = 0.0;
                    for s in row.iter_mut() {
                        *s = ((*s - max) * scale).exp();
                        sum += *s;
                    }
                    for s in row.iter_mut() {
                        *s /= sum;
                    }
                }
                sgemm(n, HD, n, &scores, n, &v[o..], H, false, &mut ctx[o..], H);
            }
            let a = self.linear(&ctx, n, H, H, &format!("{p}.attention.output.dense"));
            for (xi, ai) in x.iter_mut().zip(&a) {
                *xi += ai;
            }
            layer_norm(&mut x, self.g(&format!("{p}.attention.output.LayerNorm.weight")), self.g(&format!("{p}.attention.output.LayerNorm.bias")));
            let mut f = self.linear(&x, n, H, FF, &format!("{p}.intermediate.dense"));
            for v in f.iter_mut() {
                *v = 0.5 * *v * (1.0 + erf(*v / std::f32::consts::SQRT_2));
            }
            let o2 = self.linear(&f, n, FF, H, &format!("{p}.output.dense"));
            for (xi, oi) in x.iter_mut().zip(&o2) {
                *xi += oi;
            }
            layer_norm(&mut x, self.g(&format!("{p}.output.LayerNorm.weight")), self.g(&format!("{p}.output.LayerNorm.bias")));
        }
        let mut cls = x[..H].to_vec();
        let norm = cls.iter().map(|v| v * v).sum::<f32>().sqrt();
        for v in cls.iter_mut() {
            *v /= norm;
        }
        cls
    }
}

fn peak_rss_mb() -> f64 {
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) };
    let raw = ru.ru_maxrss as f64;
    if cfg!(target_os = "macos") { raw / 1048576.0 } else { raw / 1024.0 }
}

fn main() -> Result<()> {
    let a: Vec<String> = std::env::args().collect();
    let t = Instant::now();
    let mut w = load(&a[1])?;
    let mut q = HashMap::new();
    if std::env::var("INT8").is_ok() {
        let names: Vec<String> = w.keys().filter(|k| k.starts_with("encoder.") && k.ends_with(".weight") && !k.contains("LayerNorm")).cloned().collect();
        for k in names {
            let v = w.remove(&k).unwrap();
            let inp = if k.contains("output.dense") && !k.contains("attention") { 1536 } else { 384 };
            let mut qs = Vec::with_capacity(v.len());
            let mut sc = Vec::new();
            for row in v.chunks_exact(inp) {
                let m = row.iter().fold(0f32, |m, x| m.max(x.abs())).max(1e-12) / 127.0;
                sc.push(m);
                qs.extend(row.iter().map(|x| (x / m).round().clamp(-127.0, 127.0) as i8));
            }
            q.insert(k.trim_end_matches(".weight").to_string(), (qs, sc));
        }
    }
    let bert = Bert { w, q };
    eprintln!("loaded in {:?}, RSS {:.0} MB", t.elapsed(), peak_rss_mb());
    let mut tok = tokenizers::Tokenizer::from_file(&a[2]).map_err(anyhow::Error::msg)?;
    let max = std::env::var("TRUNC").ok().and_then(|t| t.parse().ok()).unwrap_or(512);
    tok.with_truncation(Some(tokenizers::TruncationParams { max_length: max, ..Default::default() })).map_err(anyhow::Error::msg)?;
    let limit: usize = a.get(5).map_or(200, |s| s.parse().unwrap());
    let texts: Vec<String> = std::io::BufReader::new(std::fs::File::open(&a[3])?)
        .lines()
        .take(limit)
        .map(|l| serde_json::from_str::<serde_json::Value>(&l.unwrap()).unwrap()["text"].as_str().unwrap().to_string())
        .collect();
    // candle's vectors, same order.
    let vf: Box<dyn BufRead> = match std::fs::File::open(&a[4]) {
        Ok(f) => Box::new(std::io::BufReader::new(f)),
        Err(_) => Box::new(std::io::Cursor::new(Vec::new())),
    };
    let mut lines = vf.lines();
    let mut ref_vecs = Vec::new();
    while let (Some(_), Some(hex)) = (lines.next(), lines.next()) {
        let hex = hex?;
        let b: Vec<u8> = (0..hex.len()).step_by(2).map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap()).collect();
        ref_vecs.push(b.chunks(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect::<Vec<f32>>());
        if ref_vecs.len() >= limit {
            break;
        }
    }
    let t = Instant::now();
    let mut worst: f32 = 1.0;
    let mut tokens = 0;
    for (i, text) in texts.iter().enumerate() {
        let enc = tok.encode(text.as_str(), true).map_err(anyhow::Error::msg)?;
        tokens += enc.get_ids().len();
        let v = bert.embed(enc.get_ids());
        if let Some(r) = ref_vecs.get(i) {
            let cos: f32 = v.iter().zip(r).map(|(a, b)| a * b).sum();
            worst = worst.min(cos);
        }
    }
    let took = t.elapsed().as_secs_f64();
    eprintln!(
        "{} posts, {:.0} tokens/post: {:.1} ms/post, worst cosine vs candle {worst:.6}, peak RSS {:.0} MB",
        texts.len(),
        tokens as f64 / texts.len() as f64,
        took * 1000.0 / texts.len() as f64,
        peak_rss_mb()
    );
    Ok(())
}
