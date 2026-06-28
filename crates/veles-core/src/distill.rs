//! Generic "verbose JSON → readable text" distillation.
//!
//! veles stays format-blind: it knows nothing about chat transcripts, agent
//! sessions, or any specific schema. It only knows that some folders are full
//! of *verbose JSON* — line-delimited `.jsonl` records or `.json` documents
//! whose useful content is buried under ids, hashes, and nesting — and that
//! such files index poorly as-is. This module flattens any JSON value into
//! plain `key.path: value` lines so the real prose (prompts, messages, notes)
//! becomes searchable, while staying completely schema-agnostic.
//!
//! It is deliberately dumb: no knowledge of "role", "content", "message", or
//! any product's layout. Every scalar leaf is emitted with its dotted key
//! path; obvious noise (very long opaque blobs) is truncated, not parsed.

use std::path::Path;

use serde_json::Value;

/// Strings longer than this are truncated — long prose is kept generously, but
/// base64/hex blobs don't get to dominate the derived text.
const MAX_VALUE_LEN: usize = 4000;
/// A string this long that is *also* opaque (single token, no spaces) is almost
/// certainly an encoded blob; emit a short marker instead of the bytes.
const OPAQUE_BLOB_LEN: usize = 200;

/// Minimum line-delimited-JSON files before a folder is treated as a corpus to
/// distill — keeps a stray `package.json` or a couple of fixtures from flipping
/// a code repo into distill mode.
const MIN_JSONL: usize = 10;
/// Sample cap. High enough to tally the *whole* tree of a typical corpus so the
/// histogram reflects the real mix, not a depth-first slice; a `stat`-only walk
/// of a few thousand files is sub-100ms, and giant code repos bail early anyway
/// (≈0 `.jsonl` → not distilled).
const SAMPLE_CAP: usize = 8192;

/// Does this folder look like a verbose-JSON corpus worth distilling rather
/// than indexing in place?
///
/// Trigger: there are at least [`MIN_JSONL`] line-delimited JSON files
/// (`.jsonl`/`.ndjson`) **and** at most one other extension is more common than
/// them. That survives the litter real transcript folders accumulate — e.g.
/// `~/.claude/projects` carries more `.md` notes and `.ck` cache files than
/// `.jsonl`, but `.jsonl` is still a top-2 extension, so it qualifies — while a
/// code repo (where `.jsonl` is a rare minority behind many `.rs`/`.ts`/…)
/// stays in place. Extensions are tallied across the whole tree up to
/// [`SAMPLE_CAP`] files, so depth-first walk order doesn't skew the decision.
pub fn looks_like_verbose_json(dir: &Path) -> bool {
    use std::collections::HashMap;
    let mut jsonl = 0usize;
    let mut other_ext: HashMap<String, usize> = HashMap::new();
    let mut sampled = 0usize;
    let walk = walkdir::WalkDir::new(dir)
        .follow_links(false)
        .max_depth(8)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file());
    for entry in walk {
        // Ignore veles' own state and the usual noise dirs.
        if entry
            .path()
            .components()
            .any(|c| matches!(c.as_os_str().to_str(), Some(".veles") | Some(".git")))
        {
            continue;
        }
        sampled += 1;
        match entry.path().extension().and_then(|e| e.to_str()) {
            Some("jsonl") | Some("ndjson") => jsonl += 1,
            Some(ext) => *other_ext.entry(ext.to_ascii_lowercase()).or_default() += 1,
            None => {}
        }
        if sampled >= SAMPLE_CAP {
            break;
        }
    }
    let more_common = other_ext.values().filter(|&&v| v > jsonl).count();
    jsonl >= MIN_JSONL && more_common <= 1
}

/// Whether `path` has an extension veles treats as verbose JSON.
pub fn is_json_ext(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("jsonl") | Some("ndjson") | Some("json")
    )
}

/// Distill one verbose-JSON file into readable markdown-ish text.
///
/// `.jsonl`/`.ndjson` are treated as one record per line; `.json` as a single
/// document. Lines that don't parse as JSON are passed through verbatim (so a
/// half-JSON log still contributes its plain text). Returns `None` only if the
/// file can't be read; an all-blank result still yields a short header so the
/// derived file exists and provenance is legible.
pub fn distill_file(path: &Path) -> Option<String> {
    let raw = std::fs::read_to_string(path).ok()?;
    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("record");
    let mut out = String::new();
    out.push_str("# ");
    out.push_str(name);
    out.push_str("\n\n");

    let jsonl = matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("jsonl") | Some("ndjson")
    );
    if jsonl {
        for (i, line) in raw.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            match serde_json::from_str::<Value>(line) {
                Ok(v) => {
                    out.push_str(&format!("## record {}\n", i + 1));
                    flatten(&v, "", &mut out);
                    out.push('\n');
                }
                // Not JSON — keep the raw line; never lose source text.
                Err(_) => {
                    out.push_str(line);
                    out.push('\n');
                }
            }
        }
    } else {
        match serde_json::from_str::<Value>(&raw) {
            Ok(v) => flatten(&v, "", &mut out),
            Err(_) => out.push_str(&raw),
        }
    }
    Some(out)
}

