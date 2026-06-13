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
use std::sync::Arc;

use axum::{
    extract::State,
    response::sse::{Event, KeepAlive, Sse},
    response::{Html, Json},
    routing::get,
    Router,
};
use tokio::sync::broadcast;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt;

use veles_core::cache::IndexCache;

use crate::watch::WatchManager;

#[derive(Clone)]
struct DashState {
    cache: Arc<IndexCache>,
    watch: Option<Arc<WatchManager>>,
    events: broadcast::Sender<String>,
}

/// Start the dashboard HTTP server on `127.0.0.1:port` (port 0 = OS-chosen
/// free port). Spawns onto the current runtime; returns once bound so the
/// URL can be logged. Errors are reported to stderr and are non-fatal — the
/// MCP server runs fine without the UI.
pub fn spawn(
    cache: Arc<IndexCache>,
    watch: Option<Arc<WatchManager>>,
    events: broadcast::Sender<String>,
    port: u16,
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
            .with_state(state);

        let listener = match tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
            Ok(l) => l,
            Err(e) => {
                eprintln!("veles dashboard: could not bind 127.0.0.1:{port}: {e}");
                return;
            }
        };
        match listener.local_addr() {
            Ok(addr) => eprintln!("veles dashboard: http://{addr}"),
            Err(_) => eprintln!("veles dashboard: listening"),
        }
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
            .map(|c| watched.iter().any(|r| *r == c))
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
</style>
</head>
<body>
  <h1>veles dashboard <span id="ver" class="muted"></span></h1>
  <div class="muted">Per-repo view of this MCP server. Watch: <span id="watch"></span></div>
  <h2>Indexed repositories</h2>
  <div id="repos"></div>
  <h2>Live activity</h2>
  <div id="feed"></div>
<script>
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
