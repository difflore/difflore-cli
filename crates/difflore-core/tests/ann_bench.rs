#![allow(clippy::unwrap_used)]
#![allow(unsafe_code)]
// Informal benchmark for the ANN path. Not wired into `cargo bench` (no
// criterion dep); run with:
//   cargo test -p difflore-core --release --test ann_bench -- --nocapture --ignored
//
// Ignored by default so it doesn't weigh down the regular test matrix. Prints
// timings for the HNSW path and a hand-rolled linear cosine scan on the same
// corpus to show the speedup at 1K and 10K chunks.
use difflore_core::context::ann::AnnIndex;
use std::time::Instant;

fn random_vec(seed: u64, dim: usize) -> Vec<f32> {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut v = Vec::with_capacity(dim);
    for i in 0..dim {
        let mut h = DefaultHasher::new();
        (seed, i).hash(&mut h);
        let raw = h.finish();
        let x = ((raw as i64) as f64) / (i64::MAX as f64);
        v.push(x as f32);
    }
    let n: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if n > 0.0 {
        for x in &mut v {
            *x /= n;
        }
    }
    v
}

fn cos(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b.iter()).map(|(x, y)| x * y).sum()
}

fn linear_top_k(query: &[f32], corpus: &[(String, Vec<f32>)], k: usize) -> Vec<(String, f32)> {
    let mut scored: Vec<(String, f32)> = corpus
        .iter()
        .map(|(id, v)| (id.clone(), cos(query, v)))
        .collect();
    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    scored.truncate(k);
    scored
}

async fn bench_size(n: usize, dim: usize) {
    let tmp = tempfile::TempDir::new().unwrap();
    // SAFETY: bench runs single-threaded under --test-threads=1.
    unsafe {
        std::env::set_var("DIFFLORE_HOME", tmp.path());
    }
    let hash = format!("bench-{n}");

    println!("\n== N={n}, dim={dim} ==");

    let t0 = Instant::now();
    let chunks: Vec<(String, Vec<f32>)> = (0..n)
        .map(|i| (format!("c{i}"), random_vec(i as u64, dim)))
        .collect();
    println!("corpus gen : {:?}", t0.elapsed());

    let t0 = Instant::now();
    let idx = AnnIndex::build_from_chunks(&hash, &chunks).await.unwrap();
    println!("build HNSW : {:?}", t0.elapsed());

    let query = random_vec(999, dim);

    let t0 = Instant::now();
    for _ in 0..100 {
        let _ = idx.search(&query, 10);
    }
    let ann_us_avg = t0.elapsed().as_micros() / 100;
    println!("ANN search (avg of 100): {ann_us_avg} µs");

    let t0 = Instant::now();
    for _ in 0..100 {
        let _ = linear_top_k(&query, &chunks, 10);
    }
    let lin_us_avg = t0.elapsed().as_micros() / 100;
    println!("Linear scan (avg of 100): {lin_us_avg} µs");

    if ann_us_avg > 0 {
        let ratio = lin_us_avg as f64 / ann_us_avg as f64;
        println!("Speedup: {ratio:.2}x");
    }

    unsafe {
        std::env::remove_var("DIFFLORE_HOME");
    }
}

#[tokio::test]
#[ignore = "manual benchmark; run with --ignored"]
async fn ann_vs_linear_1k() {
    bench_size(1_000, 128).await;
}

#[tokio::test]
#[ignore = "manual benchmark; run with --ignored"]
async fn ann_vs_linear_10k() {
    bench_size(10_000, 128).await;
}
