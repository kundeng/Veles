//! Declarative ingest pipeline: watch a *source*, optionally *transform* it
//! into text, and *index* the result — all without veles knowing anything
//! about what the source actually is.
//!
//! veles is a code/text search engine; it must stay blind to domain formats
//! (chat transcripts, logs, exports). The format knowledge lives in an
//! **external transform command**: for each changed source file veles runs
//! `<transform> <abs-source-path>` and writes the command's stdout to a
//! derived `.md` mirrored under the destination, then indexes the
//! destination. Sessions, PDFs, logs — all become "a glob + an adapter
//! script + a dest" in a JSON config; none of them are veles features.
//!
//! A stage's destination is guarded by the per-dest [`crate::lock`] writer
//! lock, so two pipeline runs (a one-shot `transform` and the `watch` daemon,
//! or two daemons) can never race the same dest. One stage may have many
//! inputs (e.g. Claude + Codex transcript trees) feeding one dest under one
//! lock.
//!
//! Config (`veles.pipeline.json`):
//! ```json
//! {
//!   "stages": [{
//!     "name": "agent-sessions",
//!     "dest": "~/.veles-corpora/sessions",
//!     "inputs": [
//!       {"name": "claude", "source": "~/.claude/projects/**/*.jsonl", "transform": ["python3","claude_distill.py"]},
//!       {"name": "codex", "source": "~/.codex/sessions/**/rollout-*.jsonl", "transform": ["python3","codex_distill.py"]}
//!     ]
//!   }]
//! }
//! ```

use std::collections::BTreeMap;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use model2vec_rs::model::StaticModel;

use crate::VelesIndex;
use crate::lock::{self, LockOutcome};
use crate::persist::{self, index_dir_for};

/// A whole pipeline: a list of independent stages.
#[derive(Debug, Clone, Deserialize)]
pub struct PipelineConfig {
    pub stages: Vec<Stage>,
}

impl PipelineConfig {
    /// Resolve relative source/destination paths against the config directory.
    /// Transform arguments that name an existing relative file are resolved
    /// too, so `python3 scripts/adapter.py` is stable across launch directories.
    pub fn resolve_relative_to(&mut self, base: &Path) {
        for stage in &mut self.stages {
            stage.dest = resolve_config_path(base, &stage.dest)
                .to_string_lossy()
                .into_owned();
            for input in &mut stage.inputs {
                input.source = resolve_config_path(base, &input.source)
                    .to_string_lossy()
                    .into_owned();
                for arg in &mut input.transform {
                    if arg.starts_with('-') {
                        continue;
                    }
                    let candidate = expand_tilde(arg);
                    if candidate.is_relative() && base.join(&candidate).exists() {
                        *arg = base.join(candidate).to_string_lossy().into_owned();
                    }
                }
            }
        }
    }
}

/// One destination index, fed by one or more inputs and guarded by one lock.
#[derive(Debug, Clone, Deserialize)]
pub struct Stage {
    /// Human label, recorded in the writer lock for diagnostics.
    pub name: String,
    /// Directory that holds the derived corpus + its `.veles/` index.
    pub dest: String,
    /// Index non-code text files (derived docs are `.md`/text, so this
    /// defaults to true for transform stages).
    #[serde(default = "default_true")]
    pub include_text_files: bool,
    /// Source → transform pairs. All write into `dest`.
    pub inputs: Vec<Input>,
}

/// One source glob and the external command that turns each matched file into
/// indexable text.
#[derive(Debug, Clone, Deserialize)]
pub struct Input {
    /// Stable namespace under `dest`. Prevents two source trees with the same
    /// relative paths from overwriting one another. Legacy configs may omit
    /// it and receive an ordinal `input-N` namespace.
    #[serde(default)]
    pub name: String,
    /// Glob of source files (supports `~` and `**`).
    pub source: String,
    /// External transform: `[program, args...]`. veles appends the absolute
    /// source path as the final argument and indexes the command's stdout.
    /// Required (a transform stage's whole point); a no-transform input is an
    /// error — index a plain repo with `veles index`/`watch` instead.
    pub transform: Vec<String>,
}

fn default_true() -> bool {
    true
}

