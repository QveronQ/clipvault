use std::path::{Path, PathBuf};

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
    /// Suivi souris/clavier Logitech Easy-Switch (absent = désactivé).
    pub logitech: Option<LogiConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncConfig {
    /// URL de base du serveur, ex. "http://merlin.ts.qdev.ovh:7700".
    pub server: String,
    /// Jeton partagé (doit correspondre à celui du serveur).
    pub token: String,
}

/// Quand le clavier Logitech (Easy-Switch) arrive sur cette machine, la
/// machine qui tient encore la souris reçoit l'info via le serveur de sync
/// et lui envoie un Change Host (HID++ 0x1814) vers `mouse_host`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogiConfig {
    /// Numéro Easy-Switch (1-3) de CETTE machine sur la souris.
    pub mouse_host: u8,
    /// Sous-chaîne du nom du clavier (ex. "MX Keys") ; défaut : par type.
    #[serde(default)]
    pub keyboard: Option<String>,
    /// Sous-chaîne du nom de la souris (ex. "MX Master") ; défaut : par type.
    #[serde(default)]
    pub mouse: Option<String>,
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
            logitech: None,
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

    /// Fichier de config à écrire : celui qui existe déjà (pour ne pas en créer
    /// un second qui masquerait le premier), sinon le candidat prioritaire.
    pub fn config_write_path() -> Option<PathBuf> {
        let candidates = Self::config_candidates();
        candidates
            .iter()
            .find(|p| p.is_file())
            .cloned()
            .or_else(|| candidates.into_iter().next())
    }

    /// Écrit la section `[sync]` dans le fichier de config, en préservant le
    /// reste du document (autres clés et commentaires compris) ; crée le
    /// fichier s'il n'existe pas. Renvoie le chemin écrit.
    pub fn save_sync(sync: &SyncConfig) -> Result<PathBuf, String> {
        let path = Self::config_write_path()
            .ok_or_else(|| "aucun répertoire de configuration utilisable".to_string())?;
        Self::save_sync_at(&path, sync)?;
        Ok(path)
    }

    /// Idem `save_sync`, sur un chemin imposé.
    pub fn save_sync_at(path: &Path, sync: &SyncConfig) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("création de {}: {e}", parent.display()))?;
        }
        let raw = std::fs::read_to_string(path).unwrap_or_default();
        let merged = merge_sync(&raw, sync)
            .map_err(|e| format!("config existante illisible ({}): {e}", path.display()))?;

        std::fs::write(path, merged)
            .map_err(|e| format!("écriture de {}: {e}", path.display()))?;
        // Le jeton est un secret : lisible par le seul propriétaire.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        }
        Ok(())
    }

    pub fn data_dir(&self) -> PathBuf {
        self.data_dir.clone().unwrap_or_else(|| {
            dirs::data_dir().unwrap_or_default().join("clipvault")
        })
    }
}

