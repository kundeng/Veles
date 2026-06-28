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

### D1: Engine — adopt BeaconBay/ck vs extend veles  *(bake-off in progress)*
**Choice:** PENDING the head-to-head bake-off (task 1.1). Leaning **adopt ck** per the research
recommendation + benchmark (ck uses the same fastembed/bge-small that scored 0.58–0.78; it already
has hybrid RRF + CLI + MCP, so adopting it removes the embedder-refactor work from veles).
**Why:** the only gap ck has (req 6 distill/memory) is ours to build regardless of engine; adopting
a maintained engine lets us spend effort on the differentiator (distill + failure-memory), not on
re-implementing transformer embeddings in veles. Counter-weight: veles has symbol-nav + our distill
+ coordinator. The bake-off decides on evidence, not vibes.

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
- [ ] 1.1 **Engine bake-off** — build ck (in progress), index (a) the distilled transcript corpus and
  (b) a real code repo; run the 3 benchmark queries + 2 code queries; compare relevance, ergonomics,
  CLI/MCP surface, and index size/speed against veles+fastembed. **Record the verdict in D1.**
- [ ] 1.2 **Stand up the chosen engine** over the distilled transcript corpus with a transformer
  model (bge-small/nomic), hybrid RRF enabled. (Adopt-ck: index via ck. Extend-veles: implement the
  `Embedder` enum {Static|Onnx} and a fastembed backend.)
- [ ] 1.3 **CLI + MCP prose search** works end-to-end on the corpus, daemon-free from the CLI.
- [ ] 1.4 Record the per-corpus embedding-model choice in the index manifest (transformer=prose).

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
**2026-06-27** — Spec created. Steering + data-architecture anchor + research + benchmark already
committed. Transformer lever proven (D2). ck-cli building from source for the bake-off (1.1).
Frame ratified from owner guidance without asking (owner away, loop mode).
</content>
