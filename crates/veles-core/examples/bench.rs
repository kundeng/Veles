//! Quick wall-clock benchmark for indexing + querying.
//!
//! Usage: `cargo run --release --example bench -- <repo_path> [n_queries]`
//!
//! Reports:
//!   * Index time (file walk + chunking + BM25 + dense embedding).
//!   * Per-query latency for hybrid, semantic, and BM25 modes (median over N runs).

use std::path::Path;
use std::time::Instant;

use veles_core::{VelesIndex, model, types::SearchMode};

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: bench <repo_path> [n_queries]");
        std::process::exit(2);
    }
    let repo = &args[1];
    let n_queries: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(50);

    let queries = [
        "load model from disk",
        "BM25 inverted index",
        "tokenize identifier",
        "save_pretrained",
        "rerank top-k results",
        "cosine similarity",
        "git clone repo",
        "parseConfig",
        "boost multi chunk files",
        "walk gitignore directory",
    ];

    eprintln!("Loading model …");
    let t = Instant::now();
    let model = model::load_model(None)?;
    eprintln!("  model load:        {:?}", t.elapsed());

    eprintln!("Indexing {repo} …");
    let t = Instant::now();
    let index = VelesIndex::from_path(Path::new(repo), Some(model), None, false)?;
    let index_time = t.elapsed();
    let stats = index.stats();
    eprintln!(
        "  index time:        {:?}  ({} files, {} chunks)",
        index_time, stats.indexed_files, stats.total_chunks
    );

    for &mode in &[SearchMode::Hybrid, SearchMode::Semantic, SearchMode::Bm25] {
        let mut samples: Vec<u128> = Vec::with_capacity(n_queries);
        // Warm-up.
        for q in queries.iter() {
            let _ = index.search(q, 5, mode, None, None, None);
        }
        for i in 0..n_queries {
            let q = queries[i % queries.len()];
            let t = Instant::now();
            let _r = index.search(q, 5, mode, None, None, None);
            samples.push(t.elapsed().as_micros());
        }
        samples.sort();
        let p50 = samples[samples.len() / 2];
        let p95 = samples[(samples.len() * 95) / 100];
        let total: u128 = samples.iter().sum();
        let mean = total / samples.len() as u128;
        eprintln!(
            "  {:9}  p50 {:>6} µs   p95 {:>6} µs   mean {:>6} µs   ({} runs)",
            mode, p50, p95, mean, n_queries
        );
    }

    Ok(())
}
