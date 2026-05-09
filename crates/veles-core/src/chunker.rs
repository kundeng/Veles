//! Source code chunker — splits files into indexable units.

use crate::types::Chunk;

/// Maximum number of lines per chunk when using line-based splitting.
const MAX_LINES: usize = 50;
/// Number of overlapping lines between consecutive chunks.
const OVERLAP_LINES: usize = 5;

/// Split pre-read source text into chunks.
///
/// If `language` is `Some`, returns line-based chunks (tree-sitter support
/// can be added later). Falls back to line-based for `None`.
pub fn chunk_source(source: &str, file_path: &str, language: Option<&str>) -> Vec<Chunk> {
    if source.trim().is_empty() {
        return Vec::new();
    }
    // Currently we use line-based chunking for all languages.
    // TODO: Add tree-sitter-aware chunking for better code awareness.
    let _ = language;
    chunk_lines_default(source, file_path, language)
}

/// Split source by line count with overlap.
pub fn chunk_lines(
    source: &str,
    file_path: &str,
    language: Option<&str>,
    max_lines: usize,
    overlap_lines: usize,
) -> Vec<Chunk> {
    let lines: Vec<&str> = source.lines().collect();
    if lines.is_empty() {
        return Vec::new();
    }

    let mut chunks = Vec::new();
    let mut start = 0;

    while start < lines.len() {
        let end = std::cmp::min(start + max_lines, lines.len());
        let content = lines[start..end].join("\n");

        if !content.trim().is_empty() {
            chunks.push(Chunk {
                content,
                file_path: file_path.to_string(),
                start_line: start + 1, // 1-indexed
                end_line: end,         // inclusive
                language: language.map(|l| l.to_string()),
            });
        }

        if end < lines.len() {
            start = end - overlap_lines;
        } else {
            break;
        }
    }

    chunks
}

/// Convenience: chunk with default parameters.
pub fn chunk_lines_default(source: &str, file_path: &str, language: Option<&str>) -> Vec<Chunk> {
    chunk_lines(source, file_path, language, MAX_LINES, OVERLAP_LINES)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty_source() {
        let chunks = chunk_source("", "test.py", Some("python"));
        assert!(chunks.is_empty());
    }

    #[test]
    fn test_whitespace_only() {
        let chunks = chunk_source("   \n  \n  ", "test.py", Some("python"));
        assert!(chunks.is_empty());
    }

    #[test]
    fn test_short_file() {
        let source = "fn main() {\n    println!(\"hello\");\n}\n";
        let chunks = chunk_source(source, "main.rs", Some("rust"));
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].start_line, 1);
        assert_eq!(chunks[0].end_line, 3);
    }

    #[test]
    fn test_long_file_splits() {
        let source: String = (0..120).map(|i| format!("line {i}\n")).collect();
        let chunks = chunk_lines_default(&source, "big.rs", Some("rust"));
        assert!(chunks.len() > 1);
        // Chunks should have overlap
        assert!(chunks[0].end_line > chunks[1].start_line);
    }
}
