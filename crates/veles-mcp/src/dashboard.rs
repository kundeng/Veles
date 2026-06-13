//! Optional per-repo web dashboard for the MCP server (`serve-mcp
//! --dashboard`). Feature-gated behind `dashboard` so the default binary
//! stays a lean single-purpose tool.
//!
//! Scope is deliberately **per server instance**: it shows only the repos
//! *this* MCP process has loaded — index health (files/chunks/languages),
//! whether the watcher is keeping them fresh, and a live feed of search and
//! auto-update events. It binds a free localhost port and logs the URL to
//! stderr; it never auto-opens a browser and never aggregates other
//! sessions' repos (which would just be confusing).

use std::convert::Infallible;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::Duration;

use axum::{
    extract::State,
    response::sse::{Event, KeepAlive, Sse},
    response::{Html, Json},
    routing::{get, post},
    Router,
};
use serde::Deserialize;
use tokio::sync::broadcast;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt;

use veles_core::cache::IndexCache;

use crate::watch::WatchManager;

/// Outcome of trying to claim the dashboard port at startup. The port doubles
/// as the singleton election token: exactly one veles process owns it, and so
/// owns watching + updating the indexes and serving the dashboard. Everyone
/// else follows.
pub enum Election {
    /// We bound the port — this process is the owner. Carries the bound
    /// listener (ready to hand to [`serve`]) and the address for logging.
    Owner(std::net::TcpListener, SocketAddr),
    /// The port is already held by *another veles dashboard* — stand down and
    /// run as a follower (no watcher, no dashboard; reload-on-read instead).
    FollowAnotherVeles,
    /// The port is held by some *non-veles* service — the caller should bind a
    /// free port instead so it still gets a dashboard without fighting.
    Foreign,
}

/// Try to claim `port` on localhost (startup election). Distinguishes "another
/// veles already owns it" (→ follow) from "an unrelated service is squatting
/// the port" (→ caller picks a free port) by probing `/api/status`.
pub fn elect(port: u16) -> Election {
    match std::net::TcpListener::bind(("127.0.0.1", port)) {
        Ok(l) => {
            let addr = l.local_addr().unwrap_or(SocketAddr::from(([127, 0, 0, 1], port)));
            Election::Owner(l, addr)
        }
        Err(_) if port != 0 && probe_is_veles(port) => Election::FollowAnotherVeles,
        Err(_) => Election::Foreign,
    }
}

/// Cheap promotion check (self-heal): just try to bind `port`. Returns the
/// listener iff it's now free — i.e. the previous owner exited and this
/// follower can take over. No `/api/status` probe needed: if the bind fails we
/// stay a follower regardless of who holds it.
pub fn try_become_owner(port: u16) -> Option<(std::net::TcpListener, SocketAddr)> {
    let l = std::net::TcpListener::bind(("127.0.0.1", port)).ok()?;
    let addr = l.local_addr().unwrap_or(SocketAddr::from(([127, 0, 0, 1], port)));
    Some((l, addr))
}

/// Best-effort: is whatever holds `port` a veles dashboard? Sends a minimal
/// HTTP/1.0 request and looks for our status signature. Any error → `false`
/// (treat as foreign), with short timeouts so startup never blocks.
fn probe_is_veles(port: u16) -> bool {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let Ok(mut stream) = TcpStream::connect_timeout(&addr, Duration::from_millis(300)) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_millis(300)));
    let _ = stream.set_write_timeout(Some(Duration::from_millis(300)));
    if stream
        .write_all(b"GET /api/status HTTP/1.0\r\nHost: localhost\r\n\r\n")
        .is_err()
    {
        return false;
    }
    let mut buf = String::new();
    let _ = stream.take(4096).read_to_string(&mut buf);
    buf.contains("\"server\":\"veles\"")
}

