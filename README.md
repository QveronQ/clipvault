# clipvault

Historique de copier/coller cross-platform (texte, image, binaire) avec indexation
et recherche plein texte. v1 : Linux/Wayland (Niri). v2 prévue : sync inter-machines
(serveur axum), macOS, Windows.

## Architecture

- **`clipvault-daemon`** — tourne en permanence (service systemd user) :
  - capture du presse-papier via `ext-data-control-v1` (fallback `zwlr-data-control`) ;
  - stockage SQLite + FTS5 dans `~/.local/share/clipvault/` ;
  - déduplication par hash BLAKE3, blobs images/binaires sur disque, thumbnails ;
  - serveur IPC sur `$XDG_RUNTIME_DIR/clipvault.sock` (JSON ligne à ligne).
- **`clipvault`** (UI) — popup egui éphémère lancé par raccourci : recherche
  incrémentale, navigation clavier, recopie dans le presse-papier.
- **`clipvault-core`** — types partagés (entrées, protocole IPC, config).

## Build & installation

```sh
cargo build --release
install -Dm755 target/release/clipvault-daemon ~/.local/bin/clipvault-daemon
install -Dm755 target/release/clipvault ~/.local/bin/clipvault

# Service systemd user
install -Dm644 dist/clipvault-daemon.service ~/.config/systemd/user/clipvault-daemon.service
systemctl --user daemon-reload
systemctl --user enable --now clipvault-daemon

# Config optionnelle
install -Dm644 dist/config.example.toml ~/.config/clipvault/config.toml
```

Puis intégrer `dist/niri-snippet.kdl` dans `~/.config/niri/config.kdl`
(bind `Mod+V` + window rule flottante).

## Raccourcis du popup

| Touche | Action |
|---|---|
| taper du texte | recherche plein texte (FTS5, préfixe sur le dernier mot) |
| `↑` / `↓` | naviguer |
| `Entrée` / clic | recopier l'entrée et fermer |
| `Ctrl+P` | épingler / désépingler (exclut de la purge) |
| `Suppr` | supprimer l'entrée |
| `Échap` | fermer |

## Captures ignorées

- copies marquées `x-kde-passwordManagerHint` (KeePassXC & co) ;
- éléments au-delà de `max_item_bytes` (10 Mo par défaut).

## Purge

Toutes les heures : suppression des entrées non épinglées plus vieilles que
`retention_days` ou au-delà de `max_entries`.