/// Per-source incremental state, persisted under `<dest>/.veles/`.
#[derive(Debug, Default, Serialize, Deserialize)]
struct PipelineState {
    /// abs source path → fingerprint of the last successful derivation.
    sources: BTreeMap<String, SourceState>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SourceState {
    size: u64,
    mtime_secs: i64,
    /// Derived file path, relative to `dest`.
    derived_rel: String,
}

const STATE_FILE: &str = "pipeline-state.json";

/// Outcome of running one stage.
#[derive(Debug, Default)]
pub struct StageReport {
    pub stage: String,
    /// True if the writer lock was held by someone else — stage skipped.
    pub skipped_locked: Option<String>,
    pub sources_seen: usize,
    pub derived_written: usize,
    pub derived_removed: usize,
    pub transform_failures: usize,
    pub indexed_files: usize,
    pub total_chunks: usize,
}

/// Run every stage of a pipeline once. `model` is cloned per stage.
pub fn run_pipeline(
    cfg: &PipelineConfig,
    model: &StaticModel,
    now_epoch_secs: i64,
) -> Result<Vec<StageReport>> {
    let mut reports = Vec::with_capacity(cfg.stages.len());
    for stage in &cfg.stages {
        reports.push(run_stage(stage, model, now_epoch_secs)?);
    }
    Ok(reports)
}

/// Run one stage once: acquire the dest writer lock, (re)derive changed
/// sources via their transforms, drop derived files for vanished sources,
/// then incrementally (re)index the dest. Returns a [`StageReport`]; a held
/// lock yields `skipped_locked` rather than an error.
pub fn run_stage(stage: &Stage, model: &StaticModel, now_epoch_secs: i64) -> Result<StageReport> {
    let mut report = StageReport {
        stage: stage.name.clone(),
        ..Default::default()
    };
    if stage.inputs.is_empty() {
        bail!("stage {:?} has no inputs", stage.name);
    }
    let mut input_names = std::collections::HashSet::new();
    for (input_index, input) in stage.inputs.iter().enumerate() {
        if input.transform.is_empty() {
            bail!(
                "stage {:?} input {:?} has an empty transform; transform stages require a command",
                stage.name,
                input.source
            );
        }
        let name = effective_input_name(input, input_index);
        if !input_names.insert(name.clone()) {
            bail!("stage {:?} has duplicate input name {name:?}", stage.name);
        }
    }

    let dest = expand_tilde(&stage.dest);
    fs::create_dir_all(&dest).with_context(|| format!("create dest {}", dest.display()))?;

    // Single-writer: bail out cleanly if another writer owns this dest.
    let _guard = match lock::try_acquire(&dest, &stage.name, now_epoch_secs)? {
        LockOutcome::Acquired(g) => g,
        LockOutcome::Held { holder } => {
            report.skipped_locked = Some(holder);
            return Ok(report);
        }
    };

    let mut state = load_state(&dest);
    let mut alive: std::collections::HashSet<String> = std::collections::HashSet::new();

    // 1. Derive: each input's matched sources → derived `.md` under dest.
    for (input_index, input) in stage.inputs.iter().enumerate() {
        let input_name = effective_input_name(input, input_index);
        let pattern = expand_tilde(&input.source);
        let (base, matcher) = glob_base_and_matcher(&pattern)?;
        for src in enumerate_sources(&base, &matcher) {
            report.sources_seen += 1;
            let src_key = src.to_string_lossy().into_owned();
            alive.insert(src_key.clone());

            let meta = match fs::metadata(&src) {
                Ok(m) => m,
                Err(_) => continue,
            };
            let size = meta.len();
            let mtime_secs = mtime_secs(&meta);

            // Quick skip: unchanged size+mtime and the derived file still exists.
            if let Some(prev) = state.sources.get(&src_key)
                && prev.size == size
                && prev.mtime_secs == mtime_secs
                && dest.join(&prev.derived_rel).is_file()
            {
                continue;
            }

            let derived_rel = derived_rel_path(&input_name, &base, &src);
            let derived_abs = dest.join(&derived_rel);

            match run_transform(&input.transform, &src) {
                Ok(text) => {
                    if let Some(parent) = derived_abs.parent() {
                        fs::create_dir_all(parent).ok();
                    }
                    write_atomic_bytes(&derived_abs, text.as_bytes())
                        .with_context(|| format!("write derived {}", derived_abs.display()))?;
                    state.sources.insert(
                        src_key,
                        SourceState {
                            size,
                            mtime_secs,
                            derived_rel: derived_rel.to_string_lossy().into_owned(),
                        },
                    );
                    report.derived_written += 1;
                }
                Err(e) => {
                    report.transform_failures += 1;
                    eprintln!(
                        "veles pipeline: transform failed for {}: {e}",
                        src.display()
                    );
                    // Keep any prior derived output; don't lose it on a transient failure.
                }
            }
        }
    }

    // 2. Drop derived files whose source vanished.
    let gone: Vec<String> = state
        .sources
        .keys()
        .filter(|k| !alive.contains(*k))
        .cloned()
        .collect();
    for k in gone {
        if let Some(s) = state.sources.remove(&k) {
            let p = dest.join(&s.derived_rel);
            if fs::remove_file(&p).is_ok() {
                report.derived_removed += 1;
            }
        }
    }

    save_state(&dest, &state)?;

    // 3. Index the dest (incremental if it already exists).
    let stats = index_dest(&dest, stage.include_text_files, model)?;
    report.indexed_files = stats.0;
    report.total_chunks = stats.1;

    Ok(report)
}

/// Distill every verbose-JSON file under `source` into derived `.md` mirrored
/// in `dest`, then incrementally (re)index `dest`. The built-in counterpart to
/// [`run_stage`]: same derive→drop-vanished→index machinery, but the transform
/// is veles' own [`crate::distill`] (no external command, no Python) and the
/// source format is fixed to verbose JSON.
///
/// **Lock-free by contract** — the caller (the coordinator) already holds the
/// writer lock on `dest`. Acquiring it again here would self-deadlock. Safe to
/// call repeatedly: unchanged sources quick-skip on `(size, mtime)`.
pub fn run_distill_folder(
    source: &Path,
    dest: &Path,
    model: &StaticModel,
    _now_epoch_secs: i64,
) -> Result<StageReport> {
    let mut report = StageReport {
        stage: format!("distill:{}", source.display()),
        ..Default::default()
    };
    fs::create_dir_all(dest).with_context(|| format!("create dest {}", dest.display()))?;

    let mut state = load_state(dest);
    let mut alive: std::collections::HashSet<String> = std::collections::HashSet::new();

    for src in enumerate_json_files(source) {
        report.sources_seen += 1;
        let src_key = src.to_string_lossy().into_owned();
        alive.insert(src_key.clone());

        let meta = match fs::metadata(&src) {
            Ok(m) => m,
            Err(_) => continue,
        };
        let size = meta.len();
        let mtime_secs = mtime_secs(&meta);

        if let Some(prev) = state.sources.get(&src_key)
            && prev.size == size
            && prev.mtime_secs == mtime_secs
            && dest.join(&prev.derived_rel).is_file()
        {
            continue;
        }

        let derived_rel = derived_rel_path("", source, &src);
        let derived_abs = dest.join(&derived_rel);
        match crate::distill::distill_file(&src) {
            Some(text) => {
                if let Some(parent) = derived_abs.parent() {
                    fs::create_dir_all(parent).ok();
                }
                write_atomic_bytes(&derived_abs, text.as_bytes())
                    .with_context(|| format!("write derived {}", derived_abs.display()))?;
                state.sources.insert(
                    src_key,
                    SourceState {
                        size,
                        mtime_secs,
                        derived_rel: derived_rel.to_string_lossy().into_owned(),
                    },
                );
                report.derived_written += 1;
            }
            None => {
                report.transform_failures += 1;
                eprintln!("veles distill: could not read {}", src.display());
            }
        }
    }

    // Drop derived files whose source vanished.
    let gone: Vec<String> = state
        .sources
        .keys()
        .filter(|k| !alive.contains(*k))
        .cloned()
        .collect();
    for k in gone {
        if let Some(s) = state.sources.remove(&k) {
            let p = dest.join(&s.derived_rel);
            if fs::remove_file(&p).is_ok() {
                report.derived_removed += 1;
            }
        }
    }

    save_state(dest, &state)?;

    let stats = index_dest(dest, true, model)?;
    report.indexed_files = stats.0;
    report.total_chunks = stats.1;
    Ok(report)
}

/// Walk `source` for files veles treats as verbose JSON, skipping `.veles`/`.git`.
fn enumerate_json_files(source: &Path) -> Vec<PathBuf> {
    if !source.exists() {
        return Vec::new();
    }
    walkdir::WalkDir::new(source)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| {
            !matches!(
                e.file_name().to_str(),
                Some(".veles") | Some(".git") | Some("node_modules")
            )
        })
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .map(|e| e.into_path())
        .filter(|p| crate::distill::is_json_ext(p))
        .collect()
}

