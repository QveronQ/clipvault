//! Événement de capture indépendant de la plateforme.

/// Une nouvelle sélection du presse-papier, telle que remontée par le
/// watcher de la plateforme (Wayland data-control sur Linux, polling
/// NSPasteboard/Win32 via arboard ailleurs).
pub struct Capture {
    /// MIME effectivement récupéré.
    pub mime: String,
    /// Tous les MIME offerts par la source (vide si la plateforme ne les expose pas).
    pub mime_types: Vec<String>,
    pub data: Vec<u8>,
}
