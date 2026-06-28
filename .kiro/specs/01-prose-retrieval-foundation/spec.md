---
spec_id: 01-prose-retrieval-foundation
status: ACTIVE
since: 2026-06-27
until: null
epic: retrieval
features: [engine-decision, transformer-prose-embedding, hybrid-rrf-prose, cli-mcp-prose]
supersedes: []
superseded_by: null
depends_on: []
anchors: [data-architecture]
---

# Prose Retrieval Foundation — transformer embeddings + engine decision

<!-- YAML above is the source of truth for status/relationships. -->

## Mental Model & Invariants
<!-- Ratified frame from the owner's guidance (2026-06-27), in THEIR vocabulary.
     A fresh agent reads this FIRST. -->

**Model (owner's words):**
- One **local** tool for **code search AND prose search** — hybrid **grep + semantic**, CLI + MCP.
- **Distill before you index** — extract high-signal records (esp. failure records), don't blindly
  embed raw transcripts.
- **Stay hybrid** — failure recall is often near-exact (exception classes, error codes, paths, stack
  frames); dense embeddings smear those, so keep a lexical/BM25 channel and fuse (RRF), rerank.
- **General-language embedding, not code** — the corpus is conversational.
- **Embeddable storage, no server** (sqlite-vec / LanceDB + FTS); metadata filtering (recency,
  outcome) matters as much as similarity.
- **Maybe structured memory is the foundation** — an append-only failure-signature store the agent
  writes deliberately, with semantic search as the fuzzy fallback. (← spec 02.)

**Invariants any solution must hold:**
1. Local/offline at query time; **CPU-only**; no cloud embedding.
2. Hybrid: the **lexical exact-token channel is preserved** (never a single blended store).
3. Source corpus is **never written**; derived records live in veles-owned state.
4. Embedding model is a **per-corpus** choice (transformer for prose, static OK for pure code),
   recorded in the index manifest.
5. Derived index is **reproducible** from source; authored failure-memory (spec 02) is **append-only
   source-of-truth**, never silently regenerated. (See [data-architecture].)

## Context

veles fork (v0.6.1) gives local hybrid BM25+semantic, CLI+MCP, distill jsonl→md shadow — but its
**static** (model2vec/potion) embedding makes prose relevance noise-floor (0.018). A CPU benchmark
proved a transformer embedding (bge-small via fastembed) scores the same prose **0.58–0.78 and
on-topic** (`docs/history/benchmark-cpu-transformer-embedding-2026-06-27.md`). The GitHub survey
(`docs/history/research-context-tool-landscape-2026-06-27.md`) found **BeaconBay/ck** already ships
exactly this (fastembed hybrid + CLI/MCP) and recommends adopting it as the engine. This sprint
**decides the engine** and stands up transformer-grade prose retrieval on the distilled corpus.

## Constraints

- CPU-only, offline at query time, single-binary-shippable (no GPU, no cloud embedding, no server).
- Keep the lexical/BM25 channel and RRF fusion — exact-token recall is non-negotiable.
- The distilled transcript corpus already exists at `~/.local/state/veles/folders/<hash>/derived`.
- Out of scope (→ later specs): structured failure-records distillation & boilerplate dedup (req 6),
  the append-only failure-memory store (req 6 bonus), cross-encoder reranking.

## Decisions

### D1: Engine — extend veles. ck ELIMINATED.  *(resolved 2026-06-27)*
**Choice:** **Extend veles.** BeaconBay/ck is eliminated.
**Why (evidence, not vibes):**
1. Owner's prior experience: ck is **slow at indexing**.
2. ck uses **fastembed** — the same engine my benchmark measured at **~13 chunks/s on CPU**. ck
   cannot be faster than its own embedding backend; it shares the exact bottleneck, so it offers no
   speed advantage and its one differentiator (transformer embeddings) is something veles+fastembed
   gets identically.
3. ck **won't run on this box**: prebuilt needs GLIBC ≥2.38 (box has 2.35), no musl/static asset,
   and the source build fails to compile here.
   → Adopting ck buys nothing veles+fastembed doesn't, while losing veles' symbol-nav, distill, and
   coordinator. Decision closed.

### D5: Architecture = two-stage retrieve→rerank (fast index, bounded query cost).  *(resolved 2026-06-27)*
**Context:** Transformer embedding gives the relevance (D2), but a full ~41k-chunk transformer index
is ~50 min on CPU — the slowness that eliminated ck. **The design must make transformer-grade results
affordable on CPU.**
**Choice:** **(a) Two-stage retrieve→rerank.** Keep veles' fast BM25+static index (instant, no
change); for a prose query, take the top-K candidates from that cheap recall and **rerank only those
K with a transformer at query time**. No full-corpus transformer index.
**Why (measured, task 1.2):** bge-small on CPU reranks **K=50 in 599 ms** (K=20: 204 ms; query embed:
3 ms) — interactive latency, and indexing stays as fast as today. This spends transformer cost only
on the handful of results that reach the user, exactly the owner's "fuse with RRF, then rerank"
guidance. Rejected: (b) one-time full transformer index (~50 min — the slowness owner disliked).
Complementary, later: (c) int8-quantized model to cut the 599 ms further / raise K.

