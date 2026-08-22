# clipvault sur macOS — rapport pour l'agent Linux

Machine : MacBook Air (Apple Silicon), macOS 26.3, `device_id` = `MacBookAir`.
Tout ce qui suit a été **exécuté et vérifié** sur cette machine, sauf mention
explicite du contraire.

## Résumé

| Domaine | État |
|---|---|
| Build workspace complet (serveur compris) | ✅ aucune erreur, aucun warning nouveau |
| `cargo test` (dont l'e2e de sync) | ✅ 15 tests |
| Capture presse-papier texte / image | ✅ |
| Recopie (`Activate`) texte / image | ✅ |
| Sync bidirectionnelle avec omarchie2 | ✅ dans les deux sens |
| Écran de connexion au serveur (UI) | ✅ |
| Easy-Switch Logitech en Bluetooth direct | ✅ clavier ET souris pilotables, suivi automatique local |
| Bundle `.app` + LaunchAgent | ✅ |
| Bouton pouce de la souris (divert) | ❌ impossible sur macOS, désactivé — voir plus bas |

Le backend macOS (`watcher.rs::polled`, `clipboard.rs` branche arboard) a
compilé **du premier coup**, sans retouche. Les vrais problèmes étaient
ailleurs.

## Pièges macOS rencontrés (les branches `cfg` n'y suffisaient pas)

### `dirs::config_dir()` ne pointe pas vers `~/.config`

Le plus coûteux, parce qu'il échouait **en silence**. Sur macOS
`dirs::config_dir()` vaut `~/Library/Application Support`, donc
`~/.config/clipvault/config.toml` n'était jamais lu : le daemon démarrait sur
les valeurs par défaut et la sync restait désactivée sans un mot dans les logs.

`Config::config_candidates()` essaie désormais `$XDG_CONFIG_HOME` / `~/.config`
d'abord, puis `dirs::config_dir()`. **Comportement inchangé sous Linux**, où
`dirs::config_dir()` est déjà `~/.config`. Le daemon logue maintenant le
fichier retenu au démarrage — à garder, c'est ce qui rend ce genre de panne
visible.

À noter : `dirs::data_dir()` reste `~/Library/Application Support/clipvault`,
et c'est très bien ainsi. La config et les données ne sont donc pas au même
endroit sur macOS ; c'est voulu.

### Les fontes

Les chemins de `FONT_CANDIDATES` sont tous des chemins Linux. Ajout de
`/System/Library/Fonts/SFNS.ttf` et `Apple Symbols.ttf` en tête — les chemins
absents sont simplement sautés, donc rien ne change de ton côté.

### `socket_path()`

Pas de `XDG_RUNTIME_DIR` sur macOS : on retombe sur `env::temp_dir()`, soit
`$TMPDIR` (`/var/folders/…`), qui est privé à l'utilisateur. Fonctionne tel
quel, rien à changer.

## Logitech Easy-Switch — trois bugs dans le chemin « appairage direct »

Ce Mac n'a **pas** de récepteur Bolt : MX Keys S et MX Master 3S sont appairés
en Bluetooth direct. C'est le chemin `dev_idx 0xFF` qui n'avait jamais été
exécuté. Il ne fonctionnait pas.

### 1. Clavier et souris intervertis (`c365fc1`)

Sans récepteur, pas de `getDeviceType`, et `scan()` passait les types en dur :

```rust
if keyboard.is_none() && self.matches_keyboard(&name, 0) {      // <- 0 en dur
} else if mouse.is_none() && self.matches_mouse(&name, 3) {     // <- 3 en dur
```

Or `matches_keyboard` teste `dtype == 0` : **toujours vrai** quand `[logitech]`
ne nomme pas les appareils. Le premier périphérique HID++ énuméré devenait donc
« le clavier ». Ici la souris sortait en premier : un Change Host serait parti
au clavier.

Le type se déduit maintenant des autres interfaces HID du même `product_id`
(Generic Desktop, usage 6 = clavier, 2 = souris). **Si la plateforme ne
renseigne pas les usages HID, on n'attribue plus rien** plutôt que de deviner —
il faut alors nommer les appareils dans `[logitech]`. À vérifier de ton côté :
hidraw sous Linux renseigne-t-il `usage_page`/`usage` ? Si non, le chemin
direct y exigera une config explicite. Ça ne touche pas le chemin récepteur,
qui reste prioritaire et inchangé.

### 2. Le clavier : c'est l'ouverture EXCLUSIVE qui est refusée

**Cette section disait d'abord « le clavier est impossible sans root ». C'était
faux, et la correction vaut d'être lue.**

Ouvrir un clavier échoue bien avec `kIOReturnNotPrivileged (0xE00002C1)`,
sur toutes ses interfaces, et l'autorisation « Saisie de contenu » n'y change
rien — d'où la conclusion hâtive. Mais hidapi ouvre **en exclusif par défaut**,
et c'est cela que macOS refuse sur un clavier : lui céder le périphérique
priverait le système de tes frappes.

Avec `HidApi::set_open_exclusive(false)`, le clavier s'ouvre **normalement** :

```
Clavier (direct): ping ok, change-host oui (index 10), hôte 2/3
```

et le Change Host part sans difficulté — vérifié, le clavier quitte le Mac pour
l'autre machine. Ni root, ni entitlement, ni LaunchDaemon privilégié.

Ce qui reste vrai : la présence par énumération (piège suivant) fonctionne
toujours et ne demande aucune autorisation ; elle reste le moyen le plus léger
de savoir *où* est le clavier.

### 3. Le chemin du périphérique est périmé dès la première bascule (`fd34548`)

Sur macOS, le chemin hidapi est un registry ID (`DevSrvsID:4295039304`) qui
**change à chaque reconnexion Bluetooth**. Constaté : après un aller-retour du
clavier, les deux chemins avaient changé. `switch_mouse` ouvrait donc un chemin
mort dès que la souris s'était reconnectée une fois — bug invisible tant qu'on
ne teste pas deux bascules d'affilée.

`Target::Direct` porte maintenant le `product_id` (stable), et le chemin est
re-résolu juste avant l'envoi. Si les chemins hidraw sont stables sous Linux, ce
correctif n'y change rien ; il ne coûte qu'une recherche dans l'énumération.

### Validation de bout en bout

`--logi-switch N` (ajouté, `4f51afd`) envoie la souris vers un hôte sans monter
tout le mécanisme — pratique pour isoler le Change Host.

Séquence complète observée, sans intervention :

```
logitech: actif (mouse_host de cette machine: 2)
logitech: clavier localisé: Some(direct pid 0xb378)   <- MX KEYS S, le bon
logitech: clavier détecté ici, on rapatrie la souris
```
→ la souris est revenue de l'Arch au Mac toute seule. Dans l'autre sens,
`--logi-switch 1` l'a bien envoyée sur l'Arch (confirmé de visu là-bas).

`--logi-probe` détaille désormais les périphériques directs : ping, index de la
feature Change Host, hôte courant. Sortie actuelle :

```
Clavier: Some(direct pid 0xb378)
Souris: Some(direct pid 0xb034)
  Clavier (direct): connecté, non ouvrable (protégé par macOS) — présence détectée par énumération
  Souris (direct): ping ok, change-host oui (index 10), hôte 2/3
```

### 4. Le bouton pouce confisque la souris (divert 0x1b04)

**À lire avant de retoucher au divert.** Sur macOS, hidapi ouvre un
périphérique en accès **exclusif**. Le mécanisme du bouton de bascule garde un
handle permanent sur la souris pour lire les `divertedButtonsEvent` : la souris
est alors confisquée au système, qui ne la voit plus bouger. Symptôme observé
côté utilisateur : *« elle affiche bien le canal 2 mais ne fonctionne pas »* —
le canal est correct, le pointeur est mort.

Signature au diagnostic, à ne pas confondre avec le cas du clavier :

```
souris  : 0xE00002C5  exclusive access and device already open
clavier : 0xE00002C1  privilege violation
```

Le `0xE00002C5` sur la souris **désigne toujours un autre process qui la tient**
— très souvent un second `clipvault-daemon` resté en arrière-plan. La souris
redevient normale dès que le handle est rendu (vérifié : `pkill` du daemon,
puis ouverture immédiatement possible).

Conséquence : `button_cid()` renvoie `None` sur macOS quel que soit le CID
configuré, et le daemon avertit au démarrage si la config en demandait un. Le
divert reste pleinement disponible sous Linux, où il est vérifié.

Si quelqu'un veut ressusciter la fonctionnalité côté Mac, il faudra une autre
voie que le handle hidapi persistant (IOHIDManager en mode non-exclusif, ou un
tap sur les événements) — non exploré.

### 5. Réinstaller le binaire invalide l'autorisation « Saisie de contenu »

Le piège le plus vicieux des cinq, parce qu'il **se manifeste à retardement et
que le panneau Réglages ment**. macOS attache l'autorisation à un binaire
précis ; `install`-er une nouvelle version au même chemin la casse, mais
l'entrée **reste affichée et cochée** dans Confidentialité > Saisie de contenu.
Tout a l'air en ordre, et le daemon ne peut plus ouvrir la souris.

Symptôme : la bascule marche dans le sens Arch → Mac (c'est l'Arch qui agit),
mais pas Mac → Arch — le Mac n'arrive plus à envoyer le Change Host. Dans les
logs :

```
sync: clavier arrivé sur omarchie2, souris -> hôte 1
WARN logitech: change host: … (0xE00002E2) (iokit/common) not permitted
```

Remède : **retirer puis rajouter** l'entrée (la simple case à décocher/recocher
ne suffit pas). À refaire après chaque déploiement du daemon —
`install-launchagent.sh` le rappelle désormais en fin d'installation.

