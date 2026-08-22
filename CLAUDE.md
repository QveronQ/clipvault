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

## Sync (IMPLÉMENTÉE — client-serveur, offline-first)

- `crates/clipvault-server` : journal d'événements append-only (SQLite) +
  store d'objets. REST (`POST /v1/push`, `PUT|GET /v1/objects/<hash>`) +
  WebSocket de diffusion (`GET /v1/ws?token&since&device`). Protocole dans
  `clipvault-core::sync`. Auth : token partagé (env `CLIPVAULT_TOKEN`),
  pensé pour tourner sur le tailnet Headscale (`*.ts.qdev.ovh`).
- Côté daemon (`sync.rs`) : outbox SQLite (offline-first, retry), thread push
  + thread réception WS (curseur `last_seq` persisté). Dédup par ULID +
  content_hash → pas de conflits ; épinglage/suppression = événements
  last-write-wins. Activée par la section `[sync]` de `config.toml`
  (voir `dist/config.example.toml`).
- Test bout en bout : `crates/clipvault-server/tests/e2e.rs`. Env utiles :
  `CLIPVAULT_SOCKET` (chemin du socket IPC), `CLIPVAULT_DEVICE` (force
  l'identifiant machine) — permettent plusieurs daemons sur une même machine.

## Mission en cours côté Mac : écran de connexion (UI)

En plus de la mission macOS ci-dessus, l'agent Mac implémente l'écran de
connexion au serveur dans `clipvault-ui`. Spécification :

- Point d'entrée : l'écran **Gestion** (engrenage `⚙`, `main.rs::draw_manage`).
  Quand `sync_cfg` est `None`, remplacer le message statique par un petit
  formulaire : champ URL (prérempli avec le serveur local détecté par
  `probe_local` s'il y en a un), champ token, bouton « Tester » (GET
  `/v1/status` avec le token → affiche version/machines ou l'erreur), bouton
  « Enregistrer ».
- « Enregistrer » écrit la section `[sync]` dans `~/.config/clipvault/config.toml`
  en PRÉSERVANT les autres clés existantes (relire le fichier, ne pas l'écraser
  aveuglément). Ajouter `Config::save_sync(&SyncConfig)` dans
  `clipvault-core::config` avec un test.
- Après enregistrement : message « redémarre le daemon pour activer la sync »
  (le daemon ne recharge pas sa config à chaud pour l'instant).
- Style : réutiliser `theme.rs` et les patterns existants (`kv`, `section_title`).
  Textes en français. Ne pas toucher au protocole ni au serveur.
- Optionnel (si le reste est fait) : découverte mDNS. **L'annonce côté serveur
  est FAITE** (`_clipvault._tcp.local.`, instance = hostname, TXT `version`,
  désactivable par `CLIPVAULT_MDNS=0` ; crate `mdns-sd`). Reste la partie UI :
  browse `_clipvault._tcp.local.` avec `mdns-sd` pendant que l'écran de
  connexion est ouvert, proposer les instances résolues pour préremplir l'URL
  (`http://<instance>.local:<port>`, ou une adresse IPv4 résolue ; ignorer les
  IPv6 link-local des interfaces virtuelles type veth Docker).

Pendant cette mission, l'agent Linux ne touche pas à `clipvault-ui` ni à
`clipvault-core::config` (éviter les conflits).

**Règle de coordination (deux agents sur le même repo)** : commit + push
IMMÉDIATEMENT après chaque changement cohérent qui compile — ne jamais
accumuler de travail local. `git pull --rebase` avant chaque push et avant de
commencer un chantier. Jamais de force push. Objectif : faire émerger les
conflits le plus tôt possible, quand ils sont encore petits.

## Logitech Easy-Switch (IMPLÉMENTÉ côté Linux, à tester côté Mac)

`clipvault-daemon/src/logi.rs` : quand le clavier Logitech arrive sur une
machine (ping HID++ des slots du récepteur, 1 Hz), elle publie l'événement
sync `KeyboardHere{mouse_host}` ; la machine qui tient encore la souris le
reçoit (< 15 s, anti-rejeu) et envoie Change Host (feature 0x1814). Activation
par la section `[logitech]` (voir `dist/config.example.toml`). Diagnostic :
`clipvault-daemon --logi-probe`. Vérifié sur récepteur Bolt Linux
(MX Keys S + MX Master 3S, détection par type de la feature 0x0005).

Côté Mac (pour l'agent Mac) : le chemin « périphérique direct Bluetooth »
(device index 0xFF, usage page 0xFF43) est écrit mais JAMAIS testé — à valider
avec `--logi-probe`. Si le Mac utilise aussi un récepteur Bolt, le chemin
récepteur (testé) s'applique tel quel. hidapi sur macOS peut demander
l'autorisation « Input Monitoring » (Réglages > Confidentialité).

### Backlog v2.x
- macOS : `NSPasteboard.changeCount` (éviter la relecture d'images au polling),
  type `org.nspasteboard.ConcealedType`, plist launchd.
- Statut de sync dans l'UI (connecté/hors-ligne, taille de l'outbox — la
  requête IPC `stats` peut s'enrichir).
- Chiffrement E2E (le serveur ne verrait que des blobs) si serveur cloud.
- Switch de canal Logitech (HID++ via `hidapi`).

## Contexte session Linux (ne concerne pas l'agent Mac)

Sur omarchie2 : daemon installé en service systemd user (`clipvault-daemon`),
binaires dans `~/.local/bin/`, bind Niri `Mod+V`. Une entrée de test avec
`device_id='macair'` a été injectée à la main dans la base locale (id
`01TESTMACA1R0000000000TEST`) pour tester le filtre par machine — elle pourra
être supprimée via l'UI (`Suppr`).
