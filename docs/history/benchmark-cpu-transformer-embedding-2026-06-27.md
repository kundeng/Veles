# Benchmark — CPU transformer embedding vs veles static (2026-06-27)

## Question
Does a CPU-only transformer embedding beat veles' static (`model2vec`/potion) embedding on
**prose** relevance, enough to justify changing the embedding backend?

## Method
- Corpus: veles' distilled transcript shadow (372 `.md`), 2500 chunks (~900 chars, round-robin
  across files, giant single-line hook dumps clipped to 2000 chars).
- Transformer: **bge-small-en-v1.5** via `fastembed` (ONNX, CPU, 4 threads), cosine over L2-normalized
  vectors. Baseline: veles `search --multilingual` (static potion-multilingual-128M).
- 3 queries: a prose "failure/firefighting" query, a technical "splunk anomaly" query, a meta query.

## Result

| Query | veles static (top score) | bge-small transformer (top scores) |
|-------|--------------------------|-------------------------------------|
| "things constantly breaking… firefighting… exhausting" | **0.018** — machinery hits | **0.61 / 0.60 / 0.59** — on-topic (failure-review subagents, "chaotic/mistake lessons") |
| "splunk anomaly detection threshold calibration" | (low) | **0.78 / 0.77** — all splunk-mp-anomaly |
| "veles distill jsonl session search" | 0.018 | **0.71** — on-topic |

~**33× cosine-score jump** on the prose query, and the hits are *actually relevant* instead of
noise (`output_tokens`, `## record N`). The static→transformer axis is the decisive relevance lever,
exactly as predicted. Confirms req 7 (real transformer embeddings) and that it is achievable
**CPU-only, in-process** (fastembed/ONNX; `onnxruntime` already on this box).

## Cost
13 chunks/s on 4 CPU threads → ~196s for 2500 chunks; ~50 min for the full ~41k-chunk corpus as a
one-time index. Tunable: more threads, larger batch, a faster/smaller model. Query time is trivial
(embed 1 query + cosine). Acceptable for an index-occasionally / search-often corpus.

## Caveats
- Distill noise (record headers, subagent-prompt boilerplate) still pollutes some hits — structured
  records + boilerplate dedup (req 6) would raise precision further.
- bge-small (384-dim, English) was the quick pick; nomic-embed-text (8192 ctx, better for long
  chunks) or bge-base may do better. Worth comparing in the engine bake-off.

## Implication for build-vs-adopt
This proves the transformer backend works in-process for **veles+fastembed (Option B)**. The
research doc recommends **adopting BeaconBay/ck (Option A)**, which already ships exactly this
(fastembed hybrid + MCP). Next step: a head-to-head **bake-off** (install BeaconBay/ck, run the same
queries on the same corpus + a code corpus) to choose the engine for spec 01. The local `ck` is an
unrelated broken Docker shim — BeaconBay/ck must be installed fresh.
</content>
