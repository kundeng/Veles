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

### D5: The real crux is indexing SPEED, not the engine.  *(open — drives the design)*
**Context:** Transformer embedding gives the relevance (D2), but at **~13 chunks/s on CPU** a full
~41k-chunk index is ~50 min — the same slowness that eliminated ck. Swapping the model alone repeats
that mistake. **The design must make transformer-grade results affordable on CPU.** Options to weigh
(benchmark before choosing):
- **(a) Two-stage retrieve→rerank** *(owner guidance point 2: "rerank")* — cheap recall (BM25 +
  static potion, both instant to index) returns top-K; a transformer **reranks only those K at query
  time** (bounded cost, cacheable). **No full-corpus transformer index** → indexing stays fast. Lead
  candidate.
- **(b) One-time transformer index** — embed the whole corpus once (~50 min), incremental after;
  queries instant. Rejected-leaning: this is the slowness the owner already disliked.
- **(c) Faster embedding** — int8-quantized ONNX (2–4×), smaller model, better batch/threading.
  Complementary to (a).
**Why this matters:** it converts "add a transformer" (which would re-create ck's slowness) into
"keep fast indexing, spend transformer cost only on the handful of results that reach the user."

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
- [ ] 1.2 **Benchmark the speed-vs-quality architectures (D5)** — measure, on the distilled corpus:
  (a) two-stage BM25/static recall → transformer rerank top-K (query-time cost for K=20/50);
  (b) int8-quantized bge-small index throughput vs fp32 (the 13 chunks/s baseline). Pick the
  architecture in D5 from numbers. **This is the gating task — do before building.**
- [ ] 1.3 **Implement the chosen architecture in veles.** Likely: `Embedder` enum {Static|Onnx}
  fastembed backend used as a **reranker over top-K** (not a full-corpus index), preserving the fast
  BM25/static index and the lexical RRF channel.
- [ ] 1.4 **CLI + MCP prose search** end-to-end on the corpus, daemon-free from the CLI; per-corpus
  model choice recorded in the index manifest.

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
</content>
