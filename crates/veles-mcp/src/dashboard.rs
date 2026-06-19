//! Optional per-repo web dashboard for the MCP server (`serve-mcp
//! --dashboard`). Feature-gated behind `dashboard` so the default binary
//! stays a lean single-purpose tool.
//!
//! Scope is deliberately **per server instance**. The dashboard is
//! observability only: its port never participates in repository writer
//! coordination.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::{
    Router,
    extract::State,
    response::sse::{Event, KeepAlive, Sse},
    response::{Html, Json},
    routing::{get, post},
};
use serde::Deserialize;
use tokio::sync::broadcast;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;

use veles_core::cache::IndexCache;

use crate::watch::{RepoRole, WatchManager};

/// Bind the requested dashboard port. If a fixed port is unavailable, use an
/// ephemeral port; dashboard transport never influences repository ownership.
pub fn bind(port: u16) -> anyhow::Result<(std::net::TcpListener, SocketAddr)> {
    let listener = std::net::TcpListener::bind(("127.0.0.1", port))
        .or_else(|_| std::net::TcpListener::bind(("127.0.0.1", 0)))?;
    let addr = listener.local_addr()?;
    Ok((listener, addr))
}

#[derive(Clone)]
struct DashState {
    cache: Arc<IndexCache>,
    watch: Option<Arc<WatchManager>>,
    events: broadcast::Sender<String>,
    workspace: String,
}

/// Serve the dashboard on an already-bound `listener` (from [`elect`] or
/// [`try_become_owner`], so the singleton election has already happened).
/// Spawns onto the current runtime and returns immediately. Errors are
/// non-fatal — the MCP server runs fine without the UI.
pub fn serve(
    listener: std::net::TcpListener,
    cache: Arc<IndexCache>,
    watch: Option<Arc<WatchManager>>,
    events: broadcast::Sender<String>,
    workspace: String,
) {
    tokio::spawn(async move {
        let state = DashState {
            cache,
            watch,
            events,
            workspace,
        };
        let app = Router::new()
            .route("/", get(index))
            .route("/api/status", get(status))
            .route("/api/events", get(events_sse))
            .route("/api/related", post(add_related))
            .with_state(state);

        if let Err(e) = listener.set_nonblocking(true) {
            eprintln!("veles dashboard: could not set non-blocking: {e}");
            return;
        }
        let listener = match tokio::net::TcpListener::from_std(listener) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("veles dashboard: could not adopt listener: {e}");
                return;
            }
        };
        if let Err(e) = axum::serve(listener, app).await {
            eprintln!("veles dashboard: server error: {e}");
        }
    });
}

/// JSON snapshot of the repos this server has loaded.
async fn status(State(st): State<DashState>) -> Json<serde_json::Value> {
    let coordination = st.watch.as_ref().map(|w| w.statuses()).unwrap_or_default();

    let mut repos = Vec::new();
    for key in st.cache.loaded_repos() {
        let Some(idx) = st.cache.peek(&key) else {
            continue;
        };
        let guard = idx.read().await;
        let stats = guard.stats();
        let canonical = std::fs::canonicalize(&key).ok();
        let repo_status = coordination
            .iter()
            .find(|status| canonical.as_ref() == Some(&status.path));
        repos.push(serde_json::json!({
            "repo": key,
            "indexed_files": stats.indexed_files,
            "total_chunks": stats.total_chunks,
            "languages": stats.languages,
            "role": repo_status.map(|s| match s.role {
                RepoRole::Writer => "updating",
                RepoRole::Reader => "reading",
            }).unwrap_or("loading"),
            "watched": repo_status.is_some_and(|s| s.watching),
            "generation": veles_core::persist::current_generation(std::path::Path::new(&key)),
        }));
    }

    Json(serde_json::json!({
        "server": "veles",
        "version": env!("CARGO_PKG_VERSION"),
        "workspace": st.workspace,
        "automatic_updates": st.watch.is_some(),
        "repos": repos,
    }))
}

