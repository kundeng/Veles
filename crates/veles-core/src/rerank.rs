//! Optional transformer reranker (decisions D5/D8) — a precision second stage
//! over the static/BM25 recall set.
//!
//! The base index uses a *static* model2vec embedding: ~10 ms/query on CPU, but
//! semantically blunt (validation top-1 cosine ~0.018 on the hard prose set).
//! That is great for **recall** and useless for **precision**. So we keep the
//! cheap stage for recall (top-K), then re-score just those K candidates with a
//! real transformer (`BAAI/bge-small-en-v1.5`, a 384-dim BERT bi-encoder).
//!
//! Why candle and not onnxruntime/ort: this crate ships as a **single static
//! binary**. onnxruntime needs glibc 2.38 (the deploy box is 2.35 — that is the
//! exact wall that killed `ck`). `candle` is pure-Rust ML, so the binary stays
//! portable. The model runs on CPU by default and auto-moves to CUDA when a GPU
//! is present (`Device::cuda_if_available`).
//!
//! This whole module is gated behind the `rerank` feature so the default build
//! pulls in none of candle/tokenizers/hf-hub.

use anyhow::{Context, Result};
use candle_core::{Device, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::bert::{BertModel, Config, DTYPE};
use tokenizers::Tokenizer;

/// Default reranker: a small, general-language (not code-specialised) BERT
/// bi-encoder. General-language is deliberate (D5) — the prose/failure corpus
/// is natural language, not source.
pub const DEFAULT_RERANK_MODEL: &str = "BAAI/bge-small-en-v1.5";

/// Default recall depth for the cheap first stage when reranking. K=50 was the
/// sweet spot in the CPU benchmark: ~599 ms rerank latency with the full
/// precision gain (the curve flattens past 50). Both CLI and MCP default here.
pub const DEFAULT_RERANK_RECALL: usize = 50;

/// A loaded transformer reranker: BERT weights + tokenizer + the device they
/// live on. Construct once (model load + first download is the slow part), then
/// reuse across queries.
pub struct Reranker {
    model: BertModel,
    tokenizer: Tokenizer,
    device: Device,
}

impl Reranker {
    /// Load `model_id` (default [`DEFAULT_RERANK_MODEL`]) from the HuggingFace
    /// cache, downloading on first use. Picks CUDA when available, else CPU.
    pub fn load(model_id: Option<&str>) -> Result<Self> {
        let model_id = model_id.unwrap_or(DEFAULT_RERANK_MODEL).to_string();
        // GPU when present (RTX cards, etc.), else CPU — single code path, the
        // device just changes where tensors live.
        let device = Device::cuda_if_available(0).unwrap_or(Device::Cpu);

        let api = hf_hub::api::sync::Api::new().context("init huggingface api")?;
        let repo = api.model(model_id.clone());
        let config_path = repo
            .get("config.json")
            .with_context(|| format!("fetch config.json for {model_id}"))?;
        let tokenizer_path = repo
            .get("tokenizer.json")
            .with_context(|| format!("fetch tokenizer.json for {model_id}"))?;
        let weights_path = repo
            .get("model.safetensors")
            .with_context(|| format!("fetch model.safetensors for {model_id}"))?;

        let config: Config = serde_json::from_slice(&std::fs::read(&config_path)?)
            .context("parse bert config.json")?;
        let mut tokenizer = Tokenizer::from_file(&tokenizer_path)
            .map_err(anyhow::Error::msg)
            .context("load tokenizer.json")?;
        // BERT position embeddings cap at 512; long session chunks must be
        // truncated or the forward pass indexes past the table (panic/error).
        tokenizer
            .with_truncation(Some(tokenizers::TruncationParams {
                max_length: 512,
                ..Default::default()
            }))
            .map_err(|e| anyhow::anyhow!(e.to_string()))
            .context("configure tokenizer truncation to 512")?;
        // SAFETY: mmap of a read-only model file we just downloaded; standard
        // candle pattern. The file is not mutated for the process lifetime.
        let vb = unsafe {
            VarBuilder::from_mmaped_safetensors(&[weights_path], DTYPE, &device)
                .context("mmap safetensors weights")?
        };
        let model = BertModel::load(vb, &config).context("build bert model")?;

        Ok(Self {
            model,
            tokenizer,
            device,
        })
    }

    /// True when the model is running on a GPU.
    pub fn on_gpu(&self) -> bool {
        !matches!(self.device, Device::Cpu)
    }

    /// Embed a batch of texts into L2-normalised, mean-pooled sentence vectors,
    /// shape `(n, hidden)`. Mean-pooling over *non-padding* tokens (masked) is
    /// the standard sentence-embedding recipe for BGE-style encoders.
    fn embed(&self, texts: &[String]) -> Result<Tensor> {
        let encodings = self
            .tokenizer
            .encode_batch(texts.to_vec(), true)
            .map_err(anyhow::Error::msg)
            .context("tokenize batch")?;

        // Pad to the longest sequence in the batch so the tensor is rectangular.
        let max_len = encodings
            .iter()
            .map(|e| e.get_ids().len())
            .max()
            .unwrap_or(0)
            .max(1);
        let n = texts.len();
        let mut ids = Vec::with_capacity(n * max_len);
        let mut masks = Vec::with_capacity(n * max_len);
        for e in &encodings {
            let mut id: Vec<u32> = e.get_ids().to_vec();
            let mut m: Vec<u32> = e.get_attention_mask().to_vec();
            id.resize(max_len, 0); // pad id 0; the mask zeros these out below
            m.resize(max_len, 0);
            ids.extend_from_slice(&id);
            masks.extend_from_slice(&m);
        }

        let input_ids = Tensor::from_vec(ids, (n, max_len), &self.device)?;
        let attn = Tensor::from_vec(masks, (n, max_len), &self.device)?;
        let token_type_ids = input_ids.zeros_like()?; // single-segment input

        // (n, seq, hidden) last-hidden-state.
        let hidden = self
            .model
            .forward(&input_ids, &token_type_ids, Some(&attn))
            .context("bert forward")?;

        // Masked mean pool: sum token vectors where mask=1, divide by token count.
        let mask_f = attn.to_dtype(DTYPE)?.unsqueeze(2)?; // (n, seq, 1)
        let summed = hidden.broadcast_mul(&mask_f)?.sum(1)?; // (n, hidden)
        let counts = mask_f.sum(1)?; // (n, 1) — at least 1 (CLS is never masked)
        let mean = summed.broadcast_div(&counts)?;

        // L2-normalise so a later dot product is exactly cosine similarity.
        let norm = mean.sqr()?.sum_keepdim(1)?.sqrt()?;
        let normalized = mean.broadcast_div(&norm)?;
        Ok(normalized)
    }

    /// Smoke helper: embed one text and return its raw vector (length = hidden
    /// size, 384 for bge-small). Used by the 1.3a smoke test.
    pub fn embed_one(&self, text: &str) -> Result<Vec<f32>> {
        let t = self.embed(&[text.to_string()])?;
        Ok(t.get(0)?.to_vec1::<f32>()?)
    }

    /// Re-score `candidates` against `query` by cosine similarity, returning one
    /// score per candidate in the same order. Higher is more relevant.
    ///
    /// Embeds the query and all candidates in a single batched forward pass;
    /// since vectors are L2-normalised, the cosine reduces to a dot product.
    pub fn rerank(&self, query: &str, candidates: &[String]) -> Result<Vec<f32>> {
        if candidates.is_empty() {
            return Ok(vec![]);
        }
        let mut all = Vec::with_capacity(candidates.len() + 1);
        all.push(query.to_string());
        all.extend_from_slice(candidates);

        let emb = self.embed(&all)?; // (1 + m, hidden)
        let q = emb.get(0)?.unsqueeze(1)?; // (hidden, 1)
        let cand = emb.narrow(0, 1, candidates.len())?; // (m, hidden)
        let scores = cand.matmul(&q)?.squeeze(1)?; // (m,)
        Ok(scores.to_vec1::<f32>()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 1.3a smoke test: load bge-small, embed one sentence, expect a 384-dim
    /// finite vector. Network + model download required, so it is `#[ignore]`d
    /// by default. Run during verification with:
    ///   cargo test -p veles-core --features rerank -- --ignored embed_smoke
    #[test]
    #[ignore = "downloads bge-small-en-v1.5 (~130MB) on first run"]
    fn embed_smoke() {
        let r = Reranker::load(None).expect("load reranker");
        let v = r.embed_one("parse config file and fail").expect("embed");
        assert_eq!(v.len(), 384, "bge-small hidden size");
        assert!(v.iter().all(|x| x.is_finite()), "no NaN/Inf");
        // L2-normalised → norm ≈ 1.
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-3, "expected unit norm, got {norm}");
    }

    /// 1.3b rerank test: the on-topic candidate must outscore an off-topic one.
    #[test]
    #[ignore = "downloads bge-small-en-v1.5 (~130MB) on first run"]
    fn rerank_orders_by_relevance() {
        let r = Reranker::load(None).expect("load reranker");
        let cands = vec![
            "the SSE stream never opened so the proxy buffered the whole response".to_string(),
            "a recipe for sourdough bread with a long overnight rise".to_string(),
        ];
        let scores = r
            .rerank("server sent events streaming not working, response buffered", &cands)
            .expect("rerank");
        assert_eq!(scores.len(), 2);
        assert!(
            scores[0] > scores[1],
            "on-topic ({}) should beat off-topic ({})",
            scores[0],
            scores[1]
        );
    }
}
