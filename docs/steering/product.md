# Product Vision — local context & memory tool

## North star

**One local tool for both code search and prose search** — hybrid grep + semantic,
usable as **CLI and MCP**, with a **preprocessing/distill** step that turns verbose
sources (agent transcripts, exports) into clean, high-signal records before indexing.

**Hard constraints (non-negotiable):**

- Fully **local / offline at query time** — no cloud embedding API.
- **CPU-only** — no GPU required. (One-time local model download is fine.)
- **Shippable** — single self-contained binary, easy install.

## Why this exists

Agents and humans need to answer two question shapes against the same local corpus:

1. **Code:** "where is X handled", "find definition/references", "similar code".
2. **Prose / memory:** "have I seen this failure before", "what did we decide about Y",
   "find that session where we discussed Z".

No single tool we tried does both well under the constraints. This project is the
attempt to build (or adopt) that one tool.

## Attempts log (so we don't re-evaluate dead ends)

| Tool | Code | Prose | Verdict |
|------|------|-------|---------|
| **ck** | ok | weak | Too slow. Superseded by veles. |
| **Serena** | strong (LSP/symbol) | not built for it | Good at code, weak prose. Retired 2026-06-13. |
| **semble** | — | static | Python, not shippable. Retired. |
| **semtools** (run-llama) | ok | static only | Same `model2vec`/`potion` static tier as veles — "fuzzy semantic keyword search." No prose-quality gain. Dead end. (verified 2026-06-27) |
| **veles** (fork `kundeng/Veles`) | strong | weak | Hybrid BM25 + static. **Current platform.** |

**Core realization:** veles, semtools, semble all sit in the **static-embedding tier**.
The axis that matters for prose is **static vs transformer embeddings**, not the tool.

## Owner guidance shaping the design (2026-06-27)

1. **Distill before index** — extract structured, high-signal records (esp. *failure
   records*: error signature · what was attempted · outcome · resolution), not every chat
   line. Failing that: chunk by turn/event boundary (not fixed windows), dedup near-identical
   messages, truncate giant tool dumps, attach metadata (session id, timestamp, success/failure).
2. **Stay hybrid** — failure recall is often near-exact (exception classes, error codes, paths,
   stack frames); dense embeddings smear those. Run BM25 (FTS5/tantivy) alongside vector search,
   fuse with RRF, rerank. Semantic dominates, lexical is the exact-match safety net.
3. **General-language embedding, not code** — corpus is conversational; use nomic-embed-text /
   BGE-base/M3 / E5, not a code-specific model.
4. **Embeddable storage, no server** — sqlite-vec or LanceDB for vectors + an FTS index.
   Metadata filtering (recency, outcome) matters as much as similarity.
5. **Consider structured memory as the foundation** — an append-only failure-signature store the
   agent writes deliberately (and greps), with semantic search as the fuzzy fallback for
   unstructured recall. The two compose.

## Build vs adopt (bake-off pending)

GitHub survey done (`docs/history/research-context-tool-landscape-2026-06-27.md`): **no single tool
meets all 8 reqs** — the only gap is *our* distill + failure-memory layer (req 6), which we build
regardless of engine. Two engine options:

- **Option A — adopt BeaconBay/ck** (Rust, MIT/Apache, ~1.6k★): already ships local CPU hybrid
  BM25+semantic RRF over code+prose, CLI+MCP, **fastembed transformer embeddings**. We'd build only
  the distill/memory layer in front. (Note: the local `ck` binary is an *unrelated broken Docker
  shim* — BeaconBay/ck must be installed fresh.)
- **Option B — extend veles**: add a fastembed/ONNX transformer backend to veles (the `Embedder`
  enum refactor). Keeps veles' symbol-nav + distill + coordinator + our v0.6.1 fixes.

**Decision: pending a head-to-head bake-off** (same queries, same corpus) — recorded in spec 01's
Decisions. The benchmark below proves Option B is technically feasible; the bake-off decides whether
ck's maturity beats keeping our integrated stack.

## Status (2026-06-27)

- veles fork at **v0.6.1** (pushed): distill jsonl→md shadow works; CLI-only search fixed
  (`ingest::prepare_for_read`, one shared core path with MCP); generic value-shape distill noise
  filter (index −53%).
- **Transformer-embedding lever PROVEN** (`docs/history/benchmark-cpu-transformer-embedding-2026-06-27.md`):
  bge-small CPU embedding scores prose at **0.58–0.78 and on-topic** vs veles static **0.018** — a
  ~33× jump. CPU-only, ~13 chunks/s one-time index. This is the relevance fix.
- Next: engine bake-off (ck vs veles+fastembed) → spec 01.
</content>
