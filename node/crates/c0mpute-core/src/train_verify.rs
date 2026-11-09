//! Replicated verification for p2p text fine-tunes (`infernet.text.train`).
//!
//! The idea is borrowed from CrowdGPT's pools: run the same job (same corpus,
//! same seed, same step count) on several independent workers, compare what
//! comes back, drop the outliers and keep a result the majority agrees on. A
//! worker that returns an untrained, half-trained or made-up adapter then
//! costs the buyer a replica, not the job.
//!
//! # What is compared
//!
//! A LoRA adapter is a weight delta `dW = B @ A` per target module. The
//! factors themselves are not comparable (any invertible `M` gives the same
//! delta as `B M^-1` and `M A`), so the distance is taken on `dW`:
//!
//! ```text
//! d(i, j) = ||dW_i - dW_j||_F / max(||dW_i||_F, ||dW_j||_F)    (summed over modules)
//! ```
//!
//! computed exactly without building `dW`: `<B_i A_i, B_j A_j>_F =
//! tr((B_i^T B_j)(A_j A_i^T))`, two rank-by-rank products per module. That
//! keeps a 7B adapter comparison to a few hundred MB of reads and milliseconds
//! of arithmetic.
//!
//! Two cheaper signals ride along when present: the step count in the
//! adapter's `manifest.json` (must match exactly) and the held-out eval
//! perplexity the worker reports at the end of training (must be within
//! [`DEFAULT_EVAL_TOLERANCE`] relative). Both are self-reported, so they can
//! only fail a replica, never vouch for one; the weight distance is computed
//! by the buyer from the bytes it downloaded.
//!
//! # Determinism
//!
//! Replicas are NOT bit-identical. The peft runner seeds Python and torch, so
//! the shuffle, the held-out split and the LoRA init match everywhere, but
//! CPU and CUDA draw different dropout masks from the same seed, BLAS
//! libraries sum in different orders, and thread counts change reduction
//! order. Same-device replicas land within a few percent of each other;
//! CPU-vs-GPU replicas drift further. Hence a tolerance, not equality:
//! [`DEFAULT_TOLERANCE`] is loose enough for mixed hardware and still well
//! below what an untrained adapter scores (distance 1.0 from any trained
//! one) or an unrelated one (about 1.4). Buyers who pin `--gpu` (all
//! replicas on CUDA) can tighten it with `--replica-tolerance`.
//!
//! # Accepting
//!
//! A result is accepted only if a strict majority of the requested replicas
//! agree pairwise (every pair within tolerance). Within that group the medoid
//! (smallest summed distance to the others) is kept. Adapters are NOT
//! averaged: averaging the LoRA factors is not the average of the deltas, and
//! averaging the deltas would give a full-rank matrix that is no longer a
//! LoRA adapter. Replicas outside the agreeing group are outliers and their
//! workers are struck in the buyer's [`crate::penalty_box::PenaltyBox`]. No
//! majority (a two-way split, or too many replicas missing) fails the job
//! and strikes nobody, since there is no way to tell who was right.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::Serialize;

/// Relative weight-delta distance two replicas may differ by and still agree.
pub const DEFAULT_TOLERANCE: f64 = 0.30;
/// Relative difference in final eval perplexity two replicas may show.
pub const DEFAULT_EVAL_TOLERANCE: f64 = 0.10;
/// Replica counts a buyer may ask for. 1 is the ordinary single-worker job.
pub const MAX_REPLICAS: u32 = 5;

/// One tensor out of a safetensors file, widened to f32.
#[derive(Clone, Debug, PartialEq)]
pub struct Tensor {
    pub shape: Vec<usize>,
    pub data: Vec<f32>,
}

