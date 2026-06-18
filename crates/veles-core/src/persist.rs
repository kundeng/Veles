//! Persistent on-disk index format.
//!
//! Layout under `<repo>/.veles/`:
//!
//! ```text
//! .veles/
//!   CURRENT         - atomically-published generation identifier
//!   generations/
//!     <id>/
//!       manifest.json
//!       chunks.bin
//!       bm25.bin
//!       dense.bin
//!       symbols.bin
//! ```
//!
//! The manifest records a (size, mtime, chunk_count) fingerprint per file so
//! `update` can detect added / removed / modified files without re-reading
//! everything.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::index::dense::DenseIndex;
use crate::index::sparse::Bm25Index;
use crate::symbols::Symbol;
use crate::types::Chunk;
use crate::walker;

/// Directory name used under the indexed repo to store the on-disk index.
pub const INDEX_DIR_NAME: &str = ".veles";

/// Bumped whenever the on-disk format changes incompatibly. Bumped to 2
/// when symbols.bin was added — older indexes lack tree-sitter symbols.
pub const FORMAT_VERSION: u32 = 2;

const MANIFEST_FILE: &str = "manifest.json";
const CHUNKS_FILE: &str = "chunks.bin";
const BM25_FILE: &str = "bm25.bin";
const DENSE_FILE: &str = "dense.bin";
const SYMBOLS_FILE: &str = "symbols.bin";
const CURRENT_FILE: &str = "CURRENT";
const GENERATIONS_DIR: &str = "generations";

/// Cheap fingerprint for change detection.
///
/// `(size, mtime)` is fast to compute and covers almost all real edits;
/// `content_hash` (BLAKE3 of the file bytes) is the fallback used by
/// incremental update when mtime drifts but the bytes haven't changed
/// (`touch`, `git checkout` of an identical version, no-op formatter
/// runs).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileFingerprint {
    /// File size in bytes.
    pub size: u64,
    /// Modification time as Unix epoch seconds.
    pub mtime_secs: i64,
    /// Number of chunks this file produced.
    pub chunk_count: usize,
    /// BLAKE3 hex digest of the file bytes. `None` for fingerprints
    /// loaded from a pre-content-hash manifest; new fingerprints
    /// always populate it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_hash: Option<String>,
}

impl FileFingerprint {
    /// Compute the fingerprint for a path on disk. `chunk_count` is provided
    /// by the caller after chunking.
    pub fn from_path(path: &Path, chunk_count: usize) -> Result<Self> {
        let meta = fs::metadata(path).with_context(|| format!("stat {}", path.display()))?;
        let mtime = meta.modified().unwrap_or(UNIX_EPOCH);
        let mtime_secs = mtime
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let content_hash = Some(content_hash(path)?);
        Ok(Self {
            size: meta.len(),
            mtime_secs,
            chunk_count,
            content_hash,
        })
    }
}

/// BLAKE3 hex digest of `path`'s bytes. Used both at index-build time
/// (to populate the manifest) and at update time (to verify whether a
/// touched file's content actually changed).
pub fn content_hash(path: &Path) -> Result<String> {
    let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    Ok(blake3::hash(&bytes).to_hex().to_string())
}

/// Small JSON sidecar describing a persisted index.
///
/// Human-readable on purpose so users can `cat .veles/manifest.json` to
/// debug staleness or model mismatches.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    /// Version of `veles` that wrote this index (from `CARGO_PKG_VERSION`).
    pub veles_version: String,
    /// On-disk format version. Bumped on incompatible layout changes; a
    /// mismatch on `load` forces a `veles index --force`.
    pub format_version: u32,
    /// Embedding model used at build time (e.g. `"minishlab/potion-code-16M"`).
    /// Loading with a different model is rejected.
    pub model_name: String,
    /// Dimensionality of the dense vectors.
    pub embedding_dim: usize,
    /// Whether text/document files (markdown, yaml, ...) were indexed
    /// alongside source code.
    pub include_text_files: bool,
    /// Unix epoch seconds when the index was last written.
    pub indexed_at: i64,
    /// Per-file fingerprints used by incremental update.
    pub files: BTreeMap<String, FileFingerprint>,
    /// Total chunks across all files.
    pub total_chunks: usize,
}

