# Data Architecture (anchor doc)

The horizontal contract every state-touching spec `anchors:`. Defines the tiers, the
data-lifecycle of every store, and storage-engine rationale. A schema (field list) is NOT
a data architecture — the load-bearing columns here are **source-of-truth?**, **read path**,
and **reproducible?**.

## Tier model

```
SOURCE (read-only, never written)        e.g. ~/.claude/projects/**/*.jsonl, a code repo
   │  distill / extract (preprocess)     verbose JSON → clean records; code → chunks+symbols
   ▼
DERIVED RECORDS (veles-owned)            distilled .md / structured records, in state dir
   │  embed + index
   ▼
INDEX (storage tier)                     BM25 (lexical) + vector (semantic) [+ symbols]
   │  query: lexical ∪ semantic → RRF → rerank
   ▼
RETRIEVAL (compute) → CLI / MCP (presentation)
```

**Boundary contracts:**
- SOURCE → DERIVED: one-way. The source folder is *never* written. Distillation is
  schema-blind (it distills "verbose JSON", knows nothing about "sessions").
- DERIVED → INDEX: derived records are the indexed unit; the index is rebuildable from them.
- INDEX → RETRIEVAL: lexical and semantic are *separate* indexes fused at query time (RRF),
  never a single blended store — so exact-token recall survives.

## Data-lifecycle table

| Store | Location | Writer | Immutable/rolling | Source-of-truth or derived | Read path | Retention | Reproducible? |
|-------|----------|--------|-------------------|---------------------------|-----------|-----------|---------------|
| **Source corpus** | repo / `~/.claude/projects` | external (user, agents) | rolling | **source of truth** | distiller reads | owned by user | n/a (is the truth) |
| **Distill state** | `<shadow>/.veles/` distill-state | coordinator / CLI one-shot | rolling (size+mtime per source) | derived | derive skip-check | with shadow | yes (re-derive) |
| **Derived records** | `~/.local/state/veles/folders/<hash>/derived/*.md` | distiller | rolling (re-derived on source change) | derived | indexer reads | until source gone | yes (from source) |
| **BM25 index** | `<index_root>/.veles/` generations | coordinator (sole writer) / CLI | immutable generations, atomic CURRENT swap | derived | lexical query | current + prev gen GC | yes (re-index) |
| **Vector index** *(planned)* | `<index_root>/.veles/` (sqlite-vec or lance) | same writer | immutable generations | derived | semantic query | with BM25 gen | yes (re-embed) |
| **Symbols** | `<index_root>/.veles/symbols.bin` | indexer | immutable generations | derived | defs/refs | with gen | yes (re-parse) |
| **Failure-memory** *(proposed)* | dedicated store (sqlite) | agent, deliberate append | **append-only** | **source of truth** (agent-authored) | grep + semantic fallback | long-lived | no (authored, not recomputed) |

**The reproducibility split that matters:** everything in the index tier is *derived* and
faithfully recomputable from source — so a generation can be GC'd and rebuilt without loss.
The proposed **failure-memory** store is the opposite: it is *authored* truth (the agent
deliberately records a failure record), NOT recomputed — so it must be durable, append-only,
and never silently regenerated. Mixing the two (treating authored memory as a derived index)
is the S11 trap to avoid.

## Storage-engine choices

- **Lexical:** keep veles' BM25 (in-binary, no dep) — or SQLite **FTS5** if we want exact-token
  features (prefix, phrase) cheaply. Both embeddable, no server.
- **Vector:** **sqlite-vec** (single file, embeddable, metadata columns alongside vectors → easy
  recency/outcome filtering) is the lead candidate; **LanceDB** if columnar/larger scale is needed.
  Decision deferred to spec 01; both satisfy "no server, CPU-only".
- **Embedding model:** general-language **CPU transformer** (bge-small / nomic-embed-text / e5) for
  prose-bearing (distilled) corpora; static potion stays acceptable for pure-code corpora. The
  model is a **per-corpus** choice, recorded in the index manifest (so a reader knows which model
  produced the vectors). This is the key change from today's single-static-model design.

## Open decisions (resolved in specs)

- sqlite-vec vs LanceDB for the vector store. (spec 01)
- Whether the vector index lives inside `.veles/` generations or a sibling store. (spec 01)
- Whether failure-memory is a first-class store now or a later spec. (steering: likely later spec)
- Embedding backend: in-process ONNX (`ort`/fastembed-rs) vs pure-Rust (candle) vs external. (spec 01)
</content>
