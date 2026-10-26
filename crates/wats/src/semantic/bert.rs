//! bge-small-en-v1.5's forward pass (ADR 0053 §3): a 12-layer BERT, 384
//! wide, 12 heads, written out here on [`wats_sys::sgemm`] (Accelerate on
//! macOS, the pure-Rust `gemm` crate elsewhere). One thread, one text at a
//! time, f32 compute; weights read from a safetensors file in fp16 or f32.
//!
//! The embedding is the `[CLS]` output, L2-normalised. A test checks it
//! against the reference implementation's (candle) on a fixture.

use std::collections::HashMap;

use anyhow::{Context, Result, anyhow, bail, ensure};

/// Dimensions of the embedding.
pub const DIM: usize = 384;
const HEADS: usize = 12;
const HEAD_DIM: usize = DIM / HEADS;
const FF: usize = 1536;
const LAYERS: usize = 12;
const VOCAB: usize = 30_522;
const POSITIONS: usize = 512;
const EPS: f32 = 1e-12;

/// fp16 bits to f32, exactly.
pub fn f16_to_f32(h: u16) -> f32 {
    let sign = u32::from(h >> 15) << 31;
    let exp = u32::from((h >> 10) & 0x1f);
    let frac = u32::from(h & 0x3ff);
    let bits = match (exp, frac) {
        (0, 0) => sign,
        (0, _) => {
            // Subnormal: shift until the leading 1 is the implicit bit.
            let shift = frac.leading_zeros() - 21;
            sign | ((113 - shift) << 23) | (((frac << shift) & 0x3ff) << 13)
        }
        (0x1f, _) => sign | 0x7f80_0000 | (frac << 13),
        _ => sign | ((exp + 112) << 23) | (frac << 13),
    };
    f32::from_bits(bits)
}

/// f32 to fp16 bits, rounding to nearest even (the weights' conversion).
pub fn f32_to_f16(v: f32) -> u16 {
    let bits = v.to_bits();
    let sign = ((bits >> 16) & 0x8000) as u16;
    let exp = ((bits >> 23) & 0xff) as i32;
    let man = bits & 0x7f_ffff;
    if exp == 0xff {
        return sign | 0x7c00 | if man != 0 { 0x200 } else { 0 };
    }
    let e = exp - 127 + 15;
    if e >= 0x1f {
        return sign | 0x7c00;
    }
    if e <= 0 {
        if e < -10 {
            return sign;
        }
        // Subnormal: the implicit bit joins the mantissa.
        let m = man | 0x80_0000;
        let shift = (14 - e) as u32;
        let half = 1u32 << (shift - 1);
        let rest = m & ((1u32 << shift) - 1);
        let mut out = m >> shift;
        if rest > half || (rest == half && out & 1 == 1) {
            out += 1;
        }
        return sign | out as u16;
    }
    let mut out = ((e as u32) << 10) | (man >> 13);
    let rest = man & 0x1fff;
    if rest > 0x1000 || (rest == 0x1000 && out & 1 == 1) {
        // A carry into the exponent is right, up to infinity.
        out += 1;
    }
    sign | out as u16
}

/// Tensors by name: shape and values.
pub type Tensors = HashMap<String, (Vec<usize>, Vec<f32>)>;