impl Manifest {
    pub fn new(model_name: &str, embedding_dim: usize, include_text_files: bool) -> Self {
        Self {
            veles_version: env!("CARGO_PKG_VERSION").to_string(),
            format_version: FORMAT_VERSION,
            model_name: model_name.to_string(),
            embedding_dim,
            include_text_files,
            indexed_at: now_secs(),
            files: BTreeMap::new(),
            total_chunks: 0,
        }
    }

    pub fn touch(&mut self) {
        self.indexed_at = now_secs();
    }
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Path of the `.veles/` directory under a given repo root.
pub fn index_dir_for(repo_root: &Path) -> PathBuf {
    repo_root.join(INDEX_DIR_NAME)
}

/// Current committed generation identifier, or `None` for a legacy flat
/// index / a repository with no persisted index.
pub fn current_generation(repo_root: &Path) -> Option<u64> {
    let current = index_dir_for(repo_root).join(CURRENT_FILE);
    fs::read_to_string(current).ok()?.trim().parse().ok()
}

fn generation_dir(repo_root: &Path, generation: u64) -> PathBuf {
    index_dir_for(repo_root)
        .join(GENERATIONS_DIR)
        .join(generation.to_string())
}

fn active_index_dir(repo_root: &Path) -> PathBuf {
    current_generation(repo_root)
        .map(|generation| generation_dir(repo_root, generation))
        .unwrap_or_else(|| index_dir_for(repo_root))
}

/// Returns true if a saved index appears to exist at the given path.
pub fn index_exists(repo_root: &Path) -> bool {
    let dir = active_index_dir(repo_root);
    dir.join(MANIFEST_FILE).is_file()
        && dir.join(CHUNKS_FILE).is_file()
        && dir.join(BM25_FILE).is_file()
        && dir.join(DENSE_FILE).is_file()
}

/// Components of a loaded index — the model is provided separately at load
/// time so the heavy weights aren't serialised.
pub struct PersistedIndex {
    pub manifest: Manifest,
    pub chunks: Vec<Chunk>,
    pub bm25: Bm25Index,
    pub dense: DenseIndex,
    pub symbols: Vec<Symbol>,
}

/// Write a complete immutable generation, then atomically publish it through
/// `<repo_root>/.veles/CURRENT`. Readers resolve one generation and therefore
/// never combine files from concurrent or interrupted saves.
///
/// Also drops the previous `chunks.to_vec()` / `symbols.to_vec()`
/// temporaries: slices implement `Serialize`, so we feed them in
/// directly and skip the per-save full copy.
pub fn save(
    repo_root: &Path,
    manifest: &Manifest,
    chunks: &[Chunk],
    bm25: &Bm25Index,
    dense: &DenseIndex,
    symbols: &[Symbol],
) -> Result<()> {
    let index_dir = index_dir_for(repo_root);
    let generations_dir = index_dir.join(GENERATIONS_DIR);
    fs::create_dir_all(&generations_dir)
        .with_context(|| format!("create generations dir {}", generations_dir.display()))?;

    let generation = next_generation(repo_root);
    let dir = generation_dir(repo_root, generation);
    fs::create_dir_all(&dir).with_context(|| format!("create index dir {}", dir.display()))?;

    let chunks_path = dir.join(CHUNKS_FILE);
    let bm25_path = dir.join(BM25_FILE);
    let dense_path = dir.join(DENSE_FILE);
    let symbols_path = dir.join(SYMBOLS_FILE);

    let ((r1, r2), (r3, r4)) = rayon::join(
        || {
            rayon::join(
                || write_bincode(&chunks_path, &chunks),
                || write_bincode(&bm25_path, bm25),
            )
        },
        || {
            rayon::join(
                || write_bincode(&dense_path, dense),
                || write_bincode(&symbols_path, &symbols),
            )
        },
    );
    r1?;
    r2?;
    r3?;
    r4?;
    write_json(&dir.join(MANIFEST_FILE), manifest)?;
    sync_dir(&dir);

    write_atomic_bytes(
        &index_dir.join(CURRENT_FILE),
        format!("{generation}\n").as_bytes(),
    )?;
    sync_dir(&index_dir);
    cleanup_stale_generations(repo_root, generation);
    Ok(())
}

/// Load all index artefacts from `<repo_root>/.veles/`.
pub fn load(repo_root: &Path) -> Result<PersistedIndex> {
    let dir = active_index_dir(repo_root);
    if !dir.is_dir() {
        bail!("No index found at {}", dir.display());
    }

    let manifest: Manifest = read_json(&dir.join(MANIFEST_FILE))?;
    if manifest.format_version != FORMAT_VERSION {
        bail!(
            "Index format version {} is incompatible (expected {}). Run `veles index --force` to rebuild.",
            manifest.format_version,
            FORMAT_VERSION
        );
    }
    let chunks: Vec<Chunk> = read_bincode(&dir.join(CHUNKS_FILE))?;
    let bm25: Bm25Index = read_bincode(&dir.join(BM25_FILE))?;
    let dense: DenseIndex = read_bincode(&dir.join(DENSE_FILE))?;
    // Symbols file may be missing on a partially-written index; treat as empty.
    let symbols: Vec<Symbol> = if dir.join(SYMBOLS_FILE).is_file() {
        read_bincode(&dir.join(SYMBOLS_FILE))?
    } else {
        Vec::new()
    };

    Ok(PersistedIndex {
        manifest,
        chunks,
        bm25,
        dense,
        symbols,
    })
}

/// Read just the manifest (cheap — used by `status` and to check compatibility).
pub fn load_manifest(repo_root: &Path) -> Result<Manifest> {
    let dir = active_index_dir(repo_root);
    read_json(&dir.join(MANIFEST_FILE))
}

/// Remove the on-disk index directory if it exists.
pub fn clean(repo_root: &Path) -> Result<bool> {
    let dir = index_dir_for(repo_root);
    if dir.is_dir() {
        fs::remove_dir_all(&dir).with_context(|| format!("remove {}", dir.display()))?;
        return Ok(true);
    }
    Ok(false)
}

/// Write `bytes` to `path` atomically: stream to a sibling `*.tmp`, fsync it,
/// then `rename` over the destination. `rename(2)` within a directory is
/// atomic on every local filesystem we target, so a reader (or a crash) ever
/// sees only the complete old file or the complete new one — never a torn
/// half-write. The fsync before the rename guards the file's *contents*
/// against power loss; we then best-effort fsync the directory so the rename
/// itself is durable. The per-dest writer lock guarantees no two writers race
/// on the same `*.tmp` name.
fn write_atomic(
    path: &Path,
    write_body: impl FnOnce(&mut std::io::BufWriter<fs::File>) -> Result<()>,
) -> Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let tmp = path.with_extension(format!(
        "{}tmp",
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| format!("{e}."))
            .unwrap_or_default()
    ));
    {
        let f = fs::File::create(&tmp).with_context(|| format!("create {}", tmp.display()))?;
        let mut w = std::io::BufWriter::new(f);
        write_body(&mut w)?;
        let f = w
            .into_inner()
            .with_context(|| format!("flush {}", tmp.display()))?;
        f.sync_all()
            .with_context(|| format!("fsync {}", tmp.display()))?;
    }
    fs::rename(&tmp, path)
        .with_context(|| format!("rename {} -> {}", tmp.display(), path.display()))?;
    // Best-effort: persist the rename itself. A failure here doesn't tear any
    // single file (rename already completed), so it's non-fatal.
    if let Ok(d) = fs::File::open(dir) {
        let _ = d.sync_all();
    }
    Ok(())
}

