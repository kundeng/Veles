#!/usr/bin/env python3
"""Build a *session-memory* corpus: one record per meaningful USER turn.

Why this exists (proven empirically): a Claude session is ~80% agent output and
~20% your words, interwoven. When the whole session is chunked into fixed-size
blocks, your one high-signal line ("…putting out fire constantly. I'm exhausted")
is averaged into agent prose and becomes unfindable — no reranker recovers a
1/5-of-a-chunk signal. Cut the retrieval unit down to a single user turn and the
sentiment line *is* the record: a query like "firefighting / burned out" then
returns the right session at #1, even on the cheap static embedding.

So this is a different distill from `session_distill.py` (which keeps the full
dialogue for *content* search). This one keeps ONLY your turns, one file each,
for *memory* search — "find the session where I was frustrated about X".

Usage:
    python3 session_memory.py <claude-projects-dir> <out-corpus-dir>
    veles index <out-corpus-dir> --include-text-files
    veles search "when was I frustrated about bad builds" <out-corpus-dir>

Affect-aware ranking (boosting frustration/yearning/curse turns over neutral
ones) is a deliberate follow-up — see the spec. This ships the granularity win.
"""
import json
import re
import sys
from pathlib import Path

MIN_LEN = 40             # drop trivial acks ("ok", "go", "yes, do it")
MAX_LEN = 2000           # bound a single pasted-wall-of-text turn
SCAFFOLD = re.compile(
    r"<system-reminder>.*?</system-reminder>"
    r"|<local-command-[^>]*>.*?</local-command-[^>]*>"
    r"|<command-(name|message|args|stdout|stderr)>.*?</command-\1>"
    r"|<[^>]+>",
    re.S,
)


def clean(s: str) -> str:
    return SCAFFOLD.sub("", s or "").strip()


def user_texts(msg: dict):
    """Yield the human-typed text of a user message, skipping tool_result/meta."""
    content = msg.get("content")
    if isinstance(content, str):
        yield content
        return
    for b in content or []:
        if isinstance(b, dict) and b.get("type") == "text":
            yield b.get("text") or ""


def build(session: Path, out: Path) -> int:
    sid = session.stem[:8]
    project = ""
    n = 0
    for line in session.read_text(errors="ignore").splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            r = json.loads(line)
        except json.JSONDecodeError:
            continue
        project = project or Path(r.get("cwd", "")).name
        if r.get("type") != "user":
            continue
        msg = r.get("message")
        if not isinstance(msg, dict) or msg.get("role") != "user":
            continue
        ts = r.get("timestamp", "")
        for raw in user_texts(msg):
            body = clean(raw)
            if len(body) < MIN_LEN:        # skip "ok go" acknowledgements
                continue
            if len(body) > MAX_LEN:
                body = body[:MAX_LEN].rstrip() + f"… (+{len(body)-MAX_LEN} chars)"
            n += 1
            head = f"# {project or '?'} · {ts}\n(session {sid})\n\n"
            (out / f"{sid}_{n:03d}.md").write_text(head + body + "\n")
    return n


def main():
    if len(sys.argv) != 3:
        sys.exit("usage: session_memory.py <claude-projects-dir> <out-corpus-dir>")
    src, out = Path(sys.argv[1]).expanduser(), Path(sys.argv[2]).expanduser()
    out.mkdir(parents=True, exist_ok=True)
    sessions = sorted(src.rglob("*.jsonl"))
    turns = sum(build(s, out) for s in sessions)
    print(f"{turns} user-turn records from {len(sessions)} sessions -> {out}")


if __name__ == "__main__":
    main()