Attention au diagnostic : lancer `--logi-probe` **depuis un terminal** ne
reproduit pas le problème, le terminal ayant sa propre autorisation. Le probe
dit alors « ping ok, change-host oui » pendant que le daemon échoue. Il faut
comparer les deux contextes.

### Les trois refus d'ouverture HID de macOS

Ils se ressemblent et n'ont rien à voir. `explain_hid_error()` les traduit dans
les logs :

| Code | Cause | Remède |
|---|---|---|
| `0xE00002E2` not permitted | autorisation « Saisie de contenu » absente ou périmée | retirer/rajouter le binaire |
| `0xE00002C5` exclusive access | un autre process tient l'appareil | tuer le second daemon |
| `0xE00002C1` privilege violation | ouverture **exclusive** d'un clavier | ouvrir en non exclusif |

### Testé et écarté : « on veut seulement écrire, pas surveiller »

Objection légitime — le Change Host est une écriture de 3 octets, on ne lit
jamais rien de la souris. Pourquoi faudrait-il une autorisation de
*surveillance* ?

Parce que macOS n'offre qu'une porte, `IOHIDDeviceOpen`, qui donne lecture et
écriture indissociablement ; la protection porte sur l'ouverture, pas sur
l'usage. Vérifié : ouverture **non exclusive** (`set_open_exclusive(false)`),
aucune lecture demandée, une seule écriture → même `0xE00002E2`. Inutile de
retenter.