fn write_atomic_bytes(path: &Path, bytes: &[u8]) -> Result<()> {
    write_atomic(path, |w| {
        w.write_all(bytes)
            .with_context(|| format!("write {}", path.display()))
    })
}

fn next_generation(repo_root: &Path) -> u64 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(1);
    current_generation(repo_root)
        .map(|current| now.max(current.saturating_add(1)))
        .unwrap_or(now)
}

fn sync_dir(dir: &Path) {
    if let Ok(handle) = fs::File::open(dir) {
        let _ = handle.sync_all();
    }
}

fn cleanup_stale_generations(repo_root: &Path, current: u64) {
    const MIN_AGE: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);
    let root = index_dir_for(repo_root).join(GENERATIONS_DIR);
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let Ok(generation) = entry.file_name().to_string_lossy().parse::<u64>() else {
            continue;
        };
        if generation == current {
            continue;
        }
        let old_enough = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|modified| SystemTime::now().duration_since(modified).ok())
            .is_some_and(|age| age >= MIN_AGE);
        if old_enough {
            let _ = fs::remove_dir_all(entry.path());
        }
    }
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    write_atomic(path, |w| {
        serde_json::to_writer_pretty(w, value).with_context(|| format!("write {}", path.display()))
    })
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T> {
    let f = fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let value = serde_json::from_reader(std::io::BufReader::new(f))
        .with_context(|| format!("parse {}", path.display()))?;
    Ok(value)
}

