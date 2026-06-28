#!/usr/bin/env python3
"""Distill a Claude Code .jsonl session into clean, high-signal Markdown records.

A veles transform extension (D7): veles stays format-blind and runs
`python3 session_distill.py <abs-session.jsonl>`, indexing this script's stdout.
All domain knowledge ("what a session is") lives here, not in veles.

Keeps the conversation + failure signals, drops the machinery that buries them:
  KEEP  user prose · assistant text · thinking (brief) · tool_result ERRORS
  DROP  file-history-snapshot · hook attachments · last-prompt · ai-title ·
        queue-operation · system · tool_use params · successful tool dumps ·
        repeated system-reminders · ids / usage / telemetry
Large blocks are truncated; consecutive duplicates are collapsed.

Output is one record per meaningful turn under a metadata header, so a reader
(or an embedder) sees dialogue and errors, not JSON scaffolding.
"""
import json
import re
import sys
from pathlib import Path

MAX_BLOCK = 1500          # truncate any single block to this many chars
MAX_ERR = 800             # tool errors are signal but still bounded
# Strip CLI/agent scaffolding wrappers that carry no conversational signal.
SCAFFOLD = re.compile(
    r"<system-reminder>.*?</system-reminder>"
    r"|<local-command-[^>]*>.*?</local-command-[^>]*>"
    r"|<command-(name|message|args|stdout|stderr)>.*?</command-\1>"
    r"|<[^>]+>",                       # any other stray xml-ish tag
    re.S,
)


def clip(s: str, n: int) -> str:
    s = SCAFFOLD.sub("", s or "").strip()
    return s if len(s) <= n else s[:n].rstrip() + f"… (+{len(s)-n} chars)"


def text_blocks(msg: dict):
    """Yield (kind, text) for the human-meaningful content blocks of a message."""
    content = msg.get("content")
    if isinstance(content, str):
        yield "text", content
        return
    for b in content or []:
        if not isinstance(b, dict):
            continue
        t = b.get("type")
        if t in ("text", "thinking"):
            yield t, b.get("text") or b.get("thinking") or ""
        elif t == "tool_result" and b.get("is_error"):
            inner = b.get("content")
            if isinstance(inner, list):
                inner = " ".join(x.get("text", "") for x in inner if isinstance(x, dict))
            yield "error", inner or ""


def distill(path: Path) -> str:
    session_id = project = ""
    first_ts = last_ts = ""
    records, errors = [], 0
    last_emitted = None

    for line in path.read_text(errors="ignore").splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            r = json.loads(line)
        except json.JSONDecodeError:
            continue
        session_id = session_id or r.get("sessionId", "")
        project = project or r.get("cwd", "")
        ts = r.get("timestamp", "")
        if ts:
            first_ts = first_ts or ts
            last_ts = ts
        if r.get("type") not in ("user", "assistant"):
            continue
        msg = r.get("message")
        if not isinstance(msg, dict):
            continue
        role = msg.get("role", r.get("type"))
        for kind, raw in text_blocks(msg):
            body = clip(raw, MAX_ERR if kind == "error" else MAX_BLOCK)
            if not body or body == last_emitted:   # drop empties + consecutive dupes
                continue
            last_emitted = body
            if kind == "error":
                errors += 1
                records.append(f"### ⚠ tool error\n{body}")
            elif kind == "thinking":
                records.append(f"### {role} (thinking)\n{body}")
            else:
                records.append(f"### {role}\n{body}")

    head = [
        f"# session {session_id or path.stem}",
        f"project: {Path(project).name if project else '?'}",
        f"span: {first_ts} → {last_ts}",
        f"turns: {len(records)} · tool-errors: {errors}",
        "",
    ]
    return "\n".join(head) + "\n\n".join(records) + "\n"


def main():
    if len(sys.argv) != 2:
        sys.exit("usage: session_distill.py <session.jsonl>")
    sys.stdout.write(distill(Path(sys.argv[1])))


if __name__ == "__main__":
    main()