`set_open_exclusive(false)` est conservé pour autre chose : il empêche le
daemon de confisquer la souris au système (piège 4).

## Point d'attention sur le protocole de sync

Relevé dans les logs pendant que mon daemon était en retard d'une version :

```
sync: événement illisible: unknown variant `keyboard_here`,
expected one of `entry`, `deleted`, `pinned`
```

Sans gravité immédiate — `apply_event` avance le curseur même en erreur, donc
le flux ne se bloque pas. Mais **un daemon plus ancien que le serveur perd
silencieusement les événements qu'il ne connaît pas**, et rien ne le signale
côté serveur. À garder en tête pour les prochaines extensions de `PushItem` :
il n'y a aujourd'hui aucune négociation de version entre client et serveur.

## Packaging macOS (`dist/macos/`)

- `make-app.sh` assemble `Clipvault.app` depuis le binaire de l'UI.
  `Info.plist` en `LSUIElement` : popup façon Spotlight, pas d'icône dans le
  Dock ni dans Cmd+Tab. La **signature ad-hoc est obligatoire**, sinon macOS
  retue l'app à chaque reconstruction. Génère l'icône si `dist/macos/icon.png`
  existe.
- Côté code, `clipvault-ui` règle sur macOS `ActivationPolicy::Accessory` +
  `activate_ignoring_other_apps` : sans ça une app `LSUIElement` s'ouvre
  derrière et ne reçoit pas les touches. Ajoute une dépendance `winit` **macOS
  uniquement**, même version que celle d'eframe (0.30) pour que cargo l'unifie.
- `install-launchagent.sh` installe le daemon en LaunchAgent (`RunAtLoad` +
  `KeepAlive`, logs dans `~/Library/Logs/`), équivalent du service systemd
  user. `--uninstall` pour revenir en arrière.

Raccourci global : pas d'équivalent natif d'un bind de WM. Un Service macOS
(`~/Library/Services/Clipvault.workflow`) est installé, la touche s'assigne dans
Réglages → Clavier → Raccourcis → Services. Latence mesurée par CLI : ~90 ms
pour lancer l'app, mais ~950 ms pour le moteur Automator — ces chiffres passent
par des CLI qui rechargent leur framework, le chemin réel est plus rapide, non
mesuré. Si la latence gêne, la bonne solution est un hotkey dans le daemon
(`RegisterEventHotKey`, pas d'autorisation Accessibilité requise) — mais macOS
exige une `CFRunLoop` sur le thread principal, or c'est l'IPC qui l'occupe : il
faudrait les intervertir dans `main.rs`. **Non fait, à coordonner.**

## Autorisation du daemon : ce qu'il faut retenir

Un service lancé par launchd **n'hérite d'aucune autorisation**, contrairement à
un binaire lancé depuis un terminal (qui bénéficie de celles du terminal). D'où
un piège de diagnostic : `--logi-probe` peut annoncer « ping ok » depuis un
shell pendant que le daemon échoue en `0xE00002E2`. Comparer les deux contextes,
pas l'un ou l'autre.

L'autorisation porte sur le binaire exact : le remplacer la casse, sans que
l'entrée disparaisse du panneau. Retirer puis rajouter l'entrée après chaque
déploiement — décocher/recocher ne suffit pas. Signer le daemon avec une
identité stable (certificat auto-signé dans le trousseau `login`) devrait rendre
l'autorisation permanente ; la piste est ouverte, **non vérifiée**.

## Reste à faire côté Mac

- Découverte mDNS `_clipvault._tcp` dans le formulaire de connexion (l'annonce
  serveur est faite ; l'UI ne sonde que `127.0.0.1:7700`).
- `NSPasteboard.changeCount` pour éviter de relire l'image du presse-papier à
  chaque tick de 500 ms.
- Type `org.nspasteboard.ConcealedType` (équivalent du hint password manager).
- Hotkey global intégré au daemon (voir ci-dessus).