fn write_bincode<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    write_atomic(path, |w| {
        bincode::serialize_into(w, value).with_context(|| format!("encode {}", path.display()))
    })
}

fn read_bincode<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T> {
    let f = fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let r = std::io::BufReader::new(f);
    let value =
        bincode::deserialize_from(r).with_context(|| format!("decode {}", path.display()))?;
    Ok(value)
}

/// Per-file disk metadata used during classification.
#[derive(Debug, Clone)]
pub struct DiskEntry {
    pub abs_path: PathBuf,
    pub size: u64,
    pub mtime_secs: i64,
}

/// Per-file change classification against a `Manifest`.
///
/// Distinguishes the four cases that incremental `update` cares about:
/// no content read (`Unchanged`), mtime drift but bytes still match
/// (`MtimeOnly`), bytes actually changed (`Modified`), or new since
/// last save (`Added`). Files removed since last save live in
/// `DiskState::removed`, not here.
#[derive(Debug, Clone)]
pub enum Classification {
    /// `(size, mtime)` matched the manifest exactly — no content read.
    Unchanged,
    /// `mtime` drifted but the BLAKE3 content hash still matches.
    /// Carries the hash we computed so callers don't re-read the file.
    MtimeOnly { hash: String },
    /// File was in the manifest but bytes have actually changed.
    /// `hash` is `Some` when we computed one during classification
    /// (only happens when size matched and the manifest had a hash),
    /// otherwise `None`.
    Modified { hash: Option<String> },
    /// File is new — not in the manifest.
    Added,
}

/// Result of walking the repo and classifying each file against a manifest.
#[derive(Debug)]
pub struct DiskState {
    /// For each file currently on disk: its metadata.
    pub on_disk: HashMap<String, DiskEntry>,
    /// Per-file classification — keys mirror `on_disk`.
    pub classification: HashMap<String, Classification>,
    /// Paths that were in the manifest but are not on disk now.
    pub removed: Vec<String>,
}

