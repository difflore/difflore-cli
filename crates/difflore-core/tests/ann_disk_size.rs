#![allow(clippy::unwrap_used)]
#![allow(unsafe_code)]
// Measures on-disk size of the persisted HNSW graph.

use difflore_core::context::ann::{AnnIndex, ann_files_for_project};

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

#[tokio::test]
#[ignore = "manual benchmark; run with --ignored"]
async fn measure_1k_persisted_size() {
    let tmp = tempfile::TempDir::new().unwrap();
    unsafe {
        std::env::set_var("DIFFLORE_HOME", tmp.path());
    }
    let hash = "size1k";
    let chunks: Vec<(String, Vec<f32>)> = (0..1000)
        .map(|i| (format!("c{i}"), random_vec(i as u64, 128)))
        .collect();
    let mut idx = AnnIndex::build_from_chunks(hash, &chunks).await.unwrap();
    idx.save().await.unwrap();

    let (graph, data, meta) = ann_files_for_project(hash);
    let graph_sz = std::fs::metadata(&graph).map_or(0, |m| m.len());
    let data_sz = std::fs::metadata(&data).map_or(0, |m| m.len());
    let meta_sz = std::fs::metadata(&meta).map_or(0, |m| m.len());
    println!(
        "1K chunks @ dim=128 -> graph: {} bytes, data: {} bytes, meta: {} bytes, total: {} bytes",
        graph_sz,
        data_sz,
        meta_sz,
        graph_sz + data_sz + meta_sz
    );
    unsafe {
        std::env::remove_var("DIFFLORE_HOME");
    }
}