/// Parse a safetensors file (F32, F16 and BF16 tensors; others are skipped).
pub fn parse_safetensors(bytes: &[u8]) -> Result<BTreeMap<String, Tensor>> {
    if bytes.len() < 8 {
        bail!("safetensors file is truncated");
    }
    let n = u64::from_le_bytes(bytes[..8].try_into().unwrap()) as usize;
    if n > 100 << 20 || 8 + n > bytes.len() {
        bail!("safetensors header length {n} is out of range");
    }
    let header: serde_json::Value = serde_json::from_slice(&bytes[8..8 + n]).context("safetensors header is not JSON")?;
    let body = &bytes[8 + n..];
    let mut out = BTreeMap::new();
    for (name, meta) in header.as_object().context("safetensors header is not an object")? {
        if name == "__metadata__" {
            continue;
        }
        let dtype = meta["dtype"].as_str().unwrap_or("");
        let shape: Vec<usize> = meta["shape"]
            .as_array()
            .context("tensor without a shape")?
            .iter()
            .map(|d| d.as_u64().map(|d| d as usize).context("bad dimension"))
            .collect::<Result<_>>()?;
        let offs = meta["data_offsets"].as_array().context("tensor without data_offsets")?;
        let (start, end) = match (offs.first().and_then(|v| v.as_u64()), offs.get(1).and_then(|v| v.as_u64())) {
            (Some(s), Some(e)) if s <= e && (e as usize) <= body.len() => (s as usize, e as usize),
            _ => bail!("tensor {name} has bad data_offsets"),
        };
        let raw = &body[start..end];
        let count: usize = shape.iter().product();
        let data: Vec<f32> = match dtype {
            "F32" => raw.chunks_exact(4).map(|c| f32::from_le_bytes(c.try_into().unwrap())).collect(),
            "F16" => raw.chunks_exact(2).map(|c| f16_to_f32(u16::from_le_bytes([c[0], c[1]]))).collect(),
            "BF16" => raw.chunks_exact(2).map(|c| f32::from_bits((u16::from_le_bytes([c[0], c[1]]) as u32) << 16)).collect(),
            _ => continue,
        };
        if data.len() != count {
            bail!("tensor {name}: {} values for shape {shape:?}", data.len());
        }
        out.insert(name.clone(), Tensor { shape, data });
    }
    Ok(out)
}

fn f16_to_f32(h: u16) -> f32 {
    let sign = ((h >> 15) as u32) << 31;
    let exp = ((h >> 10) & 0x1f) as u32;
    let frac = (h & 0x3ff) as u32;
    let bits = match (exp, frac) {
        (0, 0) => sign,
        (0, f) => {
            // Subnormal: renormalize.
            let mut e = 127 - 15 + 1;
            let mut f = f;
            while f & 0x400 == 0 {
                f <<= 1;
                e -= 1;
            }
            sign | (e << 23) | ((f & 0x3ff) << 13)
        }
        (0x1f, 0) => sign | 0x7f80_0000,
        (0x1f, f) => sign | 0x7f80_0000 | (f << 13),
        (e, f) => sign | ((e + 127 - 15) << 23) | (f << 13),
    };
    f32::from_bits(bits)
}

/// The LoRA factor pairs of an adapter: module name -> (A [r, in], B [out, r]).
pub fn lora_pairs(tensors: &BTreeMap<String, Tensor>) -> Result<BTreeMap<String, (Tensor, Tensor)>> {
    let mut pairs = BTreeMap::new();
    for (name, a) in tensors {
        let Some(module) = name.strip_suffix(".lora_A.weight").or_else(|| name.strip_suffix(".lora_A.default.weight")) else {
            continue;
        };
        let b_name = name.replace(".lora_A.", ".lora_B.");
        let b = tensors.get(&b_name).with_context(|| format!("{module}: lora_A without lora_B"))?;
        if a.shape.len() != 2 || b.shape.len() != 2 || a.shape[0] != b.shape[1] {
            bail!("{module}: lora shapes {:?} and {:?} do not multiply", b.shape, a.shape);
        }
        pairs.insert(module.to_string(), (a.clone(), b.clone()));
    }
    if pairs.is_empty() {
        bail!("adapter has no lora_A / lora_B pairs");
    }
    Ok(pairs)
}