### D2: Transformer embedding is the relevance lever  *(proven)*
**Choice:** Use a general-language CPU transformer embedding (bge-small now; evaluate nomic-embed for
long chunks). **Why:** benchmark shows ~33× score jump and on-topic hits vs static. Settled.

### D3: Hybrid + RRF, lexical channel preserved
**Choice:** Keep BM25/lexical alongside vector, fuse with RRF. **Why:** owner invariant — exact-token
failure recall (error codes, stack frames) must not be smeared by dense vectors.

### D4: Structured failure-memory deferred to spec 02
**Choice:** This sprint is unstructured prose retrieval; the append-only failure-signature store
(borrowing engram's FTS5+transformer+RRF+temporal-decay design) is spec 02. **Why:** keep the sprint
atomic; prove the retrieval foundation first.

### D6: GPU is an optional accelerator, never required.  *(2026-06-27)*
**Context:** Owner has a local **RTX 5070 Ti (16 GB)** and asked if GPU changes the design.
**Choice:** **GPU-optional.** CPU stays the portable floor (D5 two-stage rerank). GPU plugs in via the
**execution provider** (ONNX CUDA EP / candle CUDA) under the same `Embedder` abstraction — no
architecture change. When a GPU is present, unlock a "**full dense transformer index + bigger model**"
fast path (e.g. nomic-embed / Qwen3-Embedding-0.6B): GPU embeds the whole ~41k-chunk corpus in
~10–30 s, so first-stage recall can be semantic too, not just BM25/static.
**Why:** GPU changes the *ceiling* (index everything densely, bigger models), not the *architecture*.
It does NOT change the highest-leverage work (distill/structured records — GPU-independent), the need
for the lexical channel (exact-token recall), or shippability. GPU-optional dominates: owner's box
gets speed+quality, the tool still runs CPU-only elsewhere. Does **not** revive ck (GLIBC, unrelated).
**Consequence:** the embedding backend must expose a CPU/CUDA execution-provider switch; index manifest
records which model + provider produced the vectors (so a CPU reader never mis-reads a GPU-built index).

### D7: jsonl cleaning = external-transform pipeline (config-as-code), NOT veles core.  *(2026-06-27)*
**Context:** "How to clean jsonl without domain knowledge?" veles must stay format-blind.
**Choice:** Use veles' **existing** `pipeline.rs` mechanism: a `veles.pipeline.json` stage declares
`{source glob, transform: [program, args...]}`; veles runs `<transform> <abs-source>` and indexes the
stdout. The domain-specific cleaning (structured failure records, turn-boundary chunking, dedup,
metadata) lives in a **pluggable external command/extension** (python/DSL/any executable), wired in
config. The built-in Rust value-shape noise filter stays as the generic floor for the no-config case.
**Why:** matches owner's "run this extension / steps / pipeline / DSL as config-as-code"; keeps veles
agnostic; the structured-record cleaner that gave **+43% P@5** (D8 numbers) is just a better transform
script, no core change. **Measured:** prose-only cleaning lifted P@5 0.37→0.53, MRR 0.540→0.743.

### D8: Transformer embedding policy — rerank on CPU always; full dense index only on GPU.  *(2026-06-27)*
**Context:** Owner asked: "transformer only when GPU available, else CPU too slow — right or wrong?"
**Measured numbers (bge-small, this box):**
- **Full-corpus transformer index on CPU: ~12–40 chunks/s → ~41k chunks ≈ 20–57 min.** Too slow. ✓ owner.
- **Transformer rerank of top-K=50 on CPU: 599 ms/query.** Fast. ✗ owner ("CPU too slow" is wrong here).
- **Clean records (preprocessing): +43% P@5, GPU-independent — the biggest lever of all.**
**Choice (owner half-right):**
- **Always** use transformer as a **reranker over top-K** (BM25/static recall) — viable on CPU (0.6 s).
- **Gate the full dense transformer index on GPU** (RTX embeds 41k in ~10–30 s) — there the owner is
  right: don't full-index on CPU.
- **Prioritize clean records first** — it beats the embed-everything-vs-rerank question and costs no GPU.
**Net:** you don't need a GPU to get transformer-quality *results* (rerank delivers it on CPU); you need
a GPU only to make *semantic first-stage recall* (full dense index) affordable. CPU floor = clean
records + BM25/static recall + transformer rerank.

## Dev Environment (config-as-code — pointers only)
<!-- Rule 18: read the real config, don't copy values here. -->
- Engine source (if extend-veles): this repo (`crates/`), `cargo build --release -p veles-cli --features dashboard`.
- ck (if adopt): `cargo install --git https://github.com/BeaconBay/ck --tag 0.7.11 ck-cli` (prebuilt
  linux binary needs GLIBC ≥2.38 — build from source on this box).
- Distilled corpus: `~/.local/state/veles/folders/19b98696fd2c2982/derived` (the test corpus).
- Embedding bench harness: `/tmp/embbench.py` (fastembed bge-small, CPU).
- Search tool for this project's own history: veles (`veles search "q" ~/.claude/projects`), not grep.

## Tasks
<!-- [ ] pending | [x] done | [!] BLOCKED | [-] DROPPED: reason | [>] → spec_id -->

### P1 — Must Do
- [x] 1.1 **Engine bake-off — ck eliminated, extend veles.** Resolved by elimination (D1): ck is slow
  at indexing (owner), shares fastembed's ~13 chunks/s bottleneck (no advantage), and won't run on
  this box (GLIBC 2.35 < 2.38; source build fails). Evidence recorded in D1.
- [x] 1.2 **Architecture benchmarked (D5)** — two-stage rerank K=50 = 599ms CPU; clean records +43% P@5.
  Chosen: BM25/static recall → transformer rerank top-K. Backend = **candle** (pure Rust, single-binary,
  auto-GPU) not ort/onnxruntime (the ck portability trap). Recorded D5/D6/D8.
- [ ] 1.3 **candle reranker — bolt-on, single core path.** Sub-tasks:
  - [ ] 1.3a Add candle-core/nn/transformers + tokenizers + hf-hub to veles-core behind a `rerank`
    cargo feature; load **bge-small-en-v1.5** (BERT) + tokenizer via hf-hub; `Device::cuda_if_available`.
    Smoke: embed one string → 384-dim L2-normed vector. Default build (no feature) unchanged.
  - [ ] 1.3b `Reranker::rerank(query, candidates: &[text]) -> Vec<f32>` (mean-pool + cosine). Unit test
    on a tiny fixture (relevant doc outranks distractor).
  - [ ] 1.3c **One** core fn `search_with_rerank(index, query, k_recall, rerank: Option<&Reranker>)`
    in veles-core: hybrid recall top-`k_recall`, then reorder by reranker if present. Reuse `VelesIndex::search`.
- [ ] 1.4 **Wire CLI + MCP to the same core fn (no dual path).** CLI `--rerank` flag → `search_with_rerank`;
  MCP `search` gains a `rerank` arg → the **same** fn. Default off; feature-gated.

- [x] 1.5 **Structured-record cleaner built + wired (the +43% lever).** `pipelines/session_distill.py`
  (external transform per D7) + `pipelines/veles.pipeline.json`. Keeps user/assistant prose, brief
  thinking, tool_result ERRORS; drops snapshots/hooks/last-prompt/ai-title/queue-ops/tool-params/
  successful-dumps/scaffolding; truncates + dedupes. **Validated end-to-end:** `veles transform`
  distilled 229 sessions → 28 MB clean index / 2737 chunks in **8.7 s** (vs noisy 720 MB). veles
  stays format-blind. Static scores stay ~0.009 (expected — needs the transformer to realize the gain).

### P2 — Should Do
- [ ] 2.1 Verify: the 3 prose queries + 2 code queries return on-topic top hits via the shipped path
  (this is the verification task — system-level, not unit).
- [ ] 2.2 Document install + usage (README/quick-start) for the chosen engine path.

### P3 — Nice to Have
- [ ] 3.1 nomic-embed-text vs bge-small comparison for long (8k-ctx) chunks on this corpus.

## Open Questions
- [ ] If adopt-ck: does the project repo become "distill/memory layer over ck" (rename/reorg from the
  veles fork)? Resolve after D1.
- [ ] sqlite-vec vs ck's built-in index for the (later) failure-memory store. (→ spec 02)

## Log
**2026-06-27** — Spec created. Steering + data-architecture anchor + research + benchmark committed.
Transformer lever proven (D2).
**2026-06-27** — Bake-off resolved by elimination: **ck out** (slow indexing per owner; same fastembed
13 chunks/s bottleneck; won't run — GLIBC 2.35 < 2.38, source build fails). Engine = **extend veles**
(D1). Key reframe (D5): the crux is **indexing speed**, not the engine — pivot the design to
**two-stage retrieve→rerank** (fast index, transformer cost bounded to top-K) so we don't re-create
ck's slowness. Task 1.1 done; 1.2 now the gating benchmark. Cleaned up all ck build processes/dirs.
**2026-06-27** — Owner offered local **RTX 5070 Ti** → D6: GPU-optional accelerator (CPU floor stays).
**2026-06-27** — Task 1.2 measured: two-stage rerank latency = **K=50 in 599ms CPU** (D5 confirmed,
viable). Ran a real BM25-recall→bge-rerank validation: the rerank **mechanism works** (reorders toward
topical relevance), BUT every top chunk is still distill machinery (`toolUseResult.structuredPatch`,
`message.content[...]`). **Empirical lesson: transformer rerank cannot overcome a noisy corpus —
structured distill records are the DOMINANT quality lever, above the embedding model and GPU.** This
elevates the structured-distill layer (→ spec 02) to the top quality priority; spec 01's retrieval
architecture (two-stage rerank, GPU-optional) is sound but gated on clean records to actually shine.
(Validation run also had a sampling artifact — sequential fill let one giant session dominate the
2500-chunk slice; not corpus-representative, but the noise-ceiling conclusion holds.)
</content>
