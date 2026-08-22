//! Écoute du presse-papier Wayland via data-control.
//! Essaie `ext-data-control-v1` d'abord, puis `zwlr-data-control-unstable-v1`.

use std::sync::mpsc::Sender;
use std::time::Duration;

use anyhow::{anyhow, Result};
use tracing::{info, warn};
use wayland_clipboard_listener::{
    ClipBoardListenMessage, WlClipboardPasteStream, WlClipboardPasteStreamWlr, WlListenType,
};

/// Ordre de préférence des MIME à récupérer quand plusieurs sont offerts.
const MIME_PRIORITY: &[&str] = &[
    "image/png",
    "image/jpeg",
    "image/webp",
    "image/bmp",
    "text/plain;charset=utf-8",
    "text/plain",
    "UTF8_STRING",
    "STRING",
    "TEXT",
    "text/html",
    "text/uri-list",
];

enum Stream {
    Ext(WlClipboardPasteStream),
    Wlr(WlClipboardPasteStreamWlr),
}

impl Stream {
    fn init() -> Result<Self> {
        let priority: Vec<String> = MIME_PRIORITY.iter().map(|s| s.to_string()).collect();
        match WlClipboardPasteStream::init(WlListenType::ListenOnCopy) {
            Ok(mut s) => {
                info!("watcher: protocole ext-data-control-v1");
                s.set_priority(priority);
                Ok(Stream::Ext(s))
            }
            Err(e) => {
                warn!("ext-data-control indisponible ({e}), bascule sur wlr-data-control");
                let mut s = WlClipboardPasteStreamWlr::init(WlListenType::ListenOnCopy)
                    .map_err(|e| anyhow!("aucun protocole data-control disponible: {e}"))?;
                info!("watcher: protocole zwlr-data-control-unstable-v1");
                s.set_priority(priority);
                Ok(Stream::Wlr(s))
            }
        }
    }

    fn next(&mut self) -> Result<ClipBoardListenMessage> {
        match self {
            Stream::Ext(s) => s.get_clipboard().map_err(|e| anyhow!("{e}")),
            Stream::Wlr(s) => s.get_clipboard().map_err(|e| anyhow!("{e}")),
        }
    }
}

/// Boucle bloquante : pousse chaque nouvelle sélection dans `tx`.
/// À lancer dans un thread dédié.
pub fn run(tx: Sender<ClipBoardListenMessage>) -> Result<()> {
    let mut stream = Stream::init()?;
    let mut consecutive_errors = 0u32;
    loop {
        match stream.next() {
            Ok(msg) => {
                consecutive_errors = 0;
                if tx.send(msg).is_err() {
                    return Ok(()); // l'ingesteur est parti, on s'arrête
                }
            }
            Err(e) => {
                consecutive_errors += 1;
                warn!("watcher: erreur de lecture ({e}), tentative {consecutive_errors}");
                if consecutive_errors > 5 {
                    warn!("watcher: réinitialisation de la connexion Wayland");
                    stream = Stream::init()?;
                    consecutive_errors = 0;
                }
                std::thread::sleep(Duration::from_millis(200));
            }
        }
    }
}