impl DiskState {
    /// Files seen now (on disk).
    pub fn seen_now(&self) -> usize {
        self.on_disk.len()
    }
    /// Count of files in each classification bucket.
    pub fn count_added(&self) -> usize {
        self.classification
            .values()
            .filter(|c| matches!(c, Classification::Added))
            .count()
    }
    pub fn count_modified(&self) -> usize {
        self.classification
            .values()
            .filter(|c| matches!(c, Classification::Modified { .. }))
            .count()
    }
    pub fn count_mtime_only(&self) -> usize {
        self.classification
            .values()
            .filter(|c| matches!(c, Classification::MtimeOnly { .. }))
            .count()
    }
    pub fn count_unchanged(&self) -> usize {
        self.classification
            .values()
            .filter(|c| matches!(c, Classification::Unchanged))
            .count()
    }
    pub fn count_removed(&self) -> usize {
        self.removed.len()
    }
    /// True iff nothing changed at all (no chunk edits, no mtime
    /// drift, no adds/removes). Matches `UpdateReport::is_noop` after
    /// `update_from_path` has consumed the state.
    pub fn is_clean(&self) -> bool {
        self.removed.is_empty()
            && self
                .classification
                .values()
                .all(|c| matches!(c, Classification::Unchanged))
    }
}