/// Build or incrementally update the index at `dest`, returning (files, chunks).
fn index_dest(
    dest: &Path,
    include_text_files: bool,
    model: &StaticModel,
) -> Result<(usize, usize)> {
    if persist::index_exists(dest) {
        let mut index = VelesIndex::load(dest, model.clone())?;
        let report = index.update_from_path(dest)?;
        if !report.is_noop() {
            index.save(dest)?;
        }
        let s = index.stats();
        Ok((s.indexed_files, s.total_chunks))
    } else {
        let index = VelesIndex::from_path(dest, Some(model.clone()), None, include_text_files)?;
        index.save(dest)?;
        let s = index.stats();
        Ok((s.indexed_files, s.total_chunks))
    }
}

/// Run `cmd[0] cmd[1..] <abs-source>`; return stdout as a String. Non-zero
/// exit or non-UTF-8 stdout is an error. The source path is also exported as
/// `VELES_SOURCE` for adapters that prefer the env.
fn run_transform(cmd: &[String], source: &Path) -> Result<String> {
    let out = Command::new(&cmd[0])
        .args(&cmd[1..])
        .arg(source)
        .env("VELES_SOURCE", source)
        .output()
        .with_context(|| format!("spawn transform {:?}", cmd))?;
    if !out.status.success() {
        bail!(
            "transform exited {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    String::from_utf8(out.stdout).context("transform stdout was not valid UTF-8")
}

// ── path helpers ──────────────────────────────────────────────────────────

/// Expand a leading `~` to `$HOME`. Other `~user` forms are left as-is.
pub fn expand_tilde(s: &str) -> PathBuf {
    if let Some(rest) = s.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(rest);
        }
    } else if s == "~"
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home);
    }
    PathBuf::from(s)
}

