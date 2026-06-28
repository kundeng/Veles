# Context Tool Landscape — Build vs. Adopt for Veles

Date: 2026-06-27
Author: research agent (Claude)

## The question

We currently maintain a fork of **veles**, a local code/prose search tool. Its weakness is
that it is stuck on **static (model2vec / potion) embeddings**, which gives mediocre prose
relevance. Before we keep investing in our own fork, we want to know:

> Does an EXISTING, maintained open-source project already satisfy our full "one tool to
> rule them all" requirement set? If yes, adopt it. If no, which 1–2 projects are the
> closest adopt-or-borrow-from candidates, and which requirement(s) does nothing satisfy?

## The requirement set (the bar a candidate must clear)

1. **Fully local, offline at query time** — no cloud embedding API / SaaS. One-time local
   model download is OK.
2. **CPU-only** — must run well with NO GPU.
3. **Hybrid retrieval** — lexical/BM25 (exact tokens: error codes, stack frames,
   identifiers) AND semantic/embedding, fused (e.g. RRF); reranking a bonus.
4. **Both code AND prose** — source code (symbol-aware a plus) AND conversational/prose
   corpora (chat transcripts, notes). This dual nature is the crux.
5. **CLI + MCP server** — terminal usable AND an MCP stdio server for agents.
6. **Preprocessing / distillation** — can transform verbose sources (e.g. `.jsonl` agent
   transcripts) into clean indexable text before indexing, OR is extensible enough to add
   that. Bonus: structured/append-only "failure memory" record store with metadata
   filtering (recency, success/failure flag).
7. **Real (transformer) embeddings, pluggable** — not limited to static/word-vector
   embeddings; can use a CPU transformer model (bge-small, nomic-embed-text, e5, gte) so
   prose relevance is good. (This is exactly what veles lacks.)
8. **Shippable & maintained** — single binary or easy install, permissive license
   (MIT/Apache), active maintenance, reasonable popularity.

Scoring: ✓ = meets, ~ = partial / needs work, ✗ = does not meet.

## Comparison table

| Project | 1 Local | 2 CPU | 3 Hybrid | 4 Code+Prose | 5 CLI+MCP | 6 Preproc/memory | 7 Transformer emb | 8 Ship/maint | Stars | Lang | License | Last rel. | Disqualifying gap |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| **ck** (BeaconBay/ck) | ✓ | ✓ | ✓ (RRF) | ✓ | ✓ | ~ | ✓ (FastEmbed: bge-small/nomic/jina-code) | ✓ | ~1.6k | Rust | MIT+Apache | v0.7.11 May 2026 | **Viable.** No reranking, no jsonl-distill/failure-memory store — but both are addable on top. |
| **txtai** (neuml/txtai) | ✓ | ✓ | ✓ (sparse+dense, SPLADE/ColBERT + reranking in 9.x) | ✓ | ✓ (native MCP) | ✓ (pipelines/workflows + SQL metadata filter) | ✓ (any HF model) | ✓ | ~12.7k | Python | Apache-2.0 | v9.10 Jun 2026 | **Viable as a framework, not a product.** It's a library you assemble; not symbol-aware out of the box; you build the code/prose chunking + CLI UX yourself. |
| **LEANN** (StarTrail/yichuan-w) | ✓ | ~ | ~ (grep + semantic, not true BM25 fusion) | ✓ (AST chunking) | ✓ | ~ | ✓ (nomic/contriever/qwen) | ✓ | ~12.6k | Python | MIT | v0.3.7 Mar 2026 | CPU latency is high (its whole design recomputes embeddings on the fly); no real BM25 hybrid; weak for exact-token/agentic low-latency use. |
| **probe** (probelabs/probe) | ✓ | ✓ | ~ (BM25/TF-IDF + AST, optional BERT rerank; no vector index) | ✗ (code only) | ✓ | ✗ | ✗ (no embedding index by design) | ✓ | ~0.65k | Rust | Apache-2.0 | v0.6 Jun 2026 | Code-only and deliberately no embeddings — fails req 4 and 7 (prose semantic). |
| **codanna** (bartolli/codanna) | ✓ | ✓ | ~ (Tantivy FTS + 384-d MiniLM vectors; fusion unclear) | ~ (code-first, indexes md/txt for RAG) | ✓ | ✗ | ~ (AllMiniLM-L6, fixed) | ✓ | ~0.7k | Rust | Apache-2.0 | v0.9.22 May 2026 | Symbol/call-graph code-intelligence tool; prose is an afterthought, embedding model not pluggable, no transcript pipeline. |
| **engram** (199-bio/engram) | ✓ | ✓ | ✓ (FTS5 BM25 + Transformers.js + KG, RRF) | ✗ (prose memory only) | ✓ | ✓ (temporal decay, salience, append-only memory) | ✓ (mdbr-leaf-ir, <100M) | ~ (very new) | ~6 | TS | MIT | new repo | Excellent **memory** model but tiny/unproven, prose-only (no code search), Node/Transformers.js. Borrow ideas, don't adopt. |
| **Khoj** (khoj-ai/khoj) | ~ | ✓ | ~ | ✓ | ~ | ~ | ✓ | ✓ | large | Python | **AGPL-3.0** | Mar 2026 | Heavy "second brain" app (server + Postgres); AGPL license; overkill and not a CLI/MCP search primitive. |
| **OpenMemory / mem0** | ~ | ✓ | ~ (Qdrant vector; LLM extraction) | ✗ | ✓ | ✓ (memory records) | ✓ | ✓ | large | Python | Apache-2.0 | 2026 | Needs Docker + Postgres + Qdrant; LLM-extraction memory layer, not a code/prose hybrid search engine. Fails "single binary / easy install" spirit and req 4. |
| **txtai-assistant-mcp** (rmtech1) | ✓ | ✓ | ~ | ~ | ✓ | ~ | ✓ | ~ | small | Python | MIT | 2025 | Thin third-party MCP wrapper over txtai memory; not a maintained product, narrow. |
| **sqlite-vec + FTS5** (DIY stack) | ✓ | ✓ | ✓ (you wire RRF) | ✓ | ✗ (you build it) | ✓ (you build it) | ✓ (you choose) | ~ | n/a | C/your lang | Apache/MIT | active | This is "build your own," i.e. what veles already is. Not an adoptable product. |

