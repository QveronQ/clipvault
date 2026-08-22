//! clipvault-server — point de rendez-vous de la synchronisation.
//!
//! Journal d'événements append-only (SQLite) + store d'objets sur disque.
//! Les daemons poussent en REST et reçoivent en WebSocket (voir
//! `clipvault_core::sync` pour le protocole).
//!
//! Configuration par variables d'environnement :
//! - `CLIPVAULT_TOKEN`     jeton partagé (obligatoire)
//! - `CLIPVAULT_LISTEN`    adresse d'écoute (défaut 0.0.0.0:7700)
//! - `CLIPVAULT_DATA_DIR`  répertoire de données (défaut ~/.local/share/clipvault-server)

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use clipvault_core::sync::{PushAck, PushItem, ServerClient, ServerStatus, SyncEvent};
use rusqlite::{params, Connection};
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::broadcast;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

const MAX_BODY: usize = 32 * 1024 * 1024;

struct AppState {
    db: Mutex<Connection>,
    objects_dir: PathBuf,
    token: String,
    tx: broadcast::Sender<SyncEvent>,
    started_at: i64,
    clients: Mutex<HashMap<u64, ServerClient>>,
    next_client_id: AtomicU64,
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Garde d'enregistrement d'un client connecté (désinscription au drop).
struct ClientGuard {
    state: Arc<AppState>,
    id: u64,
}

impl ClientGuard {
    fn register(state: Arc<AppState>, device: &str) -> Self {
        let id = state.next_client_id.fetch_add(1, Ordering::Relaxed);
        state.clients.lock().unwrap().insert(
            id,
            ServerClient {
                device: device.to_string(),
                connected_at: now(),
            },
        );
        Self { state, id }
    }
}

impl Drop for ClientGuard {
    fn drop(&mut self) {
        self.state.clients.lock().unwrap().remove(&self.id);
    }
}

impl AppState {
    fn append_event(&self, origin: &str, item: &PushItem) -> Result<i64> {
        let payload = serde_json::to_string(item)?;
        let db = self.db.lock().unwrap();
        db.execute(
            "INSERT INTO events (origin, payload) VALUES (?1, ?2)",
            params![origin, payload],
        )?;
        Ok(db.last_insert_rowid())
    }

    fn replay(&self, since: i64) -> Result<Vec<SyncEvent>> {
        let db = self.db.lock().unwrap();
        let mut stmt = db.prepare_cached(
            "SELECT seq, origin, payload FROM events WHERE seq > ?1 ORDER BY seq",
        )?;
        let rows = stmt.query_map([since], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (seq, origin, payload) = row?;
            match serde_json::from_str::<PushItem>(&payload) {
                Ok(item) => out.push(SyncEvent { seq, origin, item }),
                Err(e) => warn!("événement {seq} illisible: {e}"),
            }
        }
        Ok(out)
    }
}

fn check_auth(state: &AppState, headers: &HeaderMap) -> Result<(), StatusCode> {
    let ok = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .is_some_and(|t| t == state.token);
    if ok {
        Ok(())
    } else {
        Err(StatusCode::UNAUTHORIZED)
    }
}

fn device_from_headers(headers: &HeaderMap) -> String {
    headers
        .get("x-device")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("unknown")
        .to_string()
}

async fn health() -> &'static str {
    "ok"
}

async fn status(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<ServerStatus>, StatusCode> {
    check_auth(&state, &headers)?;
    let events: i64 = {
        let db = state.db.lock().unwrap();
        db.query_row("SELECT COALESCE(MAX(seq), 0) FROM events", [], |r| r.get(0))
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    };
    let (mut objects, mut objects_bytes) = (0u64, 0u64);
    if let Ok(dir) = std::fs::read_dir(&state.objects_dir) {
        for entry in dir.flatten() {
            if let Ok(meta) = entry.metadata() {
                objects += 1;
                objects_bytes += meta.len();
            }
        }
    }
    let clients = state.clients.lock().unwrap().values().cloned().collect();
    Ok(Json(ServerStatus {
        version: env!("CARGO_PKG_VERSION").to_string(),
        started_at: state.started_at,
        events,
        objects,
        objects_bytes,
        clients,
    }))
}

async fn push(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(item): Json<PushItem>,
) -> Result<Json<PushAck>, StatusCode> {
    check_auth(&state, &headers)?;
    let origin = item
        .device_id()
        .map(str::to_string)
        .unwrap_or_else(|| device_from_headers(&headers));
    let seq = state
        .append_event(&origin, &item)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let _ = state.tx.send(SyncEvent { seq, origin, item });
    Ok(Json(PushAck { seq }))
}

fn object_path(state: &AppState, hash: &str) -> Result<PathBuf, StatusCode> {
    // Adressage par hash BLAKE3 hex uniquement — pas de traversée possible.
    if hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(StatusCode::BAD_REQUEST);
    }
    Ok(state.objects_dir.join(hash))
}