/// Largest prefix of `s` no longer than `max` bytes that ends on a UTF-8 char
/// boundary, so slicing multibyte text (emoji, CJK) never panics.
fn truncate_on_char_boundary(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Recursively emit `prefix.key: scalar` lines for every leaf of `v`.
fn flatten(v: &Value, prefix: &str, out: &mut String) {
    match v {
        Value::Object(map) => {
            for (k, val) in map {
                let next = if prefix.is_empty() {
                    k.clone()
                } else {
                    format!("{prefix}.{k}")
                };
                flatten(val, &next, out);
            }
        }
        Value::Array(items) => {
            for (i, val) in items.iter().enumerate() {
                let next = format!("{prefix}[{i}]");
                flatten(val, &next, out);
            }
        }
        Value::String(s) => emit_scalar(prefix, s, out),
        // Standalone numbers and booleans are almost always machinery in these
        // corpora (token counts, durations, exit codes, flags) — they bury the
        // prose and never make a useful search target on their own. Drop them.
        // Numbers that matter (versions, error codes, "R-22") live inside string
        // values, which are kept. This is a value-shape decision, not a
        // key-name one, so the distiller stays schema-blind.
        Value::Number(_) | Value::Bool(_) => {}
        Value::Null => {}
    }
}

/// Is `value` a single machine token — an id, hash, epoch/ISO timestamp, or bare
/// number — rather than human text? Used to drop the high-volume, zero-signal
/// leaves that otherwise dominate a distilled transcript. Deliberately
/// conservative: it only fires on **whitespace-free** values, so any multi-word
/// prose, path, command, or code snippet is always kept. No schema knowledge,
/// no key names — purely the shape of the value.
fn is_machine_token(s: &str) -> bool {
    if s.chars().any(char::is_whitespace) {
        return false;
    }
    // Bare number: integer, float, scientific, or epoch.
    if s.parse::<f64>().is_ok() {
        return true;
    }
    // UUID: 8-4-4-4-12 hex with hyphens at the canonical positions.
    if is_uuid(s) {
        return true;
    }
    // Hex digest (md5/sha1/sha256/blake3-ish): all hex at a hash-ish length.
    // Bounded at 128 so a long opaque blob still falls through to the blob
    // marker rather than being silently dropped.
    let n = s.len();
    if (32..=128).contains(&n) && s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return true;
    }
    // ISO-8601-ish timestamp: 2026-06-26T05:09:46(.138Z).
    is_iso_timestamp(s)
}

fn is_uuid(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() != 36 {
        return false;
    }
    for (i, &c) in b.iter().enumerate() {
        let ok = match i {
            8 | 13 | 18 | 23 => c == b'-',
            _ => c.is_ascii_hexdigit(),
        };
        if !ok {
            return false;
        }
    }
    true
}

fn is_iso_timestamp(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() >= 19
        && b[..4].iter().all(|c| c.is_ascii_digit())
        && b[4] == b'-'
        && s.contains('T')
        && s.contains(':')
}

