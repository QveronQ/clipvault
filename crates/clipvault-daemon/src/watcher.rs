//! Écoute du presse-papier système, par plateforme.
//!
//! - Linux : événementiel via data-control (`ext-data-control-v1`, fallback
//!   `zwlr-data-control-unstable-v1`).
//! - macOS / Windows : polling via arboard (pas d'événement système exposé).

use std::sync::mpsc::Sender;

use anyhow::Result;

use crate::capture::Capture;

#[cfg(target_os = "linux")]
pub use linux::run;
#[cfg(not(target_os = "linux"))]
pub use polled::run;

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use std::time::Duration;

    use anyhow::anyhow;
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
    pub fn run(tx: Sender<Capture>) -> Result<()> {
        let mut stream = Stream::init()?;
        let mut consecutive_errors = 0u32;
        loop {
            match stream.next() {
                Ok(msg) => {
                    consecutive_errors = 0;
                    let capture = Capture {
                        mime: msg.context.mime_type,
                        mime_types: msg.mime_types,
                        data: msg.context.context,
                    };
                    if tx.send(capture).is_err() {
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
}

#[cfg(not(target_os = "linux"))]
mod polled {
    use super::*;
    use std::io::Cursor;
    use std::time::Duration;

    use anyhow::Context as _;
    use tracing::{info, warn};

    const POLL_INTERVAL: Duration = Duration::from_millis(500);

    /// Boucle bloquante : polling du presse-papier via arboard.
    /// TODO v2 : utiliser NSPasteboard.changeCount (objc2) pour éviter de
    /// relire les images à chaque tick.
    pub fn run(tx: Sender<Capture>) -> Result<()> {
        let mut clipboard =
            arboard::Clipboard::new().context("initialisation du presse-papier arboard")?;
        info!("watcher: polling arboard ({} ms)", POLL_INTERVAL.as_millis());
        let mut last_hash: Option<blake3::Hash> = None;

        loop {
            std::thread::sleep(POLL_INTERVAL);

            // Texte d'abord (lecture peu coûteuse).
            if let Ok(text) = clipboard.get_text() {
                if !text.is_empty() {
                    let hash = blake3::hash(text.as_bytes());
                    if last_hash != Some(hash) {
                        last_hash = Some(hash);
                        let capture = Capture {
                            mime: "text/plain;charset=utf-8".into(),
                            mime_types: vec!["text/plain;charset=utf-8".into()],
                            data: text.into_bytes(),
                        };
                        if tx.send(capture).is_err() {
                            return Ok(());
                        }
                    }
                    continue;
                }
            }

            // Pas de texte : peut-être une image.
            match clipboard.get_image() {
                Ok(img) => {
                    let hash = blake3::hash(&img.bytes);
                    if last_hash == Some(hash) {
                        continue;
                    }
                    last_hash = Some(hash);
                    let (w, h) = (img.width as u32, img.height as u32);
                    let Some(rgba) = image::RgbaImage::from_raw(w, h, img.bytes.into_owned())
                    else {
                        warn!("watcher: image presse-papier invalide ({w}x{h})");
                        continue;
                    };
                    let mut png = Vec::new();
                    if let Err(e) = image::DynamicImage::ImageRgba8(rgba)
                        .write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)
                    {
                        warn!("watcher: encodage PNG échoué: {e}");
                        continue;
                    }
                    let capture = Capture {
                        mime: "image/png".into(),
                        mime_types: vec!["image/png".into()],
                        data: png,
                    };
                    if tx.send(capture).is_err() {
                        return Ok(());
                    }
                }
                Err(arboard::Error::ContentNotAvailable) => {}
                Err(e) => warn!("watcher: lecture presse-papier: {e}"),
            }
        }
    }
}
