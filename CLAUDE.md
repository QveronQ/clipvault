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

## État macOS — VOIR `docs/macos.md`

**Le rapport complet est dans [`docs/macos.md`](docs/macos.md)** : ce qui a été
vérifié sur le MacBook Air, les pièges macOS rencontrés, les bugs corrigés dans
le code partagé et ce qui reste à faire. À lire avant de toucher à
`logi.rs`, `config.rs` ou au protocole de sync.

En bref : capture, recopie, sync bidirectionnelle, écran de connexion,
Easy-Switch Logitech et packagement (`.app` + LaunchAgent) sont **opérationnels
et vérifiés**. Le backend macOS a compilé sans retouche ; les vrais problèmes
étaient ailleurs. Trois points touchent du code partagé et méritent ton
attention :

- `dirs::config_dir()` vaut `~/Library/Application Support` sur macOS, donc
  `~/.config/clipvault/config.toml` n'était jamais lu et **la sync restait
  désactivée en silence**. `Config::config_candidates()` essaie XDG/`~/.config`
  d'abord — inchangé sous Linux.
- `logi.rs`, chemin « appairage direct » (`dev_idx 0xFF`) : trois bugs, dont
  clavier et souris **intervertis** (le type était passé en dur, `dtype == 0`
  toujours vrai). Détail et conséquences pour Linux dans le rapport.
- Un daemon plus ancien que le serveur **perd silencieusement** les événements
  `PushItem` qu'il ne connaît pas (constaté avec `keyboard_here`). Aucune
  négociation de version aujourd'hui.

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

### Bouton de bascule sur la souris (nouveau — à valider côté Mac)

Le bouton pouce de la MX Master (CID 0xC3) est détourné via la feature 0x1b04
(`setCidReporting`, flags divert) ; le daemon lit les notifications
`divertedButtonsEvent` et déclenche une bascule LOCALE clavier+souris vers
`toggle_host` (défaut : l'autre entre 1 et 2). Le divert est re-posé à chaque
retour de la souris (il ne survit pas au changement d'hôte). Config :
`button_cid` / `toggle_host` dans `[logitech]`. `--logi-probe` liste les CID.
Vérifié côté récepteur Bolt (Linux). Côté Mac (appairage direct) : la lecture
des notifications passe par le handle persistant `mouse_handle` — la souris
s'ouvre sans problème sur macOS d'après docs/macos.md, mais le chemin n'a pas
encore été exécuté là-bas : à valider (un appui pouce → tout part vers l'Arch).

### Bascule depuis le Mac — synthèse (autorisations macOS)

Modèle établi (vérifié sur le Mac, en deux passes) : macOS n'a qu'une porte
vers un périphérique HID, `IOHIDDeviceOpen`, qui ouvre lecture ET écriture
indissociablement — impossible de « seulement écrire ». La protection porte
sur l'ouverture, pas sur le sens du trafic. Le point décisif est le CONTEXTE
du processus : lancé depuis un terminal interactif, l'open de la souris passe ;
un service **launchd n'hérite d'aucune autorisation** et a besoin de la sienne
propre (« Surveillance de l'entrée » pour le binaire du daemon).
`ProcessType: Background` n'y est pour rien (testé, retiré car il bride).

- **Souris : faisable.** Résoudre par product_id au moment de l'envoi (le
  registry ID change à chaque reconnexion), ouvrir en NON exclusif
  (`HidApi::set_open_exclusive(false)`) — sans lien avec TCC, mais ça évite de
  confisquer la souris au système (cause du gel constaté). La signature ad-hoc
  change à chaque rebuild → retirer/re-cocher l'entrée dans Réglages puis
  redémarrer le LaunchAgent. Piste contre cette friction : signer avec une
  identité stable (certificat auto-signé dans le trousseau).
- **Clavier : impossible même avec l'autorisation** (kIOReturnNotPrivileged,
  protection noyau anti-keylogger ; seul root passe). Ne pas insister ; un
  LaunchDaemon root dédié serait la seule voie, non retenue.
- **Design retenu pour Mac → Linux** : le raccourci Mac ne bascule que la
  souris ; le trajet complet passe par le bouton canal physique du clavier +
  le flux automatique KeyboardHere qui ramène la souris (déjà fonctionnel).
  Le bouton pouce côté Mac redevient envisageable grâce au non-exclusif —
  toujours conditionné à l'autorisation TCC du daemon.

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