fn emit_scalar(key: &str, value: &str, out: &mut String) {
    let value = value.trim();
    if value.is_empty() {
        return;
    }
    // Drop single machine tokens (ids, hashes, timestamps, bare numeric
    // strings) — high volume, no search value. Prose/paths/code are kept.
    if is_machine_token(value) {
        return;
    }
    // Opaque blob (long, no whitespace) → marker, not bytes.
    let rendered: std::borrow::Cow<str> =
        if value.len() > OPAQUE_BLOB_LEN && !value.contains(char::is_whitespace) {
            format!("<{}-char blob>", value.len()).into()
        } else if value.len() > MAX_VALUE_LEN {
            let head = truncate_on_char_boundary(value, MAX_VALUE_LEN);
            format!("{}… (+{} chars)", head, value.len() - head.len()).into()
        } else {
            value.into()
        };
    if key.is_empty() {
        out.push_str(&rendered);
    } else {
        out.push_str(key);
        out.push_str(": ");
        out.push_str(&rendered);
    }
    out.push('\n');
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn flattens_nested_record() {
        let v: Value = serde_json::json!({
            "type": "user",
            "message": {"role": "user", "content": "how does the pipeline work"},
            "uuid": "abc-123"
        });
        let mut s = String::new();
        flatten(&v, "", &mut s);
        assert!(s.contains("message.content: how does the pipeline work"));
        assert!(s.contains("message.role: user"));
        assert!(s.contains("type: user"));
    }

    #[test]
    fn opaque_blob_is_marked_not_dumped() {
        let blob = "A".repeat(500);
        let mut s = String::new();
        emit_scalar("data", &blob, &mut s);
        assert!(s.contains("<500-char blob>"), "got: {s}");
        assert!(!s.contains(&blob));
    }

    #[test]
    fn long_multibyte_value_truncates_without_panicking() {
        // A long prose value whose char at the cut point is multibyte (⭐ is 3
        // bytes). Must not panic on a non-char-boundary slice.
        let mut v = "word ".repeat(900); // 4500 bytes, has spaces
        v.push('⭐'); // push a multibyte char that may straddle MAX_VALUE_LEN
        let mut s = String::new();
        emit_scalar("content", &v, &mut s);
        assert!(s.contains("content: word"));
        assert!(
            s.contains("chars)"),
            "expected truncation marker: {}",
            &s[..60]
        );
    }

    #[test]
    fn truncate_lands_on_char_boundary() {
        let s = format!("{}⭐tail", "a".repeat(3999)); // ⭐ starts at byte 3999
        let head = truncate_on_char_boundary(&s, 4000);
        assert!(s.is_char_boundary(head.len()));
        assert_eq!(head.len(), 3999); // backed off before the emoji
    }

    #[test]
    fn prose_with_spaces_is_kept() {
        let long = "word ".repeat(60); // 300 chars but has spaces
        let mut s = String::new();
        emit_scalar("content", &long, &mut s);
        assert!(s.contains("content: word word"));
    }

    #[test]
    fn distill_jsonl_emits_per_record() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("sess.jsonl");
        fs::write(
            &f,
            "{\"message\":{\"content\":\"orbital decay notes\"}}\n{\"message\":{\"content\":\"second turn\"}}\n",
        )
        .unwrap();
        let text = distill_file(&f).unwrap();
        assert!(text.contains("# sess.jsonl"));
        assert!(text.contains("record 1"));
        assert!(text.contains("orbital decay notes"));
        assert!(text.contains("second turn"));
    }

    #[test]
    fn non_json_line_passes_through() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("mixed.jsonl");
        fs::write(&f, "{\"a\":\"alpha\"}\nplain log line\n").unwrap();
        let text = distill_file(&f).unwrap();
        assert!(text.contains("a: alpha"));
        assert!(text.contains("plain log line"));
    }

    #[test]
    fn machine_tokens_are_dropped_prose_is_kept() {
        // ids, hashes, timestamps, bare numbers, bools → gone; prose stays.
        let v: serde_json::Value = serde_json::json!({
            "uuid": "d0d03729-84a2-40c0-a4b7-5e1904e30f59",
            "messageId": "ca454a5f-5a49-4c06-a96c-3f0b1693e548",
            "output_tokens": 743,
            "is_error": false,
            "timestamp": "2026-06-26T05:09:46.138Z",
            "sha": "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            "service_tier": "standard",
            "version": "0.6.0",
            "path": "/home/u/app/bin/mpsync.py",
            "content": "things keep breaking and I am exhausted"
        });
        let mut s = String::new();
        flatten(&v, "", &mut s);
        // dropped machinery
        assert!(!s.contains("d0d03729"), "uuid leaked: {s}");
        assert!(!s.contains("ca454a5f"), "messageId leaked: {s}");
        assert!(!s.contains("743"), "number leaked: {s}");
        assert!(!s.contains("is_error"), "bool leaked: {s}");
        assert!(!s.contains("2026-06-26T05"), "timestamp leaked: {s}");
        assert!(!s.contains("e3b0c442"), "hash leaked: {s}");
        // kept signal
        assert!(s.contains("content: things keep breaking and I am exhausted"));
        assert!(s.contains("path: /home/u/app/bin/mpsync.py"), "path dropped: {s}");
        assert!(s.contains("version: 0.6.0"), "version dropped: {s}");
        // short enum words are kept (conservative — value-shape, not key-name)
        assert!(s.contains("service_tier: standard"));
    }

    #[test]
    fn machine_token_classifier_edges() {
        assert!(is_machine_token("743"));
        assert!(is_machine_token("-1.5e3"));
        assert!(is_machine_token("d0d03729-84a2-40c0-a4b7-5e1904e30f59"));
        assert!(is_machine_token(
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        ));
        assert!(is_machine_token("2026-06-26T05:09:46.138Z"));
        // kept: prose, version, path, short word, code-ish
        assert!(!is_machine_token("hello world"));
        assert!(!is_machine_token("0.6.0"));
        assert!(!is_machine_token("/home/u/x.rs"));
        assert!(!is_machine_token("standard"));
        assert!(!is_machine_token("deadbeef")); // 8-char hex, below hash length
    }

    #[test]
    fn detects_jsonl_folder_and_ignores_plain_code() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..10 {
            fs::write(dir.path().join(format!("s{i}.jsonl")), "{}\n").unwrap();
        }
        assert!(looks_like_verbose_json(dir.path()));

        let code = tempfile::tempdir().unwrap();
        for i in 0..5 {
            fs::write(code.path().join(format!("m{i}.rs")), "fn main() {}").unwrap();
        }
        assert!(!looks_like_verbose_json(code.path()));
    }
}