/// Split a glob pattern into (literal base dir to walk, full-path matcher).
/// The base is the longest leading run of components free of glob metachars.
fn glob_base_and_matcher(pattern: &Path) -> Result<(PathBuf, globset::GlobMatcher)> {
    let mut base = PathBuf::new();
    for comp in pattern.components() {
        match comp {
            Component::Prefix(p) => base.push(p.as_os_str()),
            Component::RootDir => base.push(Component::RootDir.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => base.push(".."),
            Component::Normal(os) => {
                let s = os.to_string_lossy();
                if s.contains(['*', '?', '[', ']', '{', '}']) {
                    break;
                }
                base.push(os);
            }
        }
    }
    if base.as_os_str().is_empty() {
        base.push(".");
    }
    let matcher = globset::Glob::new(&pattern.to_string_lossy())
        .with_context(|| format!("invalid glob {}", pattern.display()))?
        .compile_matcher();
    Ok((base, matcher))
}

/// Walk `base` and return files whose absolute path matches `matcher`.
fn enumerate_sources(base: &Path, matcher: &globset::GlobMatcher) -> Vec<PathBuf> {
    if !base.exists() {
        return Vec::new();
    }
    walkdir::WalkDir::new(base)
        .follow_links(false)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .map(|e| e.into_path())
        .filter(|p| matcher.is_match(p))
        .collect()
}

/// Map a source file to its derived path *relative to dest*: mirror the
/// source's path under `base`, swapping the extension to `.md`. Keeps
/// provenance legible (`<project>/<id>.md`) and lets `path:` scoping work,
/// while veles stays format-blind.
fn effective_input_name(input: &Input, index: usize) -> String {
    let trimmed = input.name.trim();
    if trimmed.is_empty() {
        format!("input-{}", index + 1)
    } else {
        trimmed.to_string()
    }
}

fn resolve_config_path(base: &Path, value: &str) -> PathBuf {
    let expanded = expand_tilde(value);
    if expanded.is_relative() {
        base.join(expanded)
    } else {
        expanded
    }
}

fn derived_rel_path(namespace: &str, base: &Path, source: &Path) -> PathBuf {
    let rel = source.strip_prefix(base).unwrap_or(source);
    let mut out = PathBuf::from(namespace);
    out.push(rel);
    out.set_extension("md");
    out
}

fn mtime_secs(meta: &fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ── state + atomic write ──────────────────────────────────────────────────

fn load_state(dest: &Path) -> PipelineState {
    let p = index_dir_for(dest).join(STATE_FILE);
    fs::read(&p)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn save_state(dest: &Path, state: &PipelineState) -> Result<()> {
    let dir = index_dir_for(dest);
    fs::create_dir_all(&dir).ok();
    let p = dir.join(STATE_FILE);
    let bytes = serde_json::to_vec_pretty(state)?;
    write_atomic_bytes(&p, &bytes)
}

/// Write bytes to `path` atomically (temp + rename in the same dir).
fn write_atomic_bytes(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension(format!(
        "{}tmp",
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| format!("{e}."))
            .unwrap_or_default()
    ));
    fs::write(&tmp, bytes).with_context(|| format!("write {}", tmp.display()))?;
    fs::rename(&tmp, path)
        .with_context(|| format!("rename {} -> {}", tmp.display(), path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_base_is_literal_prefix() {
        let (base, m) = glob_base_and_matcher(Path::new("/tmp/a/b/**/*.jsonl")).unwrap();
        assert_eq!(base, PathBuf::from("/tmp/a/b"));
        assert!(m.is_match("/tmp/a/b/x/y.jsonl"));
        assert!(!m.is_match("/tmp/a/b/x/y.txt"));
    }

    #[test]
    fn derived_mirrors_source_as_md() {
        let d = derived_rel_path(
            "codex",
            Path::new("/src"),
            Path::new("/src/proj/sess.jsonl"),
        );
        assert_eq!(d, PathBuf::from("codex/proj/sess.md"));
    }

    #[test]
    fn run_stage_derives_and_indexes_then_is_incremental() {
        // A fake "transcript" source tree + a trivial transform (cat-like via
        // `sh -c`), driven end to end through one stage.
        let tmp = tempfile::tempdir().unwrap();
        let src_dir = tmp.path().join("srcs/projA");
        fs::create_dir_all(&src_dir).unwrap();
        fs::write(src_dir.join("s1.jsonl"), "hello orbital decay\n").unwrap();
        let dest = tmp.path().join("corpus");

        // transform: emit a markdown wrapper around the file contents.
        let stage = Stage {
            name: "t".into(),
            dest: dest.to_string_lossy().into_owned(),
            include_text_files: true,
            inputs: vec![Input {
                name: "fixture".into(),
                source: format!("{}/srcs/**/*.jsonl", tmp.path().to_string_lossy()),
                transform: vec![
                    "sh".into(),
                    "-c".into(),
                    "echo '# session'; cat \"$1\"".into(),
                    "sh".into(),
                ],
            }],
        };

        let model = crate::model::load_model(None).expect("model");
        let r1 = run_stage(&stage, &model, 1000).unwrap();
        assert!(r1.skipped_locked.is_none());
        assert_eq!(r1.sources_seen, 1);
        assert_eq!(r1.derived_written, 1);
        assert!(r1.total_chunks >= 1, "expected indexed chunks, got {r1:?}");
        // Derived file mirrors source *below the glob base* as .md under dest:
        // base is `<tmp>/srcs`, so `srcs/projA/s1.jsonl` → `projA/s1.md`.
        assert!(
            dest.join("fixture/projA/s1.md").is_file(),
            "derived not found under dest"
        );

        // Re-run with no source change → quick-skip, nothing re-derived.
        let r2 = run_stage(&stage, &model, 1001).unwrap();
        assert_eq!(
            r2.derived_written, 0,
            "unchanged source should skip: {r2:?}"
        );
    }

    #[test]
    fn inputs_with_matching_relative_paths_are_namespaced() {
        let tmp = tempfile::tempdir().unwrap();
        for source in ["claude", "codex"] {
            let dir = tmp.path().join(source).join("project");
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("session.jsonl"), source).unwrap();
        }
        let dest = tmp.path().join("corpus");
        let stage = Stage {
            name: "sessions".into(),
            dest: dest.to_string_lossy().into_owned(),
            include_text_files: true,
            inputs: ["claude", "codex"]
                .into_iter()
                .map(|name| Input {
                    name: name.into(),
                    source: format!("{}/{name}/**/*.jsonl", tmp.path().to_string_lossy()),
                    transform: vec!["cat".into()],
                })
                .collect(),
        };
        let model = crate::model::load_model(None).expect("model");
        run_stage(&stage, &model, 1).unwrap();

        assert!(dest.join("claude/project/session.md").is_file());
        assert!(dest.join("codex/project/session.md").is_file());
    }

    #[test]
    fn config_paths_resolve_against_config_directory() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("scripts")).unwrap();
        fs::write(tmp.path().join("scripts/adapter.py"), "").unwrap();
        let mut config = PipelineConfig {
            stages: vec![Stage {
                name: "relative".into(),
                dest: "derived".into(),
                include_text_files: true,
                inputs: vec![Input {
                    name: "source".into(),
                    source: "inputs/**/*.jsonl".into(),
                    transform: vec!["python3".into(), "scripts/adapter.py".into()],
                }],
            }],
        };
        config.resolve_relative_to(tmp.path());
        assert_eq!(
            PathBuf::from(&config.stages[0].dest),
            tmp.path().join("derived")
        );
        assert_eq!(
            PathBuf::from(&config.stages[0].inputs[0].source),
            tmp.path().join("inputs/**/*.jsonl")
        );
        assert_eq!(
            PathBuf::from(&config.stages[0].inputs[0].transform[1]),
            tmp.path().join("scripts/adapter.py")
        );
    }
}