/// The tensors of a safetensors file, as f32, with their shapes.
pub fn read_safetensors(bytes: &[u8]) -> Result<Tensors> {
    let len = bytes
        .get(..8)
        .and_then(|b| b.try_into().ok())
        .map(u64::from_le_bytes)
        .context("safetensors: too short")?;
    let len = usize::try_from(len)?;
    let header_end = len.checked_add(8).context("safetensors: bad header")?;
    let header: serde_json::Value = serde_json::from_slice(
        bytes
            .get(8..header_end)
            .context("safetensors: truncated header")?,
    )?;
    let data = bytes.get(header_end..).unwrap_or_default();
    let mut out = HashMap::new();
    for (name, t) in header.as_object().context("safetensors: header")? {
        if name == "__metadata__" {
            continue;
        }
        let shape: Vec<usize> = t["shape"]
            .as_array()
            .context("safetensors: shape")?
            .iter()
            .map(|d| d.as_u64().and_then(|d| usize::try_from(d).ok()))
            .collect::<Option<_>>()
            .context("safetensors: shape")?;
        let offsets = |i: usize| {
            t["data_offsets"][i]
                .as_u64()
                .and_then(|o| usize::try_from(o).ok())
                .context("safetensors: offsets")
        };
        let raw = data
            .get(offsets(0)?..offsets(1)?)
            .context("safetensors: data out of range")?;
        let values: Vec<f32> = match t["dtype"].as_str() {
            Some("F32") => raw
                .as_chunks::<4>()
                .0
                .iter()
                .map(|c| f32::from_le_bytes(*c))
                .collect(),
            Some("F16") => raw
                .as_chunks::<2>()
                .0
                .iter()
                .map(|c| f16_to_f32(u16::from_le_bytes(*c)))
                .collect(),
            // Ids (position_ids) and anything else: not weights we use.
            _ => continue,
        };
        ensure!(
            values.len() == shape.iter().product::<usize>(),
            "safetensors: {name} has {} values for shape {shape:?}",
            values.len()
        );
        out.insert(name.clone(), (shape, values));
    }
    Ok(out)
}

struct Linear {
    /// [out, in], row-major (y = x Wᵀ + b).
    weight: Vec<f32>,
    bias: Vec<f32>,
    inp: usize,
    out: usize,
}

impl Linear {
    fn forward(&self, x: &[f32], n: usize) -> Vec<f32> {
        let mut y = vec![0.0; n * self.out];
        wats_sys::sgemm(
            n,
            self.out,
            self.inp,
            x,
            self.inp,
            &self.weight,
            self.inp,
            true,
            &mut y,
            self.out,
        );
        for row in y.chunks_exact_mut(self.out) {
            for (v, b) in row.iter_mut().zip(&self.bias) {
                *v += b;
            }
        }
        y
    }
}

struct Norm {
    weight: Vec<f32>,
    bias: Vec<f32>,
}

impl Norm {
    fn forward(&self, x: &mut [f32]) {
        for row in x.as_chunks_mut::<DIM>().0 {
            let mean = row.iter().sum::<f32>() / DIM as f32;
            let var = row.iter().map(|v| (v - mean) * (v - mean)).sum::<f32>() / DIM as f32;
            let inv = 1.0 / (var + EPS).sqrt();
            for ((v, w), b) in row.iter_mut().zip(&self.weight).zip(&self.bias) {
                *v = (*v - mean) * inv * w + b;
            }
        }
    }
}

struct Layer {
    query: Linear,
    key: Linear,
    value: Linear,
    attention_out: Linear,
    attention_norm: Norm,
    intermediate: Linear,
    output: Linear,
    output_norm: Norm,
}

/// The model, in memory: about 133 MB of f32.
pub struct Bert {
    words: Vec<f32>,
    positions: Vec<f32>,
    token_type: Vec<f32>,
    embeddings_norm: Norm,
    layers: Vec<Layer>,
}

/// erf, to about 1e-7 (Numerical Recipes' erfc, Chebyshev fit), for the
/// exact GELU BERT uses.
fn erf(x: f32) -> f32 {
    let x = f64::from(x);
    let z = x.abs();
    let t = 1.0 / (1.0 + 0.5 * z);
    let poly = -z * z - 1.265_512_23
        + t * (1.000_023_68
            + t * (0.374_091_96
                + t * (0.096_784_18
                    + t * (-0.186_288_06
                        + t * (0.278_868_07
                            + t * (-1.135_203_98
                                + t * (1.488_515_87 + t * (-0.822_152_23 + t * 0.170_872_77))))))));
    let r = t * poly.exp();
    (if x >= 0.0 { 1.0 - r } else { r - 1.0 }) as f32
}