/// `<B_i A_i, B_j A_j>_F` without building either product.
fn delta_inner((a_i, b_i): &(Tensor, Tensor), (a_j, b_j): &(Tensor, Tensor)) -> f64 {
    let (ri, rj) = (a_i.shape[0], a_j.shape[0]);
    let (out, inp) = (b_i.shape[0], a_i.shape[1]);
    // G_B = B_i^T B_j  (ri x rj); B is [out, r] row-major.
    let mut gb = vec![0f64; ri * rj];
    for o in 0..out {
        let bi = &b_i.data[o * ri..(o + 1) * ri];
        let bj = &b_j.data[o * rj..(o + 1) * rj];
        for (x, &u) in bi.iter().enumerate() {
            let u = u as f64;
            for (y, &v) in bj.iter().enumerate() {
                gb[x * rj + y] += u * v as f64;
            }
        }
    }
    // G_A = A_j A_i^T  (rj x ri); A is [r, in] row-major.
    let mut sum = 0f64;
    for y in 0..rj {
        let aj = &a_j.data[y * inp..(y + 1) * inp];
        for x in 0..ri {
            let ai = &a_i.data[x * inp..(x + 1) * inp];
            let ga: f64 = aj.iter().zip(ai).map(|(&p, &q)| p as f64 * q as f64).sum();
            // tr(G_B G_A) = sum_{x,y} G_B[x][y] * G_A[y][x]
            sum += gb[x * rj + y] * ga;
        }
    }
    sum
}

/// One replica's adapter, reduced to what the comparison needs.
#[derive(Clone, Debug)]
pub struct Replica {
    pub worker: String,
    pub pairs: BTreeMap<String, (Tensor, Tensor)>,
    /// `steps` from the adapter's manifest.json.
    pub steps: Option<u64>,
    /// The last eval perplexity the worker reported.
    pub eval_perplexity: Option<f64>,
}

impl Replica {
    pub fn from_bytes(worker: &str, weights: &[u8], manifest: Option<&serde_json::Value>, eval_perplexity: Option<f64>) -> Result<Self> {
        Ok(Self {
            worker: worker.to_string(),
            pairs: lora_pairs(&parse_safetensors(weights)?)?,
            steps: manifest.and_then(|m| m["steps"].as_u64()),
            eval_perplexity: eval_perplexity.filter(|p| p.is_finite() && *p > 0.0),
        })
    }

    /// Read an adapter directory a buyer downloaded.
    pub fn from_dir(worker: &str, dir: &Path, eval_perplexity: Option<f64>) -> Result<Self> {
        let weights = std::fs::read(dir.join("adapter_model.safetensors")).context("read adapter_model.safetensors")?;
        let manifest = std::fs::read(dir.join("manifest.json")).ok().and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok());
        Self::from_bytes(worker, &weights, manifest.as_ref(), eval_perplexity)
    }

    fn sq_norm(&self) -> f64 {
        self.pairs.values().map(|p| delta_inner(p, p)).sum()
    }
}

/// Relative delta distance between two replicas; `None` when their modules or
/// shapes differ (not the same job) or neither trained anything.
pub fn distance(a: &Replica, b: &Replica) -> Option<f64> {
    if a.pairs.len() != b.pairs.len() {
        return None;
    }
    let (mut aa, mut bb, mut ab) = (0f64, 0f64, 0f64);
    for (m, pa) in &a.pairs {
        let pb = b.pairs.get(m)?;
        if pa.0.shape[1] != pb.0.shape[1] || pa.1.shape[0] != pb.1.shape[0] {
            return None;
        }
        aa += delta_inner(pa, pa);
        bb += delta_inner(pb, pb);
        ab += delta_inner(pa, pb);
    }
    let scale = aa.max(bb);
    if !(scale > 0.0) {
        return None;
    }
    Some(((aa + bb - 2.0 * ab).max(0.0) / scale).sqrt())
}