/// Server-sent-events stream of live activity (searches + auto-updates).
async fn events_sse(
    State(st): State<DashState>,
) -> Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>> {
    let rx = st.events.subscribe();
    let stream = BroadcastStream::new(rx).map(|msg| {
        let line = msg.unwrap_or_else(|_| "…".to_string());
        Ok(Event::default().data(line))
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

/// Request body for `POST /api/related`: a repo path to add to the read-set.
#[derive(Deserialize)]
struct AddRepoRequest {
    path: String,
}

/// Add a repo to this workspace's `[related]` read-set in
/// `<workspace>/.veles/config.toml`. The running MCP server re-reads this file
/// when it changes (see `VelesServer::read_set`), so the addition is live — no
/// restart — and the coordinator for the new repo starts on the next search.
async fn add_related(
    State(st): State<DashState>,
    Json(req): Json<AddRepoRequest>,
) -> Json<serde_json::Value> {
    match crate::persist_related_repo(&st.workspace, req.path.trim()) {
        Ok(msg) => {
            let _ = st
                .events
                .send(format!("added related repo: {}", req.path.trim()));
            Json(serde_json::json!({ "ok": true, "message": msg }))
        }
        Err(e) => Json(serde_json::json!({ "ok": false, "message": e })),
    }
}

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

const INDEX_HTML: &str = r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>veles dashboard</title>
<style>
  :root { color-scheme: light dark; }
  body { font: 14px/1.5 ui-monospace, SFMono-Regular, Menlo, monospace; margin: 2rem; max-width: 880px; }
  h1 { font-size: 1.2rem; } h2 { font-size: 1rem; margin-top: 1.5rem; }
  .repo { border: 1px solid #8884; border-radius: 8px; padding: .75rem 1rem; margin: .5rem 0; }
  .repo .path { font-weight: 600; word-break: break-all; }
  .badge { font-size: .75rem; padding: .1rem .5rem; border-radius: 999px; border: 1px solid #8888; }
  .on { color: #2a7; border-color: #2a7; } .off { color: #a55; border-color: #a55; }
  .langs { color: #888; font-size: .8rem; }
  #feed { background: #8881; border-radius: 8px; padding: .5rem 1rem; height: 16rem; overflow-y: auto; }
  #feed div { white-space: pre-wrap; }
  .muted { color: #888; }
  #addform { display: flex; gap: .5rem; margin: .5rem 0; flex-wrap: wrap; }
  #addpath { flex: 1; min-width: 18rem; font: inherit; padding: .4rem .6rem; border: 1px solid #8884; border-radius: 6px; background: transparent; color: inherit; }
  #addbtn { font: inherit; padding: .4rem .9rem; border: 1px solid #8884; border-radius: 6px; background: #8881; color: inherit; cursor: pointer; }
  #addbtn:disabled { opacity: .5; cursor: default; }
  #addmsg { font-size: .8rem; margin: .25rem 0; min-height: 1.2em; }
  #addmsg.ok { color: #2a7; } #addmsg.err { color: #a55; }
</style>
</head>
<body>
  <h1>veles dashboard <span id="ver" class="muted"></span></h1>
  <div class="muted">Workspace: <span id="workspace"></span> · automatic updates: <span id="watch"></span></div>
  <h2>Indexed repositories</h2>
  <div id="repos"></div>
  <h2>Add a related repository</h2>
  <div class="muted">Adds a folder to this workspace's <code>[related]</code> read-set so it joins searches — live, no restart. Verbose-JSON folders (e.g. agent transcripts) are distilled automatically. Persisted to <code>.veles/config.toml</code>.</div>
  <form id="addform">
    <input id="addpath" type="text" placeholder="/absolute/path/to/repo  (or path relative to workspace)" autocomplete="off" spellcheck="false">
    <button id="addbtn" type="submit">Add</button>
  </form>
  <div id="addmsg"></div>
  <h2>Live activity</h2>
  <div id="feed"></div>
<script>
async function refresh() {
  try {
    const s = await (await fetch('/api/status')).json();
    document.getElementById('ver').textContent = 'v' + s.version;
    document.getElementById('workspace').textContent = s.workspace;
    document.getElementById('watch').textContent = s.automatic_updates ? 'enabled' : 'unavailable';
    const root = document.getElementById('repos');
    root.innerHTML = s.repos.length ? '' : '<div class="muted">No repos loaded yet — run a search.</div>';
    for (const r of s.repos) {
      const langs = Object.entries(r.languages || {}).map(([k,v]) => k+':'+v).join('  ');
      const el = document.createElement('div');
      el.className = 'repo';
      const path = document.createElement('div');
      path.className = 'path'; path.textContent = r.repo;
      const details = document.createElement('div');
      details.textContent = `${r.indexed_files} files · ${r.total_chunks} chunks · generation ${r.generation || 'legacy'} `;
      const badge = document.createElement('span');
      badge.className = `badge ${r.watched?'on':'off'}`;
      badge.textContent = r.watched ? 'updating' : r.role;
      details.appendChild(badge);
      const languages = document.createElement('div');
      languages.className = 'langs'; languages.textContent = langs;
      el.append(path, details, languages);
      root.appendChild(el);
    }
  } catch (e) { /* server going away */ }
}
function line(t) {
  const f = document.getElementById('feed');
  const d = document.createElement('div');
  d.textContent = new Date().toLocaleTimeString() + '  ' + t;
  f.appendChild(d); f.scrollTop = f.scrollHeight;
  if (f.childElementCount > 200) f.removeChild(f.firstChild);
}
const form = document.getElementById('addform');
const pathInput = document.getElementById('addpath');
const addBtn = document.getElementById('addbtn');
const addMsg = document.getElementById('addmsg');
form.addEventListener('submit', async (e) => {
  e.preventDefault();
  const path = pathInput.value.trim();
  if (!path) return;
  addBtn.disabled = true; addMsg.className = 'muted'; addMsg.textContent = 'adding…';
  try {
    const res = await fetch('/api/related', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ path }),
    });
    const r = await res.json();
    addMsg.className = r.ok ? 'ok' : 'err';
    addMsg.textContent = r.message;
    if (r.ok) pathInput.value = '';
  } catch (err) {
    addMsg.className = 'err'; addMsg.textContent = 'request failed: ' + err;
  } finally {
    addBtn.disabled = false;
    refresh();
  }
});
const es = new EventSource('/api/events');
es.onmessage = (e) => { line(e.data); refresh(); };
refresh(); setInterval(refresh, 5000);
</script>
</body>
</html>
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn port_conflict_falls_back_without_affecting_service() {
        let occupied = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let occupied_port = occupied.local_addr().unwrap().port();
        let (_listener, addr) = bind(occupied_port).unwrap();
        assert_ne!(addr.port(), occupied_port);
    }

    fn read_related(ws: &Path) -> Vec<String> {
        let cfg =
            std::fs::read_to_string(veles_core::persist::index_dir_for(ws).join("config.toml"))
                .unwrap();
        let doc = cfg.parse::<toml_edit::DocumentMut>().unwrap();
        doc["related"]["repos"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn add_related_persists_canonical_path() {
        let ws = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let other_canon = std::fs::canonicalize(other.path()).unwrap();

        let msg = crate::persist_related_repo(
            ws.path().to_str().unwrap(),
            other.path().to_str().unwrap(),
        )
        .unwrap();
        assert!(msg.contains(other_canon.to_str().unwrap()), "msg: {msg}");
        assert_eq!(
            read_related(ws.path()),
            vec![other_canon.to_string_lossy().into_owned()]
        );
    }

    #[test]
    fn add_related_is_idempotent() {
        let ws = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let p = other.path().to_str().unwrap();
        crate::persist_related_repo(ws.path().to_str().unwrap(), p).unwrap();
        let again = crate::persist_related_repo(ws.path().to_str().unwrap(), p).unwrap();
        assert!(again.contains("already"), "msg: {again}");
        assert_eq!(read_related(ws.path()).len(), 1);
    }

    #[test]
    fn add_related_preserves_existing_config() {
        let ws = tempfile::tempdir().unwrap();
        let cfg_dir = veles_core::persist::index_dir_for(ws.path());
        std::fs::create_dir_all(&cfg_dir).unwrap();
        std::fs::write(
            cfg_dir.join("config.toml"),
            "# my read-set\n[related]\nrepos = []\n",
        )
        .unwrap();
        let other = tempfile::tempdir().unwrap();
        crate::persist_related_repo(ws.path().to_str().unwrap(), other.path().to_str().unwrap())
            .unwrap();
        let text = std::fs::read_to_string(cfg_dir.join("config.toml")).unwrap();
        assert!(text.contains("# my read-set"), "comment lost: {text}");
    }

    #[test]
    fn add_related_rejects_workspace_itself() {
        let ws = tempfile::tempdir().unwrap();
        let wp = ws.path().to_str().unwrap();
        // canonicalize so the comparison against the stored workspace matches.
        let canon = std::fs::canonicalize(ws.path()).unwrap();
        let err = crate::persist_related_repo(canon.to_str().unwrap(), wp).unwrap_err();
        assert!(err.contains("current workspace"), "err: {err}");
    }

    #[test]
    fn add_related_rejects_nonexistent_path() {
        let ws = tempfile::tempdir().unwrap();
        let err =
            crate::persist_related_repo(ws.path().to_str().unwrap(), "/no/such/path/veles-test")
                .unwrap_err();
        assert!(err.contains("cannot resolve"), "err: {err}");
    }
}