async fn put_object(
    State(state): State<Arc<AppState>>,
    Path(hash): Path<String>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<StatusCode, StatusCode> {
    check_auth(&state, &headers)?;
    let path = object_path(&state, &hash)?;
    if path.exists() {
        return Ok(StatusCode::OK); // déjà présent (dédup par hash)
    }
    tokio::fs::write(&path, &body)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(StatusCode::CREATED)
}

async fn get_object(
    State(state): State<Arc<AppState>>,
    Path(hash): Path<String>,
    headers: HeaderMap,
) -> Result<Vec<u8>, StatusCode> {
    check_auth(&state, &headers)?;
    let path = object_path(&state, &hash)?;
    tokio::fs::read(&path)
        .await
        .map_err(|_| StatusCode::NOT_FOUND)
}

#[derive(Deserialize)]
struct WsParams {
    token: String,
    #[serde(default)]
    since: i64,
    device: String,
}

async fn ws_handler(
    State(state): State<Arc<AppState>>,
    Query(params): Query<WsParams>,
    ws: WebSocketUpgrade,
) -> Response {
    if params.token != state.token {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    ws.on_upgrade(move |socket| client_loop(socket, state, params.since, params.device))
}

async fn client_loop(mut socket: WebSocket, state: Arc<AppState>, since: i64, device: String) {
    info!("daemon connecté: {device} (since={since})");
    let _guard = ClientGuard::register(Arc::clone(&state), &device);
    // S'abonner AVANT de rejouer le journal, pour ne rien perdre entre les deux.
    let mut rx = state.tx.subscribe();
    let mut last = since;

    let backlog = match state.replay(since) {
        Ok(evs) => evs,
        Err(e) => {
            warn!("replay pour {device}: {e}");
            return;
        }
    };
    for ev in backlog {
        last = ev.seq;
        if ev.origin != device && send_event(&mut socket, &ev).await.is_err() {
            return;
        }
    }

    loop {
        tokio::select! {
            recv = rx.recv() => match recv {
                Ok(ev) => {
                    if ev.seq > last {
                        last = ev.seq;
                        if ev.origin != device && send_event(&mut socket, &ev).await.is_err() {
                            break;
                        }
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    // Trop d'événements en attente : on rattrape via le journal.
                    match state.replay(last) {
                        Ok(evs) => {
                            for ev in evs {
                                last = ev.seq;
                                if ev.origin != device
                                    && send_event(&mut socket, &ev).await.is_err()
                                {
                                    return;
                                }
                            }
                        }
                        Err(e) => {
                            warn!("rattrapage pour {device}: {e}");
                            break;
                        }
                    }
                }
                Err(broadcast::error::RecvError::Closed) => break,
            },
            msg = socket.recv() => match msg {
                Some(Ok(_)) => {} // pings et messages clients ignorés
                _ => break,       // déconnexion
            },
        }
    }
    info!("daemon déconnecté: {device}");
}

async fn send_event(socket: &mut WebSocket, ev: &SyncEvent) -> Result<()> {
    let json = serde_json::to_string(ev)?;
    socket.send(Message::Text(json.into())).await?;
    Ok(())
}

fn open_db(data_dir: &std::path::Path) -> Result<Connection> {
    let conn = Connection::open(data_dir.join("events.db"))?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS events (
             seq     INTEGER PRIMARY KEY AUTOINCREMENT,
             origin  TEXT NOT NULL,
             payload TEXT NOT NULL
         );",
    )?;
    Ok(conn)
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let token = std::env::var("CLIPVAULT_TOKEN")
        .context("CLIPVAULT_TOKEN doit être défini (jeton partagé avec les daemons)")?;
    let listen: SocketAddr = std::env::var("CLIPVAULT_LISTEN")
        .unwrap_or_else(|_| "0.0.0.0:7700".into())
        .parse()
        .context("CLIPVAULT_LISTEN invalide")?;
    let data_dir = std::env::var_os("CLIPVAULT_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
                .join(".local/share/clipvault-server")
        });
    let objects_dir = data_dir.join("objects");
    std::fs::create_dir_all(&objects_dir)?;

    let (tx, _) = broadcast::channel(256);
    let state = Arc::new(AppState {
        db: Mutex::new(open_db(&data_dir)?),
        objects_dir,
        token,
        tx,
        started_at: now(),
        clients: Mutex::new(HashMap::new()),
        next_client_id: AtomicU64::new(1),
    });

    let app = Router::new()
        .route("/v1/health", get(health))
        .route("/v1/status", get(status))
        .route("/v1/push", post(push))
        .route("/v1/objects/{hash}", put(put_object).get(get_object))
        .route("/v1/ws", get(ws_handler))
        .layer(DefaultBodyLimit::max(MAX_BODY))
        .with_state(state);

    info!(
        "clipvault-server écoute sur {listen} (data: {})",
        data_dir.display()
    );
    let listener = tokio::net::TcpListener::bind(listen).await?;
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}
