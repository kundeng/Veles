# Project Pillars

Load-bearing dimensions that cut across specs. A spec says "what to build"; this says
"where we are across all dimensions."

| Pillar | Means | Healthy when |
|--------|-------|-------------|
| **MVP / Ship** | A user/agent can search code AND prose locally from CLI + MCP | Both corpora return useful hits end-to-end; single binary installs clean; no GPU/cloud needed |
| **Retrieval quality** | Results are actually relevant | Prose queries surface the right record near top (not noise-floor); exact-token (error code/path) recall is reliable via the lexical channel |
| **Distill / preprocess** | Verbose sources become high-signal records | Transcripts → clean per-turn/failure records, deduped, dumps truncated, metadata attached; index is dense not noisy |
| **Design / Arch** | Specs, steering, data-architecture match code | data-architecture.md current; every state-touching spec anchors it; no stale claims |
| **Docs** | Install + usage + algorithm docs for humans and agents | README quick-start, MCP setup, distill model explained |

## Current state (2026-06-27)

- **MVP/Ship:** ✓ code search strong; ✓ prose search now *works* CLI-only (v0.6.1); ⚠ prose
  relevance weak.
- **Retrieval quality:** ⚠ BM25-grade on prose (static embedding limit); lexical exact-match ✓.
- **Distill:** ✓ jsonl→md shadow + generic noise filter; ✗ structured failure-records; ✗ per-turn
  boundary chunking; ✗ boilerplate-prose dedup.
- **Design/Arch:** ◐ steering + data-architecture being established now.
- **Docs:** ◐ SKILL.md exists (some stale claims); ✗ product-level docs.
</content>