/// Walk `repo_root` filtered by `extensions` and classify each file
/// against `manifest`. Single place where the "mtime fast path then
/// BLAKE3 fallback" decision lives — both `VelesIndex::update_from_path`
/// and the MCP `status` handler call this (§3.3 of the perf plan).
pub fn classify_disk(
    repo_root: &Path,
    manifest: &Manifest,
    extensions: &HashSet<String>,
) -> DiskState {
    // 1. Walk on-disk files.
    let mut on_disk: HashMap<String, DiskEntry> = HashMap::new();
    for abs in walker::walk_files(repo_root, extensions) {
        let Ok(rel_path) = abs.strip_prefix(repo_root) else {
            continue;
        };
        let rel = rel_path.to_string_lossy().into_owned();
        let Ok(meta) = fs::metadata(&abs) else {
            continue;
        };
        let mtime_secs = meta
            .modified()
            .ok()
            .and_then(|m| m.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        on_disk.insert(
            rel,
            DiskEntry {
                abs_path: abs,
                size: meta.len(),
                mtime_secs,
            },
        );
    }

    // 2. Classify each on-disk file.
    let mut classification: HashMap<String, Classification> = HashMap::new();
    for (rel, entry) in &on_disk {
        let cls = match manifest.files.get(rel) {
            Some(prev) if prev.size == entry.size && prev.mtime_secs == entry.mtime_secs => {
                Classification::Unchanged
            }
            Some(prev) if prev.size == entry.size && prev.content_hash.is_some() => {
                match content_hash(&entry.abs_path) {
                    Ok(h) if Some(&h) == prev.content_hash.as_ref() => {
                        Classification::MtimeOnly { hash: h }
                    }
                    Ok(h) => Classification::Modified { hash: Some(h) },
                    Err(_) => Classification::Modified { hash: None },
                }
            }
            Some(_) => Classification::Modified { hash: None },
            None => Classification::Added,
        };
        classification.insert(rel.clone(), cls);
    }

    // 3. Files in the manifest that disappeared.
    let removed: Vec<String> = manifest
        .files
        .keys()
        .filter(|k| !on_disk.contains_key(*k))
        .cloned()
        .collect();

    DiskState {
        on_disk,
        classification,
        removed,
    }
}

/// Outcome of an incremental update — returned by
/// [`crate::VelesIndex::update_from_path`].
#[derive(Debug, Default, Clone)]
pub struct UpdateReport {
    /// Files seen on disk that weren't in the previous manifest.
    pub added_files: usize,
    /// Files whose `(size, mtime)` fingerprint changed and whose content
    /// (when checked via `content_hash`) actually differed.
    pub modified_files: usize,
    /// Files in the previous manifest no longer present on disk.
    pub removed_files: usize,
    /// Files whose `mtime` drifted but whose `content_hash` still matched —
    /// no re-embedding needed, but the manifest's fingerprint was refreshed
    /// so subsequent `status` / `update` calls skip the hash recompute.
    pub mtime_refreshed_files: usize,
    /// Chunks reused from the previous index without re-embedding.
    pub kept_chunks: usize,
    /// Chunks freshly embedded for added/modified files.
    pub new_chunks: usize,
    /// Total chunks in the updated index (`kept + new`).
    pub total_chunks: usize,
}

impl UpdateReport {
    /// True when nothing changed — no chunk-level edits and no fingerprint
    /// refreshes pending. Callers use this to skip persistence.
    pub fn is_noop(&self) -> bool {
        self.added_files == 0
            && self.modified_files == 0
            && self.removed_files == 0
            && self.mtime_refreshed_files == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_roundtrip_via_json() {
        let mut m = Manifest::new("test-model", 64, false);
        m.files.insert(
            "src/lib.rs".to_string(),
            FileFingerprint {
                size: 100,
                mtime_secs: 1_000_000,
                chunk_count: 2,
                content_hash: Some("deadbeef".to_string()),
            },
        );
        m.total_chunks = 2;

        let s = serde_json::to_string(&m).unwrap();
        let m2: Manifest = serde_json::from_str(&s).unwrap();
        assert_eq!(m2.model_name, "test-model");
        assert_eq!(m2.embedding_dim, 64);
        assert_eq!(m2.files.len(), 1);
        assert_eq!(m2.files["src/lib.rs"].size, 100);
        assert_eq!(
            m2.files["src/lib.rs"].content_hash.as_deref(),
            Some("deadbeef")
        );
    }

    #[test]
    fn write_atomic_replaces_cleanly_and_leaves_no_tmp() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("chunks.bin");

        // First write.
        write_atomic(&p, |w| {
            std::io::Write::write_all(w, b"first").map_err(Into::into)
        })
        .unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"first");

        // Overwrite — must fully replace, never a torn mix.
        write_atomic(&p, |w| {
            std::io::Write::write_all(w, b"second-and-longer").map_err(Into::into)
        })
        .unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"second-and-longer");

        // No stray temp files left behind in the dir.
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains("tmp"))
            .collect();
        assert!(leftovers.is_empty(), "stray tmp files: {leftovers:?}");
    }

    #[test]
    fn stale_tmp_sibling_does_not_break_load() {
        // A crash could leave a *.tmp behind. load() reads only the canonical
        // filenames, so a leftover temp must be inert.
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("manifest.json");
        write_atomic(&p, |w| {
            std::io::Write::write_all(w, b"{}").map_err(Into::into)
        })
        .unwrap();
        std::fs::write(dir.path().join("chunks.bin.tmp"), b"garbage-half-write").unwrap();
        // The canonical file is intact and readable.
        assert_eq!(std::fs::read(&p).unwrap(), b"{}");
    }

    #[test]
    fn legacy_manifest_without_content_hash_loads() {
        // Pre-content-hash manifests omit the field entirely. Serde
        // must default it to None, not bail.
        let json = r#"{
            "veles_version": "0.2.3",
            "format_version": 2,
            "model_name": "test-model",
            "embedding_dim": 64,
            "include_text_files": false,
            "indexed_at": 0,
            "files": {
                "src/lib.rs": {
                    "size": 100,
                    "mtime_secs": 1000000,
                    "chunk_count": 2
                }
            },
            "total_chunks": 2
        }"#;
        let m: Manifest = serde_json::from_str(json).unwrap();
        assert_eq!(m.files["src/lib.rs"].size, 100);
        assert!(m.files["src/lib.rs"].content_hash.is_none());
    }

    #[test]
    fn content_hash_is_deterministic_and_discriminates() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.txt");

        std::fs::write(&p, b"hello").unwrap();
        let h1 = content_hash(&p).unwrap();
        let h2 = content_hash(&p).unwrap();
        assert_eq!(h1, h2, "same bytes must hash the same");

        std::fs::write(&p, b"hello world").unwrap();
        let h3 = content_hash(&p).unwrap();
        assert_ne!(h1, h3, "different bytes must hash differently");
    }
}