/// Do two replicas agree?
pub fn agree(a: &Replica, b: &Replica, tolerance: f64, eval_tolerance: f64) -> bool {
    if let (Some(x), Some(y)) = (a.steps, b.steps)
        && x != y
    {
        return false;
    }
    if let (Some(x), Some(y)) = (a.eval_perplexity, b.eval_perplexity)
        && (x - y).abs() / x.min(y) > eval_tolerance
    {
        return false;
    }
    distance(a, b).is_some_and(|d| d <= tolerance)
}

/// The verdict on a set of replicas.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Decision {
    /// Index (into the replicas given) of the result to keep.
    pub accepted: usize,
    /// Every replica in the agreeing majority, `accepted` included.
    pub agreeing: Vec<usize>,
    /// Replicas outside the majority: their workers get a strike.
    pub outliers: Vec<usize>,
    /// Pairwise distances (`null` where not comparable), for the report.
    pub distances: Vec<Vec<Option<f64>>>,
}

/// Pick the result a strict majority of `requested` replicas agrees on.
///
/// `replicas` are the ones that came back; missing ones count against the
/// majority. Errors name the split so the buyer can see why.
pub fn decide(replicas: &[Replica], requested: usize, tolerance: f64, eval_tolerance: f64) -> Result<Decision> {
    let n = replicas.len();
    let need = requested / 2 + 1;
    let distances: Vec<Vec<Option<f64>>> = (0..n).map(|i| (0..n).map(|j| if i == j { Some(0.0) } else { distance(&replicas[i], &replicas[j]) }).collect()).collect();
    if n == 0 {
        bail!("no replica returned an adapter (needed {need} of {requested} to agree)");
    }
    // A replica whose adapter moved nothing did not train.
    let trained: Vec<bool> = replicas.iter().map(|r| r.sq_norm() > 0.0).collect();
    // Largest group in which every pair agrees (n <= MAX_REPLICAS: brute force).
    let mut best: Vec<usize> = Vec::new();
    for mask in 1u32..(1 << n) {
        let group: Vec<usize> = (0..n).filter(|i| mask & (1 << i) != 0).collect();
        if group.len() <= best.len() || group.iter().any(|&i| !trained[i]) {
            continue;
        }
        let ok = group.iter().enumerate().all(|(k, &i)| group[k + 1..].iter().all(|&j| agree(&replicas[i], &replicas[j], tolerance, eval_tolerance)));
        if ok {
            best = group;
        }
    }
    if best.len() < need {
        let pairs: Vec<String> = (0..n)
            .flat_map(|i| (i + 1..n).map(move |j| (i, j)))
            .map(|(i, j)| match distances[i][j] {
                Some(d) => format!("{}~{}: {d:.3}", short(&replicas[i].worker), short(&replicas[j].worker)),
                None => format!("{}~{}: not comparable", short(&replicas[i].worker), short(&replicas[j].worker)),
            })
            .collect();
        bail!(
            "replicas disagree: {} of {requested} returned, the largest agreeing group has {} and {need} must agree (tolerance {tolerance}); distances {}",
            n,
            best.len(),
            if pairs.is_empty() { "n/a".to_string() } else { pairs.join(", ") }
        );
    }
    let accepted = *best
        .iter()
        .min_by(|&&i, &&j| {
            let si: f64 = best.iter().map(|&k| distances[i][k].unwrap_or(f64::MAX)).sum();
            let sj: f64 = best.iter().map(|&k| distances[j][k].unwrap_or(f64::MAX)).sum();
            si.total_cmp(&sj).then(i.cmp(&j))
        })
        .expect("non-empty group");
    let outliers = (0..n).filter(|i| !best.contains(i)).collect();
    Ok(Decision { accepted, agreeing: best, outliers, distances })
}

