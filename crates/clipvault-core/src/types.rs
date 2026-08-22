use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContentKind {
    Text,
    Image,
    Binary,
}

impl ContentKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ContentKind::Text => "text",
            ContentKind::Image => "image",
            ContentKind::Binary => "binary",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "text" => Some(ContentKind::Text),
            "image" => Some(ContentKind::Image),
            "binary" => Some(ContentKind::Binary),
            _ => None,
        }
    }
}

/// Métadonnées d'une entrée d'historique, telles qu'échangées entre daemon et UI.
/// Le contenu complet (texte long, blob) est récupéré séparément.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntryMeta {
    /// ULID — unique globalement, ordonné dans le temps (prêt pour la sync v2).
    pub id: String,
    pub device_id: String,
    pub kind: ContentKind,
    pub mime: String,
    pub size: u64,
    /// Aperçu texte tronqué ; libellé synthétique pour image/binaire.
    pub preview: String,
    /// Miniature PNG sur disque, pour les images (chemin local au daemon).
    pub thumb_path: Option<String>,
    /// Epoch secondes.
    pub created_at: i64,
    pub last_used_at: i64,
    pub pinned: bool,
}
