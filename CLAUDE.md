# clipvault — instructions pour les agents

Gestionnaire d'historique de copier/coller cross-platform (Rust). Deux machines
pour l'instant : `omarchie2` (Arch Linux / Niri, machine principale de Quentin)
et un MacBook Air (`macair`). La sync inter-machines est le but final (v2).

## Conventions

- Code Rust idiomatique, **commentaires et messages de log en français**.
- Zéro warning `cargo clippy` ; tests avec `cargo test` (workspace complet).
- Ne jamais casser la plateforme Linux : tout code spécifique plateforme passe
  par `#[cfg(target_os = ...)]` ou `[target.'cfg(...)'.dependencies]`.
- Pas de `sudo`. Pas de push force. Commits en français, concis.

## Architecture

```
crates/
├── clipvault-core/    # types partagés : EntryMeta, ContentKind, protocole IPC, Config
├── clipvault-daemon/  # daemon résident : capture + stockage + serveur IPC
│   ├── capture.rs     # struct Capture (événement indépendant de la plateforme)
│   ├── watcher.rs     # Linux: data-control (événementiel) / autres: polling arboard
│   ├── clipboard.rs   # recopie: wl-clipboard-rs (Linux) / arboard (macOS, Windows)
│   ├── store.rs       # SQLite + FTS5, dédup BLAKE3, blobs + thumbnails
│   └── ipc.rs         # socket Unix $XDG_RUNTIME_DIR/clipvault.sock, JSON par ligne
└── clipvault-ui/      # binaire `clipvault` : popup egui (thème Catppuccin, theme.rs)
```

- Le **daemon** tourne en permanence, la **UI** est un popup éphémère qui
  interroge le daemon via IPC et se ferme après sélection (`Entrée` recopie).
- **Stockage** : `~/.local/share/clipvault/` (Linux) ou équivalent `dirs::data_dir()`.
  Table `entries` : id **ULID**, `device_id` (hostname), `content_hash` (BLAKE3,
  UNIQUE → dédup), texte inline + FTS5, images/binaires en blobs `objects/<hash>`
  avec thumbnails PNG. Le schéma est DÉJÀ prêt pour la sync (ids globaux, device_id).
- **Config** : `~/.config/clipvault/config.toml` (voir `dist/config.example.toml`).

## Commandes

```sh
cargo build --release          # binaires: target/release/{clipvault-daemon,clipvault}
cargo test && cargo clippy     # doit passer sans warning
./target/release/clipvault-daemon   # daemon au premier plan (logs via RUST_LOG)
./target/release/clipvault          # popup (nécessite le daemon)
```

Test IPC sans UI :
```sh
printf '{"cmd":"stats"}\n' | python3 -c "
import socket,sys,os
p=os.environ.get('XDG_RUNTIME_DIR','/tmp')+'/clipvault.sock'
s=socket.socket(socket.AF_UNIX); s.connect(p); s.sendall(sys.stdin.buffer.read()); print(s.recv(65536).decode())"
```

## État macOS (mission de l'agent côté Mac)

Le backend macOS (`watcher.rs` module `polled`, `clipboard.rs` branche arboard)
a été écrit **sans pouvoir être compilé ni testé sur macOS**. À faire :

1. `cargo build --release` — corriger les éventuelles erreurs de compilation
   (elles seront probablement dans les branches `cfg(not(target_os = "linux"))`).
2. Tester la capture (texte puis image) et la recopie (`Activate` via l'UI).
3. Points d'attention connus :
   - le polling relit l'image du presse-papier à chaque tick (500 ms) tant
     qu'une image y reste — optimisation prévue : `NSPasteboard.changeCount`
     via `objc2-app-kit` (ne déclencher `get_image` que si le count a changé) ;
   - pas de détection « password manager » sur macOS pour l'instant
     (équivalent : type `org.nspasteboard.ConcealedType`) ;
   - `socket_path()` retombe sur `env::temp_dir()` (pas de `XDG_RUNTIME_DIR`) ;
   - lancement au démarrage : prévoir un plist `launchd` dans `dist/` (équivalent
     du `dist/clipvault-daemon.service` systemd) ;
   - l'UI eframe doit fonctionner telle quelle ; vérifier le rendu de la fenêtre
     sans décorations + transparence, et adapter `theme.rs` si les fontes
     candidates n'existent pas (macOS : ajouter p.ex. `/System/Library/Fonts/SFNS.ttf`
     en tête de `FONT_CANDIDATES` — garder les fallbacks egui sinon).

Chaque correction doit rester compatible Linux (ne pas toucher aux branches
Linux sans nécessité). Commits + push sur `main` (repo : `QveronQ/clipvault`).

## Design de la sync (v2 — NE PAS improviser autre chose)

Topologie **client-serveur, offline-first**. Pas de peer-to-peer.

- Un serveur central (`clipvault-server`, axum, à créer) sur le tailnet
  Headscale de Quentin (`*.ts.qdev.ovh`) — hébergeable au début sur omarchie2,
  à terme sur une machine toujours allumée ou le cloud.
- Chaque daemon est un client : il **pousse** ses nouvelles entrées (métadonnées
  + blob) et **reçoit** celles des autres machines (WebSocket ou long-poll).
- Le SQLite local reste la source de vérité locale : une machine hors-ligne
  garde son historique et rattrape à la reconnexion (les entrées sont immuables,
  identifiées par ULID + content_hash → pas de conflits ; épinglage/suppression
  propagés comme événements, last-write-wins).
- Les types de `clipvault-core::ipc` servent de base au protocole ; l'auth v1 :
  token partagé (le réseau est déjà privé/wireguard via le tailnet).

## Contexte session Linux (ne concerne pas l'agent Mac)

Sur omarchie2 : daemon installé en service systemd user (`clipvault-daemon`),
binaires dans `~/.local/bin/`, bind Niri `Mod+V`. Une entrée de test avec
`device_id='macair'` a été injectée à la main dans la base locale (id
`01TESTMACA1R0000000000TEST`) pour tester le filtre par machine — elle pourra
être supprimée via l'UI (`Suppr`).