## Per-candidate notes

### ck — BeaconBay/ck  (https://github.com/BeaconBay/ck)
Closest single match to the bar.
- **Hybrid:** explicit `--hybrid` flag, semantic + keyword fused with **Reciprocal Rank
  Fusion**. `--scores` exposes relevance. No cross-encoder reranking in the README.
- **Embeddings (req 7, the key one):** uses **FastEmbed** with real transformer models —
  default **BGE-Small**, plus **Nomic V1.5**, **Mixedbread xsmall**, and **Jina Code**.
  This is precisely the upgrade veles is missing (veles = static model2vec/potion).
- **Local / CPU:** "completely offline … model runs locally with no network calls."
  CPU-only via FastEmbed. Reqs 1, 2 ✓.
- **Code + prose:** Tree-sitter code chunking (Python, JS, Rust, Go, C/C++ …) AND prose
  (markdown, text, config). Req 4 ✓.
- **CLI + MCP:** ships a CLI and an MCP server with tools `semantic_search`,
  `regex_search`, `hybrid_search`, `index_status`, `reindex`, `health_check`. Req 5 ✓.
- **Gaps (req 6):** indexes files **as-is**; no jsonl-transcript distillation, no
  append-only failure-memory record store, **no metadata/recency filtering** (only score
  threshold + `--topk`). It emits jsonl output but does not ingest/preprocess jsonl input.
- **Ship/maint:** Rust, dual MIT+Apache, ~1.6k stars, active (v0.7.11, May 2026). Req 8 ✓.
- **Verdict:** meets reqs 1,2,3,4,5,7,8; **partial on 6**. The missing piece (transcript
  distillation + a metadata-filtered memory store) is an additive layer we already know how
  to build — and we'd build it in our own preprocessing step feeding `ck`'s index, not by
  modifying `ck` core.

### txtai — neuml/txtai  (https://github.com/neuml/txtai)
The most capable engine on paper, but it is a **framework, not a product**.
- Sparse + dense **hybrid** with first-class SPLADE/ColBERT and **reranking pipelines**
  (9.x). Any HuggingFace transformer embedding, CPU containers, **native MCP API**,
  **pipelines/workflows** for arbitrary preprocessing (so jsonl distillation is natural),
  and SQL-style metadata filtering on records (recency/flags doable). That covers reqs
  1,2,3,5,6,7,8 strongly.
- The catch for req 4/8: it is not symbol-aware and ships no opinionated code/prose
  chunking or end-user CLI UX — you assemble the indexer, chunker, CLI, and MCP tool
  surface yourself. So "adopt txtai" really means "build veles-2 on top of txtai." That's a
  legitimate path (and the best path if we want SPLADE/ColBERT + reranking), but it is
  build-on-a-library, not drop-in adopt.
- Sources: https://github.com/neuml/txtai , 9.0 release notes (SPLADE/ColBERT/reranking),
  6.0 notes (sparse/hybrid).

### LEANN — https://github.com/yichuan-w/LEANN
Big stars (~12.6k), MIT, real transformer embeddings, AST chunking, MCP for Claude Code.
But its defining trick — storing <5% of the index and **recomputing embeddings on the
fly** — trades storage for **CPU latency**. On a CPU-only laptop with a decent embedding
model, query latency climbs (community reports call it "too high for agentic workflows").
It also offers grep + semantic but not a true BM25-fused hybrid. Wrong latency profile for
our agent use case.

### probe — https://github.com/probelabs/probe
Fast, local, Rust, BM25/TF-IDF + AST with optional BERT rerank, CLI + MCP. But it
**deliberately avoids an embedding/vector index** ("a third path between grep and vectors")
and is **code-only**. Fails req 4 (prose semantic) and req 7 (no transformer vector
relevance for prose). Great for code grep, wrong tool for chat transcripts.

