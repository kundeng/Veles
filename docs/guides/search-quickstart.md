# Veles search quick-start: pick the right lane

Veles has **three search lanes**. Most "it didn't find what I wanted" problems
are using the wrong lane. Pick by what you're looking for, not by habit.

| You want… | Lane | Command |
|---|---|---|
| An **exact string** / known token / config key / **marker mining** (curses, error codes) | **regex** (lexical, grep parity) | `veles search 'pat' <path> --mode regex` |
| **Code by concept** ("where is auth handled") | **hybrid** (BM25 + static) — default | `veles search "query" <path>` |
| **Prose/sessions by meaning** ("the bug where approve did nothing") | **rerank** (transformer, delegated) | `veles search "query" <path> --rerank` |

## 1. Exact / literal — `--mode regex` (replaces reaching for grep)

Case-insensitive substring/regex over raw text, **identical recall to `grep -iE`**
(verified 72/72 on a marker lexicon) but indexed and **ranked by match count**
(densest hits first). BM25 matches whole tokens, so `fuck` misses `fucking`;
regex doesn't.

```bash
# find every chunk containing an exact error string
veles search 'ECONNREFUSED' . --mode regex -t 50
# mine frustration/marker turns from a session corpus, angriest first
veles search 'fuck|shit|wtf|dude|why are you|you keep' <corpus> --mode regex -t 100
```

This is the **lexical** lane — semantics is not involved. (Aliases: `--mode grep`.)

## 2. Code by concept — default hybrid

```bash
veles search "where is the index persisted" .
veles defs VelesIndex            # exact symbol nav (tree-sitter)
veles refs search_with_rerank .
```

## 3. Prose / sessions by meaning — `--rerank` (delegated)

Static embeddings are blunt on prose. `--rerank` pulls the BM25/static top-K,
then re-scores it with a **real transformer** by POSTing to a local
OpenAI-compatible `/v1/embeddings` server. Veles bundles **no model** — the
GPU/runtime lives in the server (LM Studio, ollama, TEI…), so the binary stays
lean. With no flags it **auto-detects** LM Studio (`:1234`) then ollama
(`:11434`) and picks an embedding model the server advertises.

```bash
# start a server once (example: ollama, userspace)
ollama serve &                    # GPU auto-detected; models idle-unload after 5 min
ollama pull nomic-embed-text
# then just:
veles search "the run where approval said done but nothing happened" <corpus> --rerank
# or point it explicitly:
veles search "…" <corpus> --rerank --rerank-url http://localhost:11434/v1/embeddings --rerank-model nomic-embed-text
```

**Honest limit:** rerank nails *content-bearing* queries ("frustrated, exhausted,
failed builds"). **Pure-sentiment paraphrase** ("overwhelmed and tired") still
misses — embeddings match topic/words, not affect. For affect, use lane 1
(`--mode regex` on a marker lexicon, which ranks by marker density).

## Searching your own Claude sessions

A session is ~80% agent output / ~20% you, interwoven, so full-session chunks
bury your words. Build a **per-user-turn** corpus first:

```bash
python3 pipelines/session_memory.py ~/.claude/projects ~/.local/state/veles/corpora/session-memory
veles index ~/.local/state/veles/corpora/session-memory --include-text-files
# then search it with any lane above
```

## Building the fork

```bash
cargo build --release -p veles-cli --features dashboard   # rerank is always-on, no feature flag
cp target/release/veles ~/.cargo/bin/veles
```
