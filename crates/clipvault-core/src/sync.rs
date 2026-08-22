//! Protocole de synchronisation daemon <-> serveur (v2).
//!
//! Topologie client-serveur, offline-first :
//! - chaque daemon POSTe ses nouveautés (`PushItem`) sur `/v1/push` ;
//! - les blobs (image/binaire) sont poussés à part sur `PUT /v1/objects/<hash>`
//!   et récupérés à la demande sur `GET /v1/objects/<hash>` ;
//! - chaque daemon reçoit le flux des autres machines via la WebSocket
//!   `GET /v1/ws?token=..&since=<seq>&device=<id>` (un `SyncEvent` JSON par message).
//!
//! Le serveur assigne un numéro de séquence monotone (`seq`) à chaque événement ;
//! le client persiste le dernier `seq` appliqué et reprend là où il en était.

use serde::{Deserialize, Serialize};

use crate::types::EntryMeta;

/// Une entrée complète telle qu'échangée avec le serveur.
/// Le texte voyage inline ; les contenus image/binaire passent par le store
/// d'objets (adressés par `meta.content_hash` côté serveur... voir `object_hash`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncEntry {
    pub meta: EntryMeta,
    /// Contenu texte inline (kind == Text).
    pub text: Option<String>,
    /// Hash du blob à récupérer sur /v1/objects/<hash> (kind != Text).
    pub object_hash: Option<String>,
}

/// Ce qu'un daemon pousse au serveur.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PushItem {
    Entry(SyncEntry),
    Deleted { id: String },
    Pinned { id: String, pinned: bool },
}

impl PushItem {
    /// Machine d'origine de l'événement (pour ne pas se le renvoyer).
    pub fn device_id(&self) -> Option<&str> {
        match self {
            PushItem::Entry(e) => Some(&e.meta.device_id),
            _ => None,
        }
    }
}

/// Ce que le serveur diffuse aux daemons.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncEvent {
    /// Séquence monotone assignée par le serveur (curseur de reprise).
    pub seq: i64,
    /// Machine à l'origine de l'événement.
    pub origin: String,
    #[serde(flatten)]
    pub item: PushItem,
}

/// Réponse de POST /v1/push.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PushAck {
    pub seq: i64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ContentKind;

    #[test]
    fn event_round_trip() {
        let ev = SyncEvent {
            seq: 42,
            origin: "omarchie2".into(),
            item: PushItem::Entry(SyncEntry {
                meta: EntryMeta {
                    id: "01ABC".into(),
                    device_id: "omarchie2".into(),
                    kind: ContentKind::Text,
                    mime: "text/plain".into(),
                    size: 5,
                    preview: "hello".into(),
                    thumb_path: None,
                    created_at: 1,
                    last_used_at: 1,
                    pinned: false,
                },
                text: Some("hello".into()),
                object_hash: None,
            }),
        };
        let json = serde_json::to_string(&ev).unwrap();
        let back: SyncEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(back.seq, 42);
        assert!(matches!(back.item, PushItem::Entry(ref e) if e.text.as_deref() == Some("hello")));

        let del = serde_json::to_string(&SyncEvent {
            seq: 43,
            origin: "macair".into(),
            item: PushItem::Deleted { id: "01ABC".into() },
        })
        .unwrap();
        let back: SyncEvent = serde_json::from_str(&del).unwrap();
        assert!(matches!(back.item, PushItem::Deleted { .. }));
    }
}