#[derive(Clone)]
struct DashState {
    cache: Arc<IndexCache>,
    watch: Option<Arc<WatchManager>>,
    events: broadcast::Sender<String>,
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
) {
    tokio::spawn(async move {
        let state = DashState {
            cache,
            watch,
            events,
        };
        let app = Router::new()
            .route("/", get(index))
            .route("/api/status", get(status))
            .route("/api/events", get(events_sse))
            .route("/api/watch", post(add_watch))
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
    let watched: Vec<std::path::PathBuf> = st
        .watch
        .as_ref()
        .map(|w| w.watched_roots())
        .unwrap_or_default();

    let mut repos = Vec::new();
    for key in st.cache.loaded_repos() {
        let Some(idx) = st.cache.peek(&key) else {
            continue;
        };
        let guard = idx.read().await;
        let stats = guard.stats();
        // A repo is "watched" if its canonical path is in the watch set.
        let is_watched = std::fs::canonicalize(&key)
            .map(|c| watched.contains(&c))
            .unwrap_or(false);
        repos.push(serde_json::json!({
            "repo": key,
            "indexed_files": stats.indexed_files,
            "total_chunks": stats.total_chunks,
            "languages": stats.languages,
            "watched": is_watched,
        }));
    }

    Json(serde_json::json!({
        "server": "veles",
        "version": env!("CARGO_PKG_VERSION"),
        "watch_enabled": st.watch.is_some(),
        "repos": repos,
    }))
}

/// Request body for `POST /api/watch`.
#[derive(Deserialize)]
struct WatchReq {
    /// Local directory to index and start watching.
    path: String,
}

/// `POST /api/watch {"path": "..."}` — add an external directory to this
/// (owner) server at runtime: load/build its index and start watching it, so
/// it shows up in the dashboard and stays fresh. Add-only for now. The
/// dashboard only runs on the owner, so `watch` is always present here.
async fn add_watch(
    State(st): State<DashState>,
    Json(req): Json<WatchReq>,
) -> Json<serde_json::Value> {
    let path = req.path.trim().to_string();
    if path.is_empty() {
        return Json(serde_json::json!({"ok": false, "error": "path is required"}));
    }
    if !std::path::Path::new(&path).is_dir() {
        return Json(serde_json::json!({"ok": false, "error": format!("not a directory: {path}")}));
    }
    match st.cache.get_or_load(&path, false).await {
        Ok(idx) => {
            if let Some(w) = &st.watch {
                w.watch(&path);
            }
            let stats = idx.read().await.stats();
            let _ = st.events.send(format!("added watch: {path}"));
            Json(serde_json::json!({
                "ok": true,
                "repo": path,
                "indexed_files": stats.indexed_files,
                "total_chunks": stats.total_chunks,
            }))
        }
        Err(e) => Json(serde_json::json!({"ok": false, "error": format!("index failed: {e}")})),
    }
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
  #add { display: flex; gap: .5rem; margin: .5rem 0 1rem; }
  #add input { flex: 1; font: inherit; padding: .35rem .5rem; border: 1px solid #8888; border-radius: 6px; background: transparent; color: inherit; }
  #add button { font: inherit; padding: .35rem .8rem; border: 1px solid #8888; border-radius: 6px; background: transparent; color: inherit; cursor: pointer; }
  #addmsg { font-size: .8rem; margin-left: .25rem; }
</style>
</head>
<body>
  <h1>veles dashboard <span id="ver" class="muted"></span></h1>
  <div class="muted">Per-repo view of this MCP server. Watch: <span id="watch"></span></div>
  <h2>Indexed repositories</h2>
  <form id="add" onsubmit="return addDir(event)">
    <input id="addpath" type="text" placeholder="/absolute/path/to/a/repo to index + watch" autocomplete="off" spellcheck="false">
    <button type="submit">Add directory</button>
    <span id="addmsg" class="muted"></span>
  </form>
  <div id="repos"></div>
  <h2>Live activity</h2>
  <div id="feed"></div>
<script>
async function addDir(e) {
  e.preventDefault();
  const inp = document.getElementById('addpath');
  const msg = document.getElementById('addmsg');
  const path = inp.value.trim();
  if (!path) return false;
  msg.textContent = 'indexing…'; msg.className = 'muted';
  try {
    const r = await (await fetch('/api/watch', {
      method: 'POST', headers: {'Content-Type': 'application/json'},
      body: JSON.stringify({path})
    })).json();
    if (r.ok) {
      msg.textContent = `added (${r.indexed_files} files, ${r.total_chunks} chunks)`;
      msg.className = 'on'; inp.value = ''; refresh();
    } else {
      msg.textContent = r.error || 'failed'; msg.className = 'off';
    }
  } catch (err) { msg.textContent = String(err); msg.className = 'off'; }
  return false;
}
async function refresh() {
  try {
    const s = await (await fetch('/api/status')).json();
    document.getElementById('ver').textContent = 'v' + s.version;
    document.getElementById('watch').textContent = s.watch_enabled ? 'enabled' : 'off';
    const root = document.getElementById('repos');
    root.innerHTML = s.repos.length ? '' : '<div class="muted">No repos loaded yet — run a search.</div>';
    for (const r of s.repos) {
      const langs = Object.entries(r.languages || {}).map(([k,v]) => k+':'+v).join('  ');
      const el = document.createElement('div');
      el.className = 'repo';
      el.innerHTML = `<div class="path">${r.repo}</div>
        <div>${r.indexed_files} files · ${r.total_chunks} chunks
        <span class="badge ${r.watched?'on':'off'}">${r.watched?'watching':'not watched'}</span></div>
        <div class="langs">${langs}</div>`;
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
const es = new EventSource('/api/events');
es.onmessage = (e) => { line(e.data); refresh(); };
refresh(); setInterval(refresh, 5000);
</script>
</body>
</html>
"#;
