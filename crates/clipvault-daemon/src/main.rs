mod capture;
mod clipboard;
mod ipc;
mod logi;
mod store;
mod sync;
mod watcher;

use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use clipvault_core::config::Config;
use clipvault_core::types::ContentKind;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

use store::Store;

const PASSWORD_HINT_MIME: &str = "x-kde-passwordManagerHint";

fn classify(mime: &str) -> ContentKind {
    let lower = mime.to_ascii_lowercase();
    if lower.starts_with("text/")
        || matches!(mime, "UTF8_STRING" | "STRING" | "TEXT" | "COMPOUND_TEXT")
    {
        ContentKind::Text
    } else if lower.starts_with("image/") {
        ContentKind::Image
    } else {
        ContentKind::Binary
    }
}

fn main() -> Result<()> {
    // Diagnostic : `clipvault-daemon --logi-probe` liste les périphériques
    // Logitech visibles (slots du récepteur, noms, support Change Host).
    if std::env::args().any(|a| a == "--logi-probe") {
        let cfg = Config::load().logitech.unwrap_or(
            clipvault_core::config::LogiConfig {
                mouse_host: 1,
                keyboard: None,
                mouse: None,
            },
        );
        print!("{}", logi::Engine::probe(cfg)?);
        return Ok(());
    }

    // Diagnostic : `clipvault-daemon --logi-switch N` envoie la souris vers
    // l'hôte Easy-Switch N (1-3). Sert à vérifier le Change Host sans monter
    // tout le mécanisme de détection du clavier.
    if let Some(host) = std::env::args()
        .skip_while(|a| a != "--logi-switch")
        .nth(1)
        .and_then(|a| a.parse::<u8>().ok())
    {
        let cfg = Config::load().logitech.unwrap_or(clipvault_core::config::LogiConfig {
            mouse_host: 1,
            keyboard: None,
            mouse: None,
        });
        let mut engine = logi::Engine::open(cfg)?;
        engine.switch_mouse(host)?;
        println!("Change Host envoyé: souris -> hôte {host}");
        return Ok(());
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cfg = Config::load();
    // CLIPVAULT_DEVICE permet de forcer l'identifiant machine (tests, alias).
    let device_id = std::env::var("CLIPVAULT_DEVICE").unwrap_or_else(|_| {
        hostname::get()
            .map(|h| h.to_string_lossy().into_owned())
            .unwrap_or_else(|_| "unknown".into())
    });
    match Config::config_candidates().into_iter().find(|p| p.is_file()) {
        Some(p) => info!("config chargée: {}", p.display()),
        None => info!("aucun config.toml trouvé, valeurs par défaut"),
    }
    info!(
        "clipvault-daemon démarre (device: {device_id}, data: {})",
        cfg.data_dir().display()
    );

    let store = Arc::new(Mutex::new(Store::open(cfg.clone(), device_id.clone())?));

    // Thread watcher Wayland -> canal -> thread d'ingestion.
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("wl-watcher".into())
        .spawn(move || {
            if let Err(e) = watcher::run(tx) {
                // Sans watcher le daemon n'a plus de raison d'être : on sort,
                // systemd (Restart=on-failure) relancera.
                eprintln!("watcher fatal: {e}");
                std::process::exit(1);
            }
        })?;

    let ingest_store = Arc::clone(&store);
    let ingest_cfg = cfg.clone();
    std::thread::Builder::new()
        .name("ingest".into())
        .spawn(move || {
            for msg in rx {
                let mime = msg.mime;
                let data = msg.data;

                if data.is_empty() {
                    continue;
                }
                if data.len() as u64 > ingest_cfg.max_item_bytes {
                    info!("capture ignorée: {} octets > max_item_bytes", data.len());
                    continue;
                }
                if ingest_cfg.ignore_password_hint
                    && msg
                        .mime_types
                        .iter()
                        .any(|m| m.eq_ignore_ascii_case(PASSWORD_HINT_MIME))
                {
                    info!("capture ignorée: marquée confidentielle (password manager)");
                    continue;
                }

                let kind = classify(&mime);
                match ingest_store.lock().unwrap().insert(kind, &mime, &data) {
                    Ok(id) => info!("capturé {} ({mime}, {} octets)", id, data.len()),
                    Err(e) => warn!("échec d'ingestion: {e}"),
                }
            }
        })?;

    // Suivi souris/clavier Logitech, si configuré.
    let logi_tx = if let Some(logi_cfg) = cfg.logitech.clone() {
        let (tx, rx) = mpsc::channel();
        let logi_store = Arc::clone(&store);
        let logi_device = device_id.clone();
        std::thread::Builder::new()
            .name("logitech".into())
            .spawn(move || logi::run(logi_store, logi_cfg, logi_device, rx))?;
        Some(tx)
    } else {
        None
    };

    // Synchronisation avec le serveur, si configurée.
    let sync_connected = Arc::new(std::sync::atomic::AtomicBool::new(false));
    if let Some(sync_cfg) = cfg.sync.clone() {
        let push_store = Arc::clone(&store);
        let (push_cfg, push_device) = (sync_cfg.clone(), device_id.clone());
        std::thread::Builder::new()
            .name("sync-push".into())
            .spawn(move || sync::run_push(push_store, push_cfg, push_device))?;
        let recv_store = Arc::clone(&store);
        let recv_connected = Arc::clone(&sync_connected);
        let recv_device = device_id.clone();
        std::thread::Builder::new()
            .name("sync-recv".into())
            .spawn(move || {
                sync::run_recv(recv_store, sync_cfg, recv_device, recv_connected, logi_tx)
            })?;
    }

    // Purge périodique des vieilles entrées.
    let purge_store = Arc::clone(&store);
    std::thread::Builder::new()
        .name("purge".into())
        .spawn(move || loop {
            match purge_store.lock().unwrap().purge() {
                Ok(0) => {}
                Ok(n) => info!("purge: {n} entrées supprimées"),
                Err(e) => warn!("purge: {e}"),
            }
            std::thread::sleep(Duration::from_secs(3600));
        })?;

    // Serveur IPC sur le thread principal (bloquant).
    ipc::serve(
        store,
        ipc::SyncCtx {
            cfg: cfg.sync.clone(),
            connected: sync_connected,
        },
    )
}
