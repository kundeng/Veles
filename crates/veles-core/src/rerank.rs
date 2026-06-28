//! HTTP-delegated transformer reranker — the precision second stage.
//!
//! The base index uses a *static* model2vec embedding: fast on CPU, but
//! semantically blunt (BM25-grade prose relevance). That's great for **recall**
//! and weak for **precision**. So we keep the cheap stage for recall (top-K),
//! then re-score just those K candidates with a *real* transformer.
//!
//! Crucially, we do **not** bundle the transformer. Instead we POST the
//! candidate texts to a local **OpenAI-compatible `/v1/embeddings`** server —
//! LM Studio, ollama, HuggingFace TEI, Infinity, llama.cpp-server, … — and let
//! it run the model on whatever GPU/runtime it already has. That keeps veles a
//! lean single binary (just a tiny HTTP client), works cross-vendor (the
//! server's Vulkan/CUDA/Metal, not ours), and is server-agnostic: every one of
//! those servers speaks the identical `/v1/embeddings` request/response shape.
//!
//! The reranker is a **bi-encoder**: embed the query and each candidate, then
//! rank by cosine. That needs only `/v1/embeddings` (uniform across servers).
//! A cross-encoder `/rerank` endpoint (TEI/Infinity only) is a possible later
//! precision upgrade, but it would break server-uniformity, so we don't use it.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

/// Default endpoint: LM Studio's local server. Override for ollama
/// (`http://localhost:11434/v1/embeddings`), TEI, etc. via `--rerank-url` /
/// `VELES_RERANK_URL`.
pub const DEFAULT_RERANK_URL: &str = "http://localhost:1234/v1/embeddings";

/// Default model id. Must match a model the target server actually has loaded;
/// ollama users typically want `nomic-embed-text`. Override via `--rerank-model`
/// / `VELES_RERANK_MODEL`.
pub const DEFAULT_RERANK_MODEL: &str = "nomic-embed-text";

/// Default recall depth for the cheap first stage when reranking. The
/// transformer cost is bounded to these K candidates; K=50 is a good
/// precision/latency tradeoff. Both CLI and MCP default here.
pub const DEFAULT_RERANK_RECALL: usize = 50;

/// A reranker that delegates embedding to a local `/v1/embeddings` server.
/// Construct once and reuse across queries (the `ureq::Agent` pools connections).
pub struct HttpReranker {
    url: String,
    model: String,
    agent: ureq::Agent,
}

#[derive(Serialize)]
struct EmbedRequest<'a> {
    model: &'a str,
    input: &'a [String],
    encoding_format: &'a str,
}

#[derive(Deserialize)]
struct EmbedResponse {
    data: Vec<EmbedDatum>,
}

#[derive(Deserialize)]
struct EmbedDatum {
    embedding: Vec<f32>,
    /// OpenAI returns the input position; we sort by it so order is guaranteed
    /// even if a server reorders the batch.
    #[serde(default)]
    index: usize,
}

impl HttpReranker {
    /// Build a reranker targeting `url` (default [`DEFAULT_RERANK_URL`]) and
    /// `model` (default [`DEFAULT_RERANK_MODEL`]). No network call here — the
    /// server is contacted lazily on the first `rerank`.
    pub fn new(url: Option<&str>, model: Option<&str>) -> Self {
        let agent = ureq::AgentBuilder::new()
            .timeout(std::time::Duration::from_secs(120))
            .build();
        Self {
            url: url.unwrap_or(DEFAULT_RERANK_URL).to_string(),
            model: model.unwrap_or(DEFAULT_RERANK_MODEL).to_string(),
            agent,
        }
    }

    /// Resolve config from explicit args, then env (`VELES_RERANK_URL`,
    /// `VELES_RERANK_MODEL`), then the defaults.
    pub fn from_env_or(url: Option<&str>, model: Option<&str>) -> Self {
        let env_url = std::env::var("VELES_RERANK_URL").ok();
        let env_model = std::env::var("VELES_RERANK_MODEL").ok();
        Self::new(
            url.or(env_url.as_deref()),
            model.or(env_model.as_deref()),
        )
    }

    pub fn endpoint(&self) -> &str {
        &self.url
    }
    pub fn model(&self) -> &str {
        &self.model
    }

    /// Embed a batch via `POST /v1/embeddings`, returning one L2-normalised
    /// vector per input, in input order.
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let req = EmbedRequest {
            model: &self.model,
            input: texts,
            encoding_format: "float",
        };
        let resp = match self.agent.post(&self.url).send_json(&req) {
            Ok(r) => r,
            Err(ureq::Error::Status(code, r)) => {
                let body = r.into_string().unwrap_or_default();
                bail!(
                    "embeddings server at {} returned HTTP {code}: {}",
                    self.url,
                    body.chars().take(300).collect::<String>()
                );
            }
            Err(e) => bail!(
                "cannot reach embeddings server at {} ({e}). Start LM Studio / ollama \
                 (or pass --rerank-url) and load model {:?}.",
                self.url,
                self.model
            ),
        };

        let parsed: EmbedResponse = resp
            .into_json()
            .context("parse /v1/embeddings response")?;
        if parsed.data.len() != texts.len() {
            bail!(
                "embeddings server returned {} vectors for {} inputs",
                parsed.data.len(),
                texts.len()
            );
        }
        let mut data = parsed.data;
        data.sort_by_key(|d| d.index); // guarantee input order
        let mut out: Vec<Vec<f32>> = data.into_iter().map(|d| d.embedding).collect();
        for v in &mut out {
            l2_normalize(v);
        }
        Ok(out)
    }

    /// Re-score `candidates` against `query` by cosine similarity (one score per
    /// candidate, same order). Embeds query + candidates in one batched request.
    pub fn rerank(&self, query: &str, candidates: &[String]) -> Result<Vec<f32>> {
        if candidates.is_empty() {
            return Ok(vec![]);
        }
        let mut all = Vec::with_capacity(candidates.len() + 1);
        all.push(query.to_string());
        all.extend_from_slice(candidates);

        let emb = self.embed(&all)?;
        let q = &emb[0];
        // Vectors are L2-normalised, so cosine == dot product.
        Ok(emb[1..].iter().map(|c| dot(q, c)).collect())
    }
}

fn l2_normalize(v: &mut [f32]) {
    let n: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if n > 0.0 {
        for x in v.iter_mut() {
            *x /= n;
        }
    }
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cosine_of_normalised_vectors() {
        // Pure-math check (no network): identical dir → 1, orthogonal → 0.
        let mut a = vec![3.0_f32, 4.0];
        let mut b = vec![3.0_f32, 4.0];
        let mut c = vec![-4.0_f32, 3.0];
        l2_normalize(&mut a);
        l2_normalize(&mut b);
        l2_normalize(&mut c);
        assert!((dot(&a, &b) - 1.0).abs() < 1e-6);
        assert!(dot(&a, &c).abs() < 1e-6);
    }

    #[test]
    fn empty_candidates_is_empty() {
        let r = HttpReranker::new(None, None);
        assert!(r.rerank("q", &[]).unwrap().is_empty());
    }
}
