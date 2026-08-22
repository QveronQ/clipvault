use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Taille max d'un élément capturé, en octets.
    pub max_item_bytes: u64,
    /// Rétention des entrées non épinglées, en jours.
    pub retention_days: u32,
    /// Nombre max d'entrées non épinglées conservées.
    pub max_entries: u32,
    /// Ignorer les copies marquées confidentielles (x-kde-passwordManagerHint).
    pub ignore_password_hint: bool,
    /// Répertoire de données (défaut : ~/.local/share/clipvault).
    pub data_dir: Option<PathBuf>,
    /// Synchronisation avec un serveur clipvault (absent = pas de sync).
    pub sync: Option<SyncConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncConfig {
    /// URL de base du serveur, ex. "http://merlin.ts.qdev.ovh:7700".
    pub server: String,
    /// Jeton partagé (doit correspondre à celui du serveur).
    pub token: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            max_item_bytes: 10 * 1024 * 1024,
            retention_days: 90,
            max_entries: 10_000,
            ignore_password_hint: true,
            data_dir: None,
            sync: None,
        }
    }
}

impl Config {
    /// Chemins candidats du fichier de config, par ordre de priorité.
    ///
    /// `~/.config/clipvault/config.toml` d'abord : c'est le chemin documenté et
    /// le seul sur Linux (`dirs::config_dir()` y pointe déjà). Sur macOS
    /// `dirs::config_dir()` vaut `~/Library/Application Support`, qu'on garde en
    /// second pour respecter la convention système.
    pub fn config_candidates() -> Vec<PathBuf> {
        let mut out = Vec::new();
        let xdg = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| dirs::home_dir().map(|h| h.join(".config")));
        if let Some(dir) = xdg {
            out.push(dir.join("clipvault/config.toml"));
        }
        if let Some(dir) = dirs::config_dir() {
            let p = dir.join("clipvault/config.toml");
            if !out.contains(&p) {
                out.push(p);
            }
        }
        out
    }

    /// Charge `~/.config/clipvault/config.toml`, ou les valeurs par défaut si absent.
    pub fn load() -> Self {
        let Some(path) = Self::config_candidates()
            .into_iter()
            .find(|p| p.is_file())
        else {
            return Self::default();
        };
        match std::fs::read_to_string(&path) {
            Ok(raw) => toml::from_str(&raw).unwrap_or_else(|e| {
                eprintln!("clipvault: config invalide ({}): {e}", path.display());
                Self::default()
            }),
            Err(_) => Self::default(),
        }
    }

    pub fn data_dir(&self) -> PathBuf {
        self.data_dir.clone().unwrap_or_else(|| {
            dirs::data_dir().unwrap_or_default().join("clipvault")
        })
    }
}