/// Insère ou remplace la section `[sync]` d'un document TOML, en laissant tout
/// le reste intact (clés inconnues, ordre, commentaires).
fn merge_sync(raw: &str, sync: &SyncConfig) -> Result<String, toml_edit::TomlError> {
    use toml_edit::{value, DocumentMut, Item, Table};

    let mut doc: DocumentMut = raw.parse()?;
    if !doc.contains_key("sync") {
        doc["sync"] = Item::Table(Table::new());
    }
    match doc["sync"].as_table_mut() {
        Some(table) => {
            table["server"] = value(sync.server.trim());
            table["token"] = value(sync.token.trim());
        }
        None => {
            // « sync » existe mais n'est pas une section : on la remplace.
            let mut table = Table::new();
            table["server"] = value(sync.server.trim());
            table["token"] = value(sync.token.trim());
            doc["sync"] = Item::Table(table);
        }
    }
    Ok(doc.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_sync_preserves_other_keys_and_comments() {
        let raw = "# mon commentaire\nretention_days = 7\nmax_entries = 42\n";
        let out = merge_sync(
            raw,
            &SyncConfig {
                server: "  http://omarchie2:7700  ".into(),
                token: "  secret  ".into(),
            },
        )
        .unwrap();
        assert!(out.contains("# mon commentaire"), "commentaire perdu: {out}");
        assert!(out.contains("retention_days = 7"));
        assert!(out.contains("max_entries = 42"));

        let cfg: Config = toml::from_str(&out).unwrap();
        assert_eq!(cfg.retention_days, 7);
        assert_eq!(cfg.max_entries, 42);
        let s = cfg.sync.unwrap();
        // Les espaces autour des valeurs saisies sont éliminés.
        assert_eq!(s.server, "http://omarchie2:7700");
        assert_eq!(s.token, "secret");
    }

    #[test]
    fn merge_sync_replaces_existing_section() {
        let raw = "[sync]\nserver = \"http://vieux:1\"\ntoken = \"vieux\"\n";
        let out = merge_sync(
            raw,
            &SyncConfig {
                server: "http://neuf:7700".into(),
                token: "neuf".into(),
            },
        )
        .unwrap();
        assert!(!out.contains("vieux"), "ancienne valeur conservée: {out}");
        let s = toml::from_str::<Config>(&out).unwrap().sync.unwrap();
        assert_eq!(s.server, "http://neuf:7700");
        assert_eq!(s.token, "neuf");
    }

    #[test]
    fn merge_sync_creates_section_in_empty_file() {
        let out = merge_sync(
            "",
            &SyncConfig {
                server: "http://x:7700".into(),
                token: "t".into(),
            },
        )
        .unwrap();
        let s = toml::from_str::<Config>(&out).unwrap().sync.unwrap();
        assert_eq!(s.server, "http://x:7700");
    }

    #[test]
    fn save_sync_at_creates_dirs_and_merges() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sous/dossier/config.toml");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "# garde-moi\nmax_entries = 5\n").unwrap();

        Config::save_sync_at(
            &path,
            &SyncConfig {
                server: "http://s:7700".into(),
                token: "tok".into(),
            },
        )
        .unwrap();

        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("# garde-moi"), "{raw}");
        let cfg: Config = toml::from_str(&raw).unwrap();
        assert_eq!(cfg.max_entries, 5);
        assert_eq!(cfg.sync.as_ref().unwrap().token, "tok");

        // Réécriture : la section est remplacée, pas dupliquée.
        Config::save_sync_at(
            &path,
            &SyncConfig {
                server: "http://s2:7700".into(),
                token: "tok2".into(),
            },
        )
        .unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        assert_eq!(raw.matches("[sync]").count(), 1, "{raw}");
        let cfg: Config = toml::from_str(&raw).unwrap();
        assert_eq!(cfg.sync.as_ref().unwrap().server, "http://s2:7700");
        assert_eq!(cfg.max_entries, 5);
    }

    #[test]
    fn save_sync_at_creates_missing_file_and_parents() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("neuf/config.toml");
        Config::save_sync_at(
            &path,
            &SyncConfig {
                server: "http://s:7700".into(),
                token: "tok".into(),
            },
        )
        .unwrap();
        assert!(path.is_file());
        let cfg: Config = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(cfg.sync.unwrap().server, "http://s:7700");
    }

    #[cfg(unix)]
    #[test]
    fn save_sync_at_restricts_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        Config::save_sync_at(
            &path,
            &SyncConfig {
                server: "http://s:7700".into(),
                token: "tok".into(),
            },
        )
        .unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "le jeton doit rester privé (mode {mode:o})");
    }

    #[test]
    fn config_candidates_all_point_at_clipvault_config() {
        let candidates = Config::config_candidates();
        assert!(!candidates.is_empty());
        assert!(candidates
            .iter()
            .all(|p| p.ends_with("clipvault/config.toml")));
    }
}