### codanna — https://github.com/bartolli/codanna
Rust code-intelligence MCP: Tree-sitter symbols/call-graphs, Tantivy FTS, 384-d MiniLM
vectors from doc comments. Strong for code navigation, but embedding model is fixed
(AllMiniLM-L6), prose is secondary, and there's no transcript pipeline or memory store.

### engram — https://github.com/199-biotechnologies/engram
The best **failure-memory** design we saw: SQLite FTS5 (BM25) + Transformers.js
(mdbr-leaf-ir transformer) + knowledge-graph, fused with RRF, plus **temporal decay
(Ebbinghaus), salience, and append-only recall** — exactly req 6's bonus. But it's
prose-memory-only (no code search), Node/Transformers.js, and brand new (~6 stars). Mine it
for the memory-store design; don't adopt it as the engine.

### Khoj — https://github.com/khoj-ai/khoj
Self-hostable "AI second brain." Powerful but a heavyweight server app (Postgres), and
**AGPL-3.0** — a licensing non-starter for embedding in our shippable tool, and far larger
in scope than a CLI/MCP search primitive.

### mem0 / OpenMemory — https://github.com/mem0ai/mem0
Local-first memory layer (Docker + Postgres + Qdrant), LLM-extraction based. It's a memory
service, not a code+prose hybrid search engine; fails the "easy install / single binary"
spirit and req 4.

### DIY: sqlite-vec/sqlite-vss + FTS5
Technically can meet 1,2,3,4,6,7 — but that's "build your own," which is what veles already
is. Not an adoptable third-party product; only relevant as an implementation substrate.

## Requirements nothing fully satisfies in one drop-in tool

- **Req 6 (transcript distillation + structured append-only failure-memory with
  recency/success-flag metadata filtering)** is the gap **no shippable code+prose hybrid
  search product** covers. The only project that nails the memory model (engram) is
  prose-only and immature; the only framework that can express it (txtai) requires you to
  build it. So req 6 is the genuine "must-build-ourselves" layer regardless of which engine
  we pick.
- The combination of **(req 4 code+prose) + (req 7 pluggable transformer embeddings) + (req
  3 true BM25-RRF hybrid) + (req 5 CLI+MCP) all in one maintained binary** is met by exactly
  **one** product: **ck**. Everything else fails at least one of those.

## Recommendation

**There is no single tool that meets all of 1–8 out of the box.** Req 6 (transcript
distillation + a metadata-filtered failure-memory store) is unmet by every shippable
code+prose search product. But two clear adopt-or-build-on candidates emerge:

1. **Adopt `ck` (BeaconBay/ck) as the engine — top recommendation.** It is the *only*
   maintained, single-binary product that satisfies reqs 1,2,3,4,5,7,8 today, and crucially
   it fixes veles's core weakness: it uses **real FastEmbed transformer embeddings
   (bge-small / nomic / jina-code) on CPU**, not static model2vec. Adoption cost is low and
   bounded: we keep our own thin preprocessing layer (jsonl→clean-text distillation) and a
   small metadata/recency "failure-memory" record store *in front of* `ck`'s index — i.e.
   exactly req 6, which no engine gives us anyway. Risk: `ck` has no cross-encoder
   reranking and no metadata filtering inside its index; we'd add reranking later or live
   with RRF.

2. **Build on `txtai` — strongest if we want SPLADE/ColBERT + reranking and full control.**
   It gives transformer embeddings, sparse+dense hybrid with reranking, native MCP,
   metadata filtering, and arbitrary preprocessing pipelines — but no code/prose chunking,
   no symbol-awareness, and no CLI UX out of the box, so it's "build veles-2 on txtai,"
   more work than adopting `ck`.

   And **borrow engram's memory design** (FTS5 BM25 + transformer + RRF + temporal-decay /
   salience / append-only recall) when we implement the req-6 failure-memory store on top of
   whichever engine we pick.

**Bottom line:** stop extending veles's static-embedding core. Adopt **`ck`** as the
hybrid+transformer engine (it directly solves the embedding-quality problem driving this
search), and build only the thin req-6 distillation/memory layer ourselves, taking the
design from **engram**. Fall back to **txtai** only if we decide we need SPLADE/ColBERT +
reranking badly enough to assemble our own engine.

## Sources

- ck — https://github.com/BeaconBay/ck
- txtai — https://github.com/neuml/txtai (9.0 release: SPLADE/ColBERT/reranking; 6.0:
  sparse/hybrid)
- LEANN — https://github.com/yichuan-w/LEANN , paper https://arxiv.org/abs/2506.08276
- probe — https://github.com/probelabs/probe , https://probeai.dev
- codanna — https://github.com/bartolli/codanna
- engram — https://github.com/199-biotechnologies/engram
- Khoj — https://github.com/khoj-ai/khoj
- mem0 / OpenMemory — https://github.com/mem0ai/mem0/tree/main/openmemory
- txtai-assistant-mcp — https://github.com/rmtech1/txtai-assistant-mcp
