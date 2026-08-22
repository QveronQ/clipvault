//! Protocole IPC daemon <-> UI : JSON, un message par ligne, sur socket Unix.
//! Ces types serviront de base au protocole de sync v2.

use serde::{Deserialize, Serialize};

use crate::types::EntryMeta;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Request {
    /// Recherche plein texte (FTS5). Query vide = entrées récentes.
    Search {
        query: String,
        limit: u32,
        offset: u32,
        /// Restreindre à une machine (device_id). None = toutes.
        #[serde(default)]
        device: Option<String>,
    },
    /// Liste des machines (device_id) présentes dans l'historique.
    Devices,
    /// Recopie l'entrée dans le presse-papier système.
    Activate { id: String },
    Delete { id: String },
    SetPinned { id: String, pinned: bool },
    /// Contenu texte complet d'une entrée (aperçu détaillé côté UI).
    GetText { id: String },
    Stats,
    /// État de la synchronisation du daemon.
    SyncStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum Response {
    Entries { entries: Vec<EntryMeta> },
    Devices { devices: Vec<String> },
    Text { text: String },
    Stats { entries: u64, bytes: u64 },
    SyncStatus {
        /// Identifiant de cette machine.
        device: String,
        /// Une section [sync] est-elle configurée ?
        enabled: bool,
        /// URL du serveur configuré.
        server: Option<String>,
        /// Le flux WebSocket est-il actuellement connecté ?
        connected: bool,
        /// Événements locaux en attente d'envoi.
        outbox: u64,
        /// Curseur de réception (dernier seq serveur appliqué).
        last_seq: i64,
    },
    Ok,
    Error { message: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_round_trip() {
        let req = Request::Search {
            query: "hello".into(),
            limit: 50,
            offset: 0,
            device: Some("omarchie2".into()),
        };
        let json = serde_json::to_string(&req).unwrap();
        let back: Request = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, Request::Search { ref query, .. } if query == "hello"));

        // Compat : un client sans le champ device reste valide.
        let legacy: Request =
            serde_json::from_str(r#"{"cmd":"search","query":"x","limit":1,"offset":0}"#).unwrap();
        assert!(matches!(legacy, Request::Search { device: None, .. }));
    }

    #[test]
    fn response_error_round_trip() {
        let json = serde_json::to_string(&Response::Error {
            message: "boom".into(),
        })
        .unwrap();
        let back: Response = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, Response::Error { ref message } if message == "boom"));
    }
}
