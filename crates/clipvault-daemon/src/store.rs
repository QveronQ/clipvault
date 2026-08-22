use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use clipvault_core::config::Config;
use clipvault_core::sync::{PushItem, SyncEntry};
use clipvault_core::types::{ContentKind, EntryMeta};
use rusqlite::{params, Connection, OptionalExtension, Row};

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS entries (
    id            TEXT NOT NULL UNIQUE,
    device_id     TEXT NOT NULL,
    content_hash  TEXT NOT NULL UNIQUE,
    kind          TEXT NOT NULL,
    mime          TEXT NOT NULL,
    size          INTEGER NOT NULL,
    text_content  TEXT,
    object_path   TEXT,
    thumb_path    TEXT,
    preview       TEXT NOT NULL,
    created_at    INTEGER NOT NULL,
    last_used_at  INTEGER NOT NULL,
    pinned        INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_entries_last_used ON entries(pinned DESC, last_used_at DESC);

CREATE VIRTUAL TABLE IF NOT EXISTS entries_fts USING fts5(
    text_content,
    content='entries',
    content_rowid='rowid'
);

CREATE TRIGGER IF NOT EXISTS entries_ai AFTER INSERT ON entries BEGIN
    INSERT INTO entries_fts(rowid, text_content)
    VALUES (new.rowid, coalesce(new.text_content, ''));
END;
CREATE TRIGGER IF NOT EXISTS entries_ad AFTER DELETE ON entries BEGIN
    INSERT INTO entries_fts(entries_fts, rowid, text_content)
    VALUES ('delete', old.rowid, coalesce(old.text_content, ''));
END;

-- File d'attente de sync sortante (offline-first) et curseur de réception.
CREATE TABLE IF NOT EXISTS outbox (
    seq     INTEGER PRIMARY KEY AUTOINCREMENT,
    payload TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS sync_state (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
"#;

pub struct Store {
    conn: Connection,
    cfg: Config,
    device_id: String,
    objects_dir: PathBuf,
    thumbs_dir: PathBuf,
}

/// Résultat de l'écriture du contenu d'une entrée.
#[derive(Default)]
struct Payload {
    text_content: Option<String>,
    object_path: Option<String>,
    thumb_path: Option<String>,
    preview: String,
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Aperçu monoligne tronqué pour du texte.
fn text_preview(text: &str) -> String {
    let flat: String = text
        .trim()
        .chars()
        .map(|c| if c == '\n' || c == '\t' { ' ' } else { c })
        .take(200)
        .collect();
    flat
}

impl Store {
    pub fn open(cfg: Config, device_id: String) -> Result<Self> {
        let data_dir = cfg.data_dir();
        let objects_dir = data_dir.join("objects");
        let thumbs_dir = data_dir.join("thumbs");
        std::fs::create_dir_all(&objects_dir)?;
        std::fs::create_dir_all(&thumbs_dir)?;

        let conn = Connection::open(data_dir.join("clipvault.db"))
            .context("ouverture de la base SQLite")?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.execute_batch(SCHEMA)?;

        Ok(Self {
            conn,
            cfg,
            device_id,
            objects_dir,
            thumbs_dir,
        })
    }

    pub fn device_id(&self) -> &str {
        &self.device_id
    }

    /// Ingestion d'une capture. Retourne l'id (existant si dédupliqué, sinon nouveau).
    pub fn insert(&mut self, kind: ContentKind, mime: &str, data: &[u8]) -> Result<String> {
        let hash = blake3::hash(data).to_hex().to_string();
        let ts = now();

        if let Some(id) = self
            .conn
            .query_row(
                "SELECT id FROM entries WHERE content_hash = ?1",
                [&hash],
                |r| r.get::<_, String>(0),
            )
            .optional()?
        {
            self.conn.execute(
                "UPDATE entries SET last_used_at = ?1 WHERE id = ?2",
                params![ts, id],
            )?;
            return Ok(id);
        }

        let id = ulid::Ulid::generate().to_string();
        let payload = self.write_payload(kind, mime, &hash, data)?;

        self.conn.execute(
            "INSERT INTO entries (id, device_id, content_hash, kind, mime, size,
                                  text_content, object_path, thumb_path, preview,
                                  created_at, last_used_at, pinned)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?11, 0)",
            params![
                id,
                self.device_id,
                hash,
                kind.as_str(),
                mime,
                data.len() as i64,
                payload.text_content,
                payload.object_path,
                payload.thumb_path,
                payload.preview,
                ts,
            ],
        )?;

        // Sync : mettre la nouvelle entrée dans la file sortante.
        if self.cfg.sync.is_some() {
            let entry = SyncEntry {
                meta: EntryMeta {
                    id: id.clone(),
                    device_id: self.device_id.clone(),
                    kind,
                    mime: mime.to_string(),
                    size: data.len() as u64,
                    preview: payload.preview,
                    thumb_path: None, // chemin local, chaque machine régénère
                    created_at: ts,
                    last_used_at: ts,
                    pinned: false,
                },
                text: payload.text_content,
                object_hash: (kind != ContentKind::Text).then(|| hash.clone()),
            };
            self.enqueue(&PushItem::Entry(entry))?;
        }
        Ok(id)
    }

    /// Écrit le contenu (texte inline / blob + thumbnail) et calcule l'aperçu.
    fn write_payload(
        &self,
        kind: ContentKind,
        mime: &str,
        hash: &str,
        data: &[u8],
    ) -> Result<Payload> {
        let mut payload = Payload::default();
        match kind {
            ContentKind::Text => {
                let text = String::from_utf8_lossy(data).into_owned();
                payload.preview = text_preview(&text);
                payload.text_content = Some(text);
            }
            ContentKind::Image => {
                let obj = self.objects_dir.join(hash);
                std::fs::write(&obj, data)?;
                payload.object_path = Some(obj.to_string_lossy().into_owned());
                match image::load_from_memory(data) {
                    Ok(img) => {
                        let (w, h) = (img.width(), img.height());
                        let thumb = img.thumbnail(256, 256);
                        let tp = self.thumbs_dir.join(format!("{hash}.png"));
                        if thumb.to_rgba8().save(&tp).is_ok() {
                            payload.thumb_path = Some(tp.to_string_lossy().into_owned());
                        }
                        payload.preview = format!("Image {w}×{h}");
                    }
                    Err(_) => {
                        payload.preview = format!("Image ({mime})");
                    }
                }
            }
            ContentKind::Binary => {
                let obj = self.objects_dir.join(hash);
                std::fs::write(&obj, data)?;
                payload.object_path = Some(obj.to_string_lossy().into_owned());
                payload.preview = format!("{mime} — {} octets", data.len());
            }
        }
        Ok(payload)
    }

    /// Applique une entrée reçue du serveur (id, device et horodatages préservés).
    /// `data` : blob téléchargé pour les entrées non-texte.
    pub fn apply_remote_entry(&mut self, entry: &SyncEntry, data: Option<&[u8]>) -> Result<()> {
        let m = &entry.meta;
        let exists: Option<String> = self
            .conn
            .query_row("SELECT id FROM entries WHERE id = ?1", [&m.id], |r| {
                r.get(0)
            })
            .optional()?;
        let bytes;
        let (hash, data) = match (&entry.text, data) {
            (Some(t), _) => {
                bytes = t.clone().into_bytes();
                (blake3::hash(&bytes).to_hex().to_string(), bytes.as_slice())
            }
            (None, Some(d)) => match &entry.object_hash {
                Some(h) => (h.clone(), d),
                None => return Ok(()), // entrée binaire sans hash : illisible
            },
            (None, None) => return Ok(()), // blob indisponible : on ignore
        };
        if exists.is_some()
            || self
                .conn
                .query_row(
                    "SELECT 1 FROM entries WHERE content_hash = ?1",
                    [&hash],
                    |_| Ok(()),
                )
                .optional()?
                .is_some()
        {
            return Ok(()); // déjà connue (id ou même contenu)
        }

        let payload = self.write_payload(m.kind, &m.mime, &hash, data)?;
        self.conn.execute(
            "INSERT INTO entries (id, device_id, content_hash, kind, mime, size,
                                  text_content, object_path, thumb_path, preview,
                                  created_at, last_used_at, pinned)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![
                m.id,
                m.device_id,
                hash,
                m.kind.as_str(),
                m.mime,
                data.len() as i64,
                payload.text_content,
                payload.object_path,
                payload.thumb_path,
                payload.preview,
                m.created_at,
                m.last_used_at,
                m.pinned as i64,
            ],
        )?;
        Ok(())
    }

    // ---- File d'attente de sync sortante ----

    /// Ajoute un événement à pousser vers le serveur (no-op sans sync configurée).
    pub fn enqueue(&self, item: &PushItem) -> Result<()> {
        if self.cfg.sync.is_none() {
            return Ok(());
        }
        self.conn.execute(
            "INSERT INTO outbox (payload) VALUES (?1)",
            [serde_json::to_string(item)?],
        )?;
        Ok(())
    }

    pub fn outbox_peek(&self, limit: u32) -> Result<Vec<(i64, PushItem)>> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT seq, payload FROM outbox ORDER BY seq LIMIT ?1")?;
        let rows = stmt.query_map([limit], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (seq, payload) = row?;
            match serde_json::from_str(&payload) {
                Ok(item) => out.push((seq, item)),
                Err(_) => {
                    // Entrée illisible : on la retire pour ne pas bloquer la file.
                    self.conn.execute("DELETE FROM outbox WHERE seq = ?1", [seq])?;
                }
            }
        }
        Ok(out)
    }

    pub fn outbox_remove(&self, seq: i64) -> Result<()> {
        self.conn.execute("DELETE FROM outbox WHERE seq = ?1", [seq])?;
        Ok(())
    }

    /// Dernier seq serveur appliqué (curseur de réception).
    pub fn last_seq(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row(
                "SELECT value FROM sync_state WHERE key = 'last_seq'",
                [],
                |r| r.get::<_, String>(0),
            )
            .optional()?
            .and_then(|v| v.parse().ok())
            .unwrap_or(0))
    }

    pub fn set_last_seq(&self, seq: i64) -> Result<()> {
        self.conn.execute(
            "INSERT INTO sync_state (key, value) VALUES ('last_seq', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [seq.to_string()],
        )?;
        Ok(())
    }

    /// Contenu brut d'un blob local (pour le pousser au serveur).
    pub fn object_data(&self, hash: &str) -> Result<Option<Vec<u8>>> {
        let path = self.objects_dir.join(hash);
        match std::fs::read(&path) {
            Ok(d) => Ok(Some(d)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    fn row_to_meta(row: &Row<'_>) -> rusqlite::Result<EntryMeta> {
        Ok(EntryMeta {
            id: row.get(0)?,
            device_id: row.get(1)?,
            kind: ContentKind::parse(&row.get::<_, String>(2)?).unwrap_or(ContentKind::Binary),
            mime: row.get(3)?,
            size: row.get::<_, i64>(4)? as u64,
            preview: row.get(5)?,
            thumb_path: row.get(6)?,
            created_at: row.get(7)?,
            last_used_at: row.get(8)?,
            pinned: row.get::<_, i64>(9)? != 0,
        })
    }

    const META_COLS: &'static str =
        "id, device_id, kind, mime, size, preview, thumb_path, created_at, last_used_at, pinned";

    pub fn search(
        &self,
        query: &str,
        device: Option<&str>,
        limit: u32,
        offset: u32,
    ) -> Result<Vec<EntryMeta>> {
        let query = query.trim();
        let mut out = Vec::new();
        if query.is_empty() {
            let mut stmt = self.conn.prepare_cached(&format!(
                "SELECT {} FROM entries
                 WHERE (?3 IS NULL OR device_id = ?3)
                 ORDER BY pinned DESC, last_used_at DESC LIMIT ?1 OFFSET ?2",
                Self::META_COLS
            ))?;
            let rows = stmt.query_map(params![limit, offset, device], Self::row_to_meta)?;
            for r in rows {
                out.push(r?);
            }
        } else {
            let fts = build_fts_query(query);
            let mut stmt = self.conn.prepare_cached(&format!(
                "SELECT {} FROM entries e
                 JOIN entries_fts f ON f.rowid = e.rowid
                 WHERE entries_fts MATCH ?1 AND (?4 IS NULL OR e.device_id = ?4)
                 ORDER BY e.pinned DESC, e.last_used_at DESC LIMIT ?2 OFFSET ?3",
                Self::META_COLS
                    .split(", ")
                    .map(|c| format!("e.{c}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ))?;
            let rows = stmt.query_map(params![fts, limit, offset, device], Self::row_to_meta)?;
            for r in rows {
                out.push(r?);
            }
        }
        Ok(out)
    }

    /// Machines présentes dans l'historique, la plus récemment active d'abord.
    pub fn devices(&self) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT device_id FROM entries
             GROUP BY device_id ORDER BY MAX(last_used_at) DESC",
        )?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Contenu complet (mime, octets) pour recopie dans le presse-papier.
    pub fn get_content(&self, id: &str) -> Result<Option<(String, Vec<u8>)>> {
        let row = self
            .conn
            .query_row(
                "SELECT mime, text_content, object_path FROM entries WHERE id = ?1",
                [id],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, Option<String>>(1)?,
                        r.get::<_, Option<String>>(2)?,
                    ))
                },
            )
            .optional()?;
        let Some((mime, text, obj)) = row else {
            return Ok(None);
        };
        let data = match (text, obj) {
            (Some(t), _) => t.into_bytes(),
            (None, Some(p)) => std::fs::read(p)?,
            (None, None) => Vec::new(),
        };
        Ok(Some((mime, data)))
    }

    pub fn get_text(&self, id: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT text_content FROM entries WHERE id = ?1",
                [id],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten())
    }

    pub fn touch(&self, id: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE entries SET last_used_at = ?1 WHERE id = ?2",
            params![now(), id],
        )?;
        Ok(())
    }

    pub fn set_pinned(&self, id: &str, pinned: bool) -> Result<()> {
        self.conn.execute(
            "UPDATE entries SET pinned = ?1 WHERE id = ?2",
            params![pinned as i64, id],
        )?;
        Ok(())
    }

    pub fn delete(&self, id: &str) -> Result<()> {
        let paths = self
            .conn
            .query_row(
                "SELECT object_path, thumb_path FROM entries WHERE id = ?1",
                [id],
                |r| {
                    Ok((
                        r.get::<_, Option<String>>(0)?,
                        r.get::<_, Option<String>>(1)?,
                    ))
                },
            )
            .optional()?;
        self.conn.execute("DELETE FROM entries WHERE id = ?1", [id])?;
        if let Some((obj, thumb)) = paths {
            for p in [obj, thumb].into_iter().flatten() {
                let _ = std::fs::remove_file(p);
            }
        }
        Ok(())
    }

    /// Purge : entrées non épinglées trop vieilles ou au-delà du plafond.
    pub fn purge(&self) -> Result<usize> {
        let cutoff = now() - self.cfg.retention_days as i64 * 86_400;
        let old_ids: Vec<String> = {
            let mut stmt = self.conn.prepare(
                "SELECT id FROM entries WHERE pinned = 0 AND (
                     last_used_at < ?1
                     OR rowid NOT IN (
                         SELECT rowid FROM entries WHERE pinned = 0
                         ORDER BY last_used_at DESC LIMIT ?2
                     )
                 )",
            )?;
            let rows = stmt.query_map(params![cutoff, self.cfg.max_entries], |r| {
                r.get::<_, String>(0)
            })?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        for id in &old_ids {
            self.delete(id)?;
        }
        Ok(old_ids.len())
    }

    pub fn stats(&self) -> Result<(u64, u64)> {
        let (count, bytes): (i64, i64) = self.conn.query_row(
            "SELECT COUNT(*), COALESCE(SUM(size), 0) FROM entries",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        Ok((count as u64, bytes as u64))
    }
}

/// Transforme une requête utilisateur en requête FTS5 sûre :
/// chaque mot entre guillemets (échappés), `*` de préfixe sur le dernier.
fn build_fts_query(query: &str) -> String {
    let tokens: Vec<&str> = query.split_whitespace().collect();
    let n = tokens.len();
    tokens
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let escaped = t.replace('"', "\"\"");
            if i == n - 1 {
                format!("\"{escaped}\"*")
            } else {
                format!("\"{escaped}\"")
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_store() -> (Store, tempfile::TempDir) {
        let dir = tempfile::TempDir::new().unwrap();
        let cfg = Config {
            data_dir: Some(dir.path().to_path_buf()),
            ..Config::default()
        };
        (Store::open(cfg, "test-device".into()).unwrap(), dir)
    }

    #[test]
    fn insert_search_dedup() {
        let (mut store, _dir) = test_store();
        let id1 = store
            .insert(ContentKind::Text, "text/plain;charset=utf-8", b"hello clipvault world")
            .unwrap();
        let id2 = store
            .insert(ContentKind::Text, "text/plain;charset=utf-8", b"autre contenu")
            .unwrap();
        assert_ne!(id1, id2);

        // Dédup : même contenu -> même id
        let id3 = store
            .insert(ContentKind::Text, "text/plain;charset=utf-8", b"hello clipvault world")
            .unwrap();
        assert_eq!(id1, id3);

        let all = store.search("", None, 50, 0).unwrap();
        assert_eq!(all.len(), 2);

        let hits = store.search("clipv", None, 50, 0).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, id1);

        let none = store.search("introuvable", None, 50, 0).unwrap();
        assert!(none.is_empty());

        // Filtre par machine
        assert_eq!(store.devices().unwrap(), vec!["test-device".to_string()]);
        assert_eq!(store.search("", Some("test-device"), 50, 0).unwrap().len(), 2);
        assert!(store.search("", Some("autre"), 50, 0).unwrap().is_empty());
        assert_eq!(store.search("clipv", Some("test-device"), 50, 0).unwrap().len(), 1);
        assert!(store.search("clipv", Some("autre"), 50, 0).unwrap().is_empty());
    }

    #[test]
    fn delete_and_pin() {
        let (mut store, _dir) = test_store();
        let id = store
            .insert(ContentKind::Text, "text/plain", b"a supprimer")
            .unwrap();
        store.set_pinned(&id, true).unwrap();
        assert!(store.search("", None, 10, 0).unwrap()[0].pinned);
        store.delete(&id).unwrap();
        assert!(store.search("", None, 10, 0).unwrap().is_empty());
    }

    #[test]
    fn binary_round_trip() {
        let (mut store, _dir) = test_store();
        let payload = vec![0u8, 159, 146, 150];
        let id = store
            .insert(ContentKind::Binary, "application/octet-stream", &payload)
            .unwrap();
        let (mime, data) = store.get_content(&id).unwrap().unwrap();
        assert_eq!(mime, "application/octet-stream");
        assert_eq!(data, payload);
    }

    #[test]
    fn fts_query_is_safe() {
        assert_eq!(build_fts_query("foo bar"), "\"foo\" \"bar\"*");
        assert_eq!(build_fts_query("l\"esprit"), "\"l\"\"esprit\"*");
    }
}