impl Bert {
    /// Loads the weights of a safetensors file (fp16 or f32).
    pub fn load(bytes: &[u8]) -> Result<Self> {
        let mut t = read_safetensors(bytes)?;
        let mut take = |name: &str, shape: &[usize]| -> Result<Vec<f32>> {
            let (got, values) = t
                .remove(name)
                .ok_or_else(|| anyhow!("model: no tensor {name}"))?;
            if got != shape {
                bail!("model: {name} is {got:?}, not {shape:?}");
            }
            Ok(values)
        };
        let mut linear = |name: &str, inp: usize, out: usize| -> Result<Linear> {
            Ok(Linear {
                weight: take(&format!("{name}.weight"), &[out, inp])?,
                bias: take(&format!("{name}.bias"), &[out])?,
                inp,
                out,
            })
        };
        let mut layers = Vec::with_capacity(LAYERS);
        for l in 0..LAYERS {
            let p = format!("encoder.layer.{l}");
            layers.push(Layer {
                query: linear(&format!("{p}.attention.self.query"), DIM, DIM)?,
                key: linear(&format!("{p}.attention.self.key"), DIM, DIM)?,
                value: linear(&format!("{p}.attention.self.value"), DIM, DIM)?,
                attention_out: linear(&format!("{p}.attention.output.dense"), DIM, DIM)?,
                attention_norm: Norm {
                    weight: Vec::new(),
                    bias: Vec::new(),
                },
                intermediate: linear(&format!("{p}.intermediate.dense"), DIM, FF)?,
                output: linear(&format!("{p}.output.dense"), FF, DIM)?,
                output_norm: Norm {
                    weight: Vec::new(),
                    bias: Vec::new(),
                },
            });
        }
        drop(linear);
        let mut norm = |name: &str| -> Result<Norm> {
            Ok(Norm {
                weight: take(&format!("{name}.weight"), &[DIM])?,
                bias: take(&format!("{name}.bias"), &[DIM])?,
            })
        };
        for (l, layer) in layers.iter_mut().enumerate() {
            let p = format!("encoder.layer.{l}");
            layer.attention_norm = norm(&format!("{p}.attention.output.LayerNorm"))?;
            layer.output_norm = norm(&format!("{p}.output.LayerNorm"))?;
        }
        let embeddings_norm = norm("embeddings.LayerNorm")?;
        drop(norm);
        Ok(Bert {
            words: take("embeddings.word_embeddings.weight", &[VOCAB, DIM])?,
            positions: take("embeddings.position_embeddings.weight", &[POSITIONS, DIM])?,
            token_type: take("embeddings.token_type_embeddings.weight", &[2, DIM])?
                .into_iter()
                .take(DIM)
                .collect(),
            embeddings_norm,
            layers,
        })
    }