/// Strike every outlier's worker; clear the record of the ones that agreed.
pub fn apply_penalties(penalties: &crate::penalty_box::PenaltyBox, replicas: &[Replica], decision: &Decision) {
    for &i in &decision.outliers {
        let bench = penalties.strike(&replicas[i].worker);
        tracing::warn!(worker = %replicas[i].worker, ?bench, "train: replica disagreed with the majority; worker benched");
    }
    for &i in &decision.agreeing {
        penalties.clear(&replicas[i].worker);
    }
}

fn short(peer: &str) -> &str {
    &peer[peer.len().saturating_sub(8)..]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tiny deterministic PRNG so the tests need no rand dependency.
    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self) -> f32 {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            ((self.0 >> 33) as f32 / (1u64 << 31) as f32) - 0.5
        }
    }

    const MODULES: [&str; 3] = ["l0.q_proj", "l0.v_proj", "l1.q_proj"];
    const R: usize = 4;
    const IN: usize = 16;
    const OUT: usize = 16;

    /// An adapter: `base` seeds the "true" training result, `noise` adds
    /// per-replica drift of that relative size, `noise_seed` picks the drift.
    fn adapter(base: u64, noise: f32, noise_seed: u64) -> BTreeMap<String, Tensor> {
        let mut g = Lcg(base);
        let mut n = Lcg(noise_seed);
        let mut t = BTreeMap::new();
        for m in MODULES {
            let mut a: Vec<f32> = (0..R * IN).map(|_| g.next()).collect();
            let mut b: Vec<f32> = (0..OUT * R).map(|_| g.next()).collect();
            for v in a.iter_mut().chain(b.iter_mut()) {
                *v += noise * n.next();
            }
            t.insert(format!("base_model.model.{m}.lora_A.weight"), Tensor { shape: vec![R, IN], data: a });
            t.insert(format!("base_model.model.{m}.lora_B.weight"), Tensor { shape: vec![OUT, R], data: b });
        }
        t
    }

    fn to_safetensors(t: &BTreeMap<String, Tensor>, dtype: &str) -> Vec<u8> {
        let mut header = serde_json::Map::new();
        let mut body = Vec::new();
        for (name, tensor) in t {
            let start = body.len();
            for &v in &tensor.data {
                match dtype {
                    "F32" => body.extend_from_slice(&v.to_le_bytes()),
                    "BF16" => body.extend_from_slice(&((v.to_bits() >> 16) as u16).to_le_bytes()),
                    _ => unreachable!(),
                }
            }
            header.insert(name.clone(), serde_json::json!({"dtype": dtype, "shape": tensor.shape, "data_offsets": [start, body.len()]}));
        }
        header.insert("__metadata__".into(), serde_json::json!({"format": "pt"}));
        let h = serde_json::to_vec(&header).unwrap();
        let mut out = (h.len() as u64).to_le_bytes().to_vec();
        out.extend(h);
        out.extend(body);
        out
    }

    fn replica(worker: &str, t: &BTreeMap<String, Tensor>) -> Replica {
        Replica::from_bytes(worker, &to_safetensors(t, "F32"), Some(&serde_json::json!({"steps": 100})), Some(12.0)).unwrap()
    }

    #[test]
    fn safetensors_round_trip_f32_bf16_f16() {
        let t = adapter(1, 0.0, 0);
        assert_eq!(parse_safetensors(&to_safetensors(&t, "F32")).unwrap(), t);
        let bf = parse_safetensors(&to_safetensors(&t, "BF16")).unwrap();
        let k = "base_model.model.l0.q_proj.lora_A.weight";
        for (x, y) in bf[k].data.iter().zip(&t[k].data) {
            assert!((x - y).abs() <= 0.01, "{x} vs {y}");
        }
        for (h, f) in [(0x3c00u16, 1.0f32), (0xc000, -2.0), (0x3555, 0.333_251_95), (0x0001, 5.960_464_5e-8), (0, 0.0)] {
            assert_eq!(f16_to_f32(h), f, "{h:#x}");
        }
        assert!(parse_safetensors(b"short").is_err());
        assert!(parse_safetensors(&[0xff; 16]).is_err());
    }

    #[test]
    fn the_inner_product_trick_matches_the_dense_delta() {
        let (a, b) = (adapter(3, 0.0, 0), adapter(4, 0.0, 0));
        let (ra, rb) = (replica("a", &a), replica("b", &b));
        // Dense ||dW_a - dW_b|| / max(||dW_a||, ||dW_b||) for comparison.
        let dense = |t: &BTreeMap<String, Tensor>, m: &str| {
            let a = &t[&format!("base_model.model.{m}.lora_A.weight")];
            let b = &t[&format!("base_model.model.{m}.lora_B.weight")];
            let mut w = vec![0f64; OUT * IN];
            for o in 0..OUT {
                for i in 0..IN {
                    w[o * IN + i] = (0..R).map(|r| b.data[o * R + r] as f64 * a.data[r * IN + i] as f64).sum();
                }
            }
            w
        };
        let (mut diff, mut na, mut nb) = (0f64, 0f64, 0f64);
        for m in MODULES {
            let (wa, wb) = (dense(&a, m), dense(&b, m));
            diff += wa.iter().zip(&wb).map(|(x, y)| (x - y).powi(2)).sum::<f64>();
            na += wa.iter().map(|x| x * x).sum::<f64>();
            nb += wb.iter().map(|x| x * x).sum::<f64>();
        }
        let want = (diff / na.max(nb)).sqrt();
        let got = distance(&ra, &rb).unwrap();
        assert!((got - want).abs() < 1e-9, "{got} vs {want}");
        // Factorization invariance: scaling A by 2 and B by 1/2 is the same delta.
        let mut scaled = a.clone();
        for (k, v) in scaled.iter_mut() {
            let f = if k.contains("lora_A") { 2.0 } else { 0.5 };
            v.data.iter_mut().for_each(|x| *x *= f);
        }
        assert!(distance(&ra, &replica("s", &scaled)).unwrap() < 1e-6);
    }

    #[test]
    fn agreeing_replicas_accept_the_medoid() {
        // Three honest replicas: same job, small hardware drift.
        let rs = vec![replica("w1", &adapter(7, 0.01, 1)), replica("w2", &adapter(7, 0.01, 2)), replica("w3", &adapter(7, 0.01, 3))];
        let d = decide(&rs, 3, DEFAULT_TOLERANCE, DEFAULT_EVAL_TOLERANCE).unwrap();
        assert_eq!(d.agreeing, vec![0, 1, 2]);
        assert!(d.outliers.is_empty());
        assert!(d.distances[0][1].unwrap() < 0.1);
        // The medoid has the smallest summed distance to the others.
        let sum = |i: usize| (0..3).map(|k| d.distances[i][k].unwrap()).sum::<f64>();
        assert!((0..3).all(|i| sum(d.accepted) <= sum(i)));
    }

    #[test]
    fn one_bad_worker_is_dropped_and_benched() {
        // w2 returns an adapter from some other training run.
        let rs = vec![replica("w1", &adapter(7, 0.01, 1)), replica("w2", &adapter(99, 0.0, 0)), replica("w3", &adapter(7, 0.01, 3))];
        let d = decide(&rs, 3, DEFAULT_TOLERANCE, DEFAULT_EVAL_TOLERANCE).unwrap();
        assert_eq!(d.agreeing, vec![0, 2]);
        assert_eq!(d.outliers, vec![1]);
        assert_ne!(d.accepted, 1);
        let pb = crate::penalty_box::PenaltyBox::new();
        pb.strike("w1"); // an old strike, cleared by agreeing now
        apply_penalties(&pb, &rs, &d);
        assert_eq!(pb.benched(), vec!["w2".to_string()]);

        // An untrained adapter (B = 0, the LoRA init) never counts.
        let mut lazy = adapter(7, 0.0, 0);
        for (k, v) in lazy.iter_mut() {
            if k.contains("lora_B") {
                v.data.iter_mut().for_each(|x| *x = 0.0);
            }
        }
        let rs = vec![replica("w1", &adapter(7, 0.01, 1)), replica("lazy", &lazy), replica("w3", &adapter(7, 0.01, 3))];
        assert_eq!(decide(&rs, 3, DEFAULT_TOLERANCE, DEFAULT_EVAL_TOLERANCE).unwrap().outliers, vec![1]);

        // A replica that reports a different step count disagrees even when
        // its weights happen to be close.
        let mut short_run = replica("w2", &adapter(7, 0.01, 2));
        short_run.steps = Some(50);
        let rs = vec![replica("w1", &adapter(7, 0.01, 1)), short_run, replica("w3", &adapter(7, 0.01, 3))];
        assert_eq!(decide(&rs, 3, DEFAULT_TOLERANCE, DEFAULT_EVAL_TOLERANCE).unwrap().outliers, vec![1]);

        // So does one whose eval perplexity is far off.
        let mut off = replica("w2", &adapter(7, 0.01, 2));
        off.eval_perplexity = Some(30.0);
        let rs = vec![replica("w1", &adapter(7, 0.01, 1)), off, replica("w3", &adapter(7, 0.01, 3))];
        assert_eq!(decide(&rs, 3, DEFAULT_TOLERANCE, DEFAULT_EVAL_TOLERANCE).unwrap().outliers, vec![1]);
    }

    #[test]
    fn a_split_fails_the_job_clearly_and_strikes_nobody() {
        // Three replicas, all different: no majority.
        let rs = vec![replica("w1", &adapter(1, 0.0, 0)), replica("w2", &adapter(2, 0.0, 0)), replica("w3", &adapter(3, 0.0, 0))];
        let e = decide(&rs, 3, DEFAULT_TOLERANCE, DEFAULT_EVAL_TOLERANCE).unwrap_err().to_string();
        assert!(e.contains("replicas disagree") && e.contains("2 must agree"), "{e}");

        // Two-way split: one of three never delivered and the two left disagree.
        let rs = vec![replica("w1", &adapter(1, 0.0, 0)), replica("w2", &adapter(2, 0.0, 0))];
        let e = decide(&rs, 3, DEFAULT_TOLERANCE, DEFAULT_EVAL_TOLERANCE).unwrap_err().to_string();
        assert!(e.contains("2 of 3 returned"), "{e}");

        // Four replicas split two and two: no strict majority either.
        let rs = vec![
            replica("a1", &adapter(1, 0.01, 1)),
            replica("a2", &adapter(1, 0.01, 2)),
            replica("b1", &adapter(2, 0.01, 3)),
            replica("b2", &adapter(2, 0.01, 4)),
        ];
        assert!(decide(&rs, 4, DEFAULT_TOLERANCE, DEFAULT_EVAL_TOLERANCE).is_err());

        // Two that agree out of three requested is enough.
        let rs = vec![replica("w1", &adapter(1, 0.01, 1)), replica("w2", &adapter(1, 0.01, 2))];
        assert_eq!(decide(&rs, 3, DEFAULT_TOLERANCE, DEFAULT_EVAL_TOLERANCE).unwrap().agreeing, vec![0, 1]);

        assert!(decide(&[], 3, DEFAULT_TOLERANCE, DEFAULT_EVAL_TOLERANCE).is_err());
    }

    #[test]
    fn adapters_of_different_shapes_are_not_comparable() {
        let a = replica("a", &adapter(1, 0.0, 0));
        let mut t = adapter(1, 0.0, 0);
        t.retain(|k, _| !k.contains("l1."));
        assert_eq!(distance(&a, &replica("b", &t)), None);
        assert!(lora_pairs(&BTreeMap::new()).is_err());
    }
}
