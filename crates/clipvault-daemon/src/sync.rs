//! Client de synchronisation : deux threads bloquants.
//! - `run_push` : vide l'outbox vers le serveur (REST), avec retry.
//! - `run_recv` : reçoit le flux des autres machines (WebSocket) et l'applique.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clipvault_core::config::SyncConfig;
use clipvault_core::sync::{PushItem, SyncEvent};
use tracing::{debug, info, warn};

use crate::store::Store;

const RETRY_DELAY: Duration = Duration::from_secs(5);
const IDLE_DELAY: Duration = Duration::from_secs(2);

fn bearer(cfg: &SyncConfig) -> String {
    format!("Bearer {}", cfg.token)
}

/// Pousse l'outbox vers le serveur, indéfiniment.
pub fn run_push(store: Arc<Mutex<Store>>, cfg: SyncConfig, device_id: String) {
    info!("sync: push vers {}", cfg.server);
    loop {
        let batch = match store.lock().unwrap().outbox_peek(20) {
            Ok(b) => b,
            Err(e) => {
                warn!("sync: lecture outbox: {e}");
                std::thread::sleep(RETRY_DELAY);
                continue;
            }
        };
        if batch.is_empty() {
            std::thread::sleep(IDLE_DELAY);
            continue;
        }
        for (seq, item) in batch {
            match push_one(&store, &cfg, &device_id, &item) {
                Ok(()) => {
                    debug!("sync: outbox {seq} poussé");
                    if let Err(e) = store.lock().unwrap().outbox_remove(seq) {
                        warn!("sync: purge outbox {seq}: {e}");
                    }
                }
                Err(e) => {
                    // Serveur injoignable ou refus : on réessaiera tout à l'heure.
                    warn!("sync: push en attente ({e})");
                    std::thread::sleep(RETRY_DELAY);
                    break;
                }
            }
        }
    }
}

fn push_one(
    store: &Arc<Mutex<Store>>,
    cfg: &SyncConfig,
    device_id: &str,
    item: &PushItem,
) -> Result<()> {
    // Blob d'abord : le serveur doit l'avoir avant de diffuser l'entrée.
    if let PushItem::Entry(entry) = item {
        if let Some(hash) = &entry.object_hash {
            let data = store.lock().unwrap().object_data(hash)?;
            match data {
                Some(data) => {
                    ureq::put(&format!("{}/v1/objects/{hash}", cfg.server))
                        .header("Authorization", &bearer(cfg))
                        .send(&data[..])
                        .context("PUT objet")?;
                }
                None => bail!("blob local {hash} introuvable"),
            }
        }
    }
    ureq::post(&format!("{}/v1/push", cfg.server))
        .header("Authorization", &bearer(cfg))
        .header("X-Device", device_id)
        .send_json(item)
        .context("POST push")?;
    Ok(())
}

/// Reçoit et applique le flux du serveur, indéfiniment (reconnexion auto).
/// `connected` reflète l'état de la connexion (pour l'IPC SyncStatus).
pub fn run_recv(
    store: Arc<Mutex<Store>>,
    cfg: SyncConfig,
    device_id: String,
    connected: Arc<std::sync::atomic::AtomicBool>,
) {
    use std::sync::atomic::Ordering;
    loop {
        if let Err(e) = recv_session(&store, &cfg, &device_id, &connected) {
            warn!("sync: connexion au flux perdue ({e}), retry dans 5 s");
        }
        connected.store(false, Ordering::Relaxed);
        std::thread::sleep(RETRY_DELAY);
    }
}

fn recv_session(
    store: &Arc<Mutex<Store>>,
    cfg: &SyncConfig,
    device_id: &str,
    connected: &std::sync::atomic::AtomicBool,
) -> Result<()> {
    let since = store.lock().unwrap().last_seq()?;
    let ws_base = cfg
        .server
        .replacen("http://", "ws://", 1)
        .replacen("https://", "wss://", 1);
    let url = format!(
        "{ws_base}/v1/ws?token={}&since={since}&device={device_id}",
        cfg.token
    );
    let (mut ws, _) = tungstenite::connect(&url).context("connexion WebSocket")?;
    connected.store(true, std::sync::atomic::Ordering::Relaxed);
    info!("sync: connecté au flux (since={since})");

    loop {
        match ws.read()? {
            tungstenite::Message::Text(txt) => {
                let event: SyncEvent = match serde_json::from_str(txt.as_str()) {
                    Ok(ev) => ev,
                    Err(e) => {
                        warn!("sync: événement illisible: {e}");
                        continue;
                    }
                };
                apply_event(store, cfg, event);
            }
            tungstenite::Message::Close(_) => bail!("fermé par le serveur"),
            _ => {} // ping/pong gérés par tungstenite
        }
    }
}

fn apply_event(store: &Arc<Mutex<Store>>, cfg: &SyncConfig, event: SyncEvent) {
    let seq = event.seq;
    let result = (|| -> Result<()> {
        match event.item {
            PushItem::Entry(entry) => {
                let data = match &entry.object_hash {
                    Some(hash) => Some(fetch_object(cfg, hash)?),
                    None => None,
                };
                store
                    .lock()
                    .unwrap()
                    .apply_remote_entry(&entry, data.as_deref())?;
                info!(
                    "sync: reçu {} de {} ({})",
                    entry.meta.id, entry.meta.device_id, entry.meta.mime
                );
            }
            PushItem::Deleted { id } => {
                store.lock().unwrap().delete(&id)?;
                info!("sync: suppression propagée ({id})");
            }
            PushItem::Pinned { id, pinned } => {
                store.lock().unwrap().set_pinned(&id, pinned)?;
                info!("sync: épinglage propagé ({id} -> {pinned})");
            }
        }
        Ok(())
    })();
    match result {
        // Curseur avancé même sur erreur d'application : un événement cassé ne
        // doit pas bloquer le flux pour toujours (il resurgirait à chaque
        // reconnexion). Les entrées sont immuables, on ne perd que celle-là.
        Ok(()) | Err(_) => {
            if let Err(e) = &result {
                warn!("sync: application de l'événement {seq}: {e}");
            }
            if let Err(e) = store.lock().unwrap().set_last_seq(seq) {
                warn!("sync: sauvegarde du curseur {seq}: {e}");
            }
        }
    }
}

fn fetch_object(cfg: &SyncConfig, hash: &str) -> Result<Vec<u8>> {
    let mut resp = ureq::get(&format!("{}/v1/objects/{hash}", cfg.server))
        .header("Authorization", &bearer(cfg))
        .call()
        .context("GET objet")?;
    let data = resp
        .body_mut()
        .with_config()
        .limit(64 * 1024 * 1024)
        .read_to_vec()
        .context("lecture objet")?;
    Ok(data)
}