    /// The embedding of `ids` (at most 512, `[CLS]` first): L2-normalised.
    pub fn embed(&self, ids: &[u32]) -> Vec<f32> {
        let n = ids.len().clamp(1, POSITIONS);
        let mut x = vec![0.0f32; n * DIM];
        for (i, row) in x.as_chunks_mut::<DIM>().0.iter_mut().enumerate() {
            let id = ids.get(i).map_or(0, |&id| (id as usize).min(VOCAB - 1));
            let word = &self.words[id * DIM..(id + 1) * DIM];
            let position = &self.positions[i * DIM..(i + 1) * DIM];
            for (d, v) in row.iter_mut().enumerate() {
                *v = word[d] + position[d] + self.token_type[d];
            }
        }
        self.embeddings_norm.forward(&mut x);
        let scale = 1.0 / (HEAD_DIM as f32).sqrt();
        let mut scores = vec![0.0f32; n * n];
        let mut context = vec![0.0f32; n * DIM];
        for layer in &self.layers {
            let q = layer.query.forward(&x, n);
            let k = layer.key.forward(&x, n);
            let v = layer.value.forward(&x, n);
            for h in 0..HEADS {
                let o = h * HEAD_DIM;
                // scores = q_h k_hᵀ, then a softmax per row.
                wats_sys::sgemm(
                    n,
                    n,
                    HEAD_DIM,
                    &q[o..],
                    DIM,
                    &k[o..],
                    DIM,
                    true,
                    &mut scores,
                    n,
                );
                for row in scores.chunks_exact_mut(n) {
                    let max = row.iter().copied().fold(f32::MIN, f32::max);
                    let mut sum = 0.0;
                    for s in row.iter_mut() {
                        *s = ((*s - max) * scale).exp();
                        sum += *s;
                    }
                    for s in row.iter_mut() {
                        *s /= sum;
                    }
                }
                wats_sys::sgemm(
                    n,
                    HEAD_DIM,
                    n,
                    &scores,
                    n,
                    &v[o..],
                    DIM,
                    false,
                    &mut context[o..],
                    DIM,
                );
            }
            let attention = layer.attention_out.forward(&context, n);
            x.iter_mut().zip(&attention).for_each(|(x, a)| *x += a);
            layer.attention_norm.forward(&mut x);
            let mut hidden = layer.intermediate.forward(&x, n);
            for h in hidden.iter_mut() {
                *h = 0.5 * *h * (1.0 + erf(*h / std::f32::consts::SQRT_2));
            }
            let out = layer.output.forward(&hidden, n);
            x.iter_mut().zip(&out).for_each(|(x, o)| *x += o);
            layer.output_norm.forward(&mut x);
        }
        let mut cls = x[..DIM].to_vec();
        let norm = cls.iter().map(|v| v * v).sum::<f32>().sqrt().max(1e-12);
        cls.iter_mut().for_each(|v| *v /= norm);
        cls
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn f16_round_trips() {
        for v in [
            0.0f32, -0.0, 1.0, -2.5, 0.333, 65504.0, 6.1e-5, 5.96e-8, 1e-3,
        ] {
            let h = f32_to_f16(v);
            let back = f16_to_f32(h);
            assert!(
                (back - v).abs() <= v.abs() * 1e-3 + 6e-8,
                "{v} -> {h:#x} -> {back}"
            );
        }
        assert_eq!(f32_to_f16(1.0), 0x3c00);
        assert_eq!(f32_to_f16(-2.0), 0xc000);
        assert_eq!(f32_to_f16(65504.0), 0x7bff);
        assert_eq!(f32_to_f16(1e6), 0x7c00);
        assert_eq!(f16_to_f32(0x0001), 5.960_464_5e-8);
        assert_eq!(f16_to_f32(0x03ff), 6.097_555e-5);
        // Halfway between 1.0 and the next fp16: to even (1.0).
        assert_eq!(f32_to_f16(1.0 + 1.0 / 2048.0), 0x3c00);
        // Every fp16 survives f16 -> f32 -> f16.
        for h in 0..=u16::MAX {
            let f = f16_to_f32(h);
            if !f.is_nan() {
                assert_eq!(f32_to_f16(f), h, "{h:#x}");
            }
        }
    }

    #[test]
    fn erf_is_close() {
        for (x, want) in [
            (0.0, 0.0),
            (0.5, 0.520_499_9),
            (1.0, 0.842_700_8),
            (-2.0, -0.995_322_3),
        ] {
            assert!((erf(x) - want).abs() < 2e-7, "{x}");
        }
    }

    /// With the model file (`WATS_SEMANTIC_MODEL`), the embeddings of the
    /// fixture's first texts match the reference implementation's.
    #[test]
    #[ignore = "needs the model: WATS_SEMANTIC_MODEL=<model.safetensors>"]
    fn matches_the_reference_embeddings() {
        let path = std::env::var("WATS_SEMANTIC_MODEL").unwrap();
        let bert = Bert::load(&std::fs::read(path).unwrap()).unwrap();
        let fixture = include_str!("../../tests/data/semantic-fixture.jsonl");
        let mut checked = 0;
        for line in fixture.lines() {
            let v: serde_json::Value = serde_json::from_str(line).unwrap();
            let Some(want) = v["vec"].as_array() else {
                continue;
            };
            let ids = super::super::tokenizer::encode(v["text"].as_str().unwrap(), 256);
            let got = bert.embed(&ids);
            let cos: f32 = got
                .iter()
                .zip(want)
                .map(|(a, b)| a * b.as_f64().unwrap() as f32)
                .sum();
            assert!(cos > 0.9999, "cosine {cos}");
            checked += 1;
        }
        assert_eq!(checked, 6);
    }
}
