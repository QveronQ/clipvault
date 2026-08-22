//! Serveur IPC : socket Unix, JSON ligne à ligne (protocole dans clipvault-core).

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::{Arc, Mutex};

use anyhow::{bail, Context, Result};
use clipvault_core::ipc::{Request, Response};
use tracing::{debug, warn};

use crate::clipboard;
use crate::store::Store;

pub fn serve(store: Arc<Mutex<Store>>) -> Result<()> {
    let path = clipvault_core::socket_path();

    // Un seul daemon à la fois : si le socket répond, on refuse de démarrer ;
    // sinon c'est un reste d'un arrêt brutal, on le nettoie.
    if path.exists() {
        if UnixStream::connect(&path).is_ok() {
            bail!("un daemon clipvault écoute déjà sur {}", path.display());
        }
        std::fs::remove_file(&path)?;
    }

    let listener = UnixListener::bind(&path)
        .with_context(|| format!("bind du socket {}", path.display()))?;
    tracing::info!("IPC prêt sur {}", path.display());

    for conn in listener.incoming() {
        match conn {
            Ok(stream) => {
                let store = Arc::clone(&store);
                std::thread::spawn(move || {
                    if let Err(e) = handle_client(stream, store) {
                        debug!("client IPC terminé: {e}");
                    }
                });
            }
            Err(e) => warn!("accept IPC: {e}"),
        }
    }
    Ok(())
}

fn handle_client(stream: UnixStream, store: Arc<Mutex<Store>>) -> Result<()> {
    let mut writer = stream.try_clone()?;
    let reader = BufReader::new(stream);

    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let response = match serde_json::from_str::<Request>(&line) {
            Ok(req) => handle_request(req, &store),
            Err(e) => Response::Error {
                message: format!("requête invalide: {e}"),
            },
        };
        let mut out = serde_json::to_string(&response)?;
        out.push('\n');
        writer.write_all(out.as_bytes())?;
    }
    Ok(())
}

fn handle_request(req: Request, store: &Mutex<Store>) -> Response {
    let result = (|| -> Result<Response> {
        match req {
            Request::Search {
                query,
                limit,
                offset,
                device,
            } => {
                let entries = store.lock().unwrap().search(
                    &query,
                    device.as_deref(),
                    limit.min(500),
                    offset,
                )?;
                Ok(Response::Entries { entries })
            }
            Request::Devices => {
                let devices = store.lock().unwrap().devices()?;
                Ok(Response::Devices { devices })
            }
            Request::Activate { id } => {
                let content = {
                    let s = store.lock().unwrap();
                    let c = s.get_content(&id)?;
                    if c.is_some() {
                        s.touch(&id)?;
                    }
                    c
                };
                match content {
                    Some((mime, data)) => {
                        clipboard::set(&mime, data)?;
                        Ok(Response::Ok)
                    }
                    None => Ok(Response::Error {
                        message: format!("entrée inconnue: {id}"),
                    }),
                }
            }
            Request::Delete { id } => {
                let s = store.lock().unwrap();
                s.delete(&id)?;
                s.enqueue(&clipvault_core::sync::PushItem::Deleted { id })?;
                Ok(Response::Ok)
            }
            Request::SetPinned { id, pinned } => {
                let s = store.lock().unwrap();
                s.set_pinned(&id, pinned)?;
                s.enqueue(&clipvault_core::sync::PushItem::Pinned { id, pinned })?;
                Ok(Response::Ok)
            }
            Request::GetText { id } => {
                let text = store.lock().unwrap().get_text(&id)?.unwrap_or_default();
                Ok(Response::Text { text })
            }
            Request::Stats => {
                let (entries, bytes) = store.lock().unwrap().stats()?;
                Ok(Response::Stats { entries, bytes })
            }
        }
    })();
    result.unwrap_or_else(|e| Response::Error {
        message: e.to_string(),
    })
}
