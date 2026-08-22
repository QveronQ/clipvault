//! Suivi souris/clavier Logitech Easy-Switch (HID++ 2.0).
//!
//! Principe : chaque machine surveille la présence de son clavier Logitech
//! (ping HID++ des slots du récepteur Unifying/Bolt, ou présence du device
//! Bluetooth). Quand le clavier APPARAÎT ici, on publie `KeyboardHere` via le
//! serveur de sync ; la machine qui tient encore la souris le reçoit et envoie
//! un Change Host (feature 0x1814) à la souris pour qu'elle suive le clavier.

use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Result};
use clipvault_core::config::LogiConfig;
use clipvault_core::sync::PushItem;
use hidapi::{HidApi, HidDevice};
use tracing::{debug, info, warn};

use crate::store::Store;

/// Ordre reçu du serveur de sync ou de l'IPC.
pub enum LogiCommand {
    /// Envoyer la souris vers cet hôte Easy-Switch (1-3).
    SwitchMouse { host: u8 },
    /// Envoyer souris ET clavier vers cet hôte (raccourci local).
    SwitchBoth { host: u8 },
}

const VID_LOGITECH: u16 = 0x046d;
/// Récepteurs USB connus (Unifying, Bolt, nano).
const RECEIVER_PIDS: &[u16] = &[0xc52b, 0xc548, 0xc534, 0xc539, 0xc53f];
/// Page HID vendor des récepteurs (HID++) et des périphériques BLE.
const HIDPP_USAGE_PAGES: &[u16] = &[0xff00, 0xff43];
/// Page/usages HID standard, pour reconnaître un clavier d'une souris quand le
/// périphérique est appairé en direct (pas de `getDeviceType` sans récepteur).
const USAGE_PAGE_GENERIC_DESKTOP: u16 = 0x0001;
const USAGE_MOUSE: u16 = 0x0002;
const USAGE_KEYBOARD: u16 = 0x0006;
/// Type non déterminable (la plateforme ne renseigne pas les usages HID).
const DEVICE_TYPE_UNKNOWN: u8 = 0xff;
const SWID: u8 = 0x0d;
const READ_TIMEOUT_MS: i32 = 300;
/// Anti-rebond : pas deux événements KeyboardHere en moins de 5 s.
const COOLDOWN: Duration = Duration::from_secs(5);
/// Tick rapide : dépilage des notifications (lecture locale, zéro radio).
const FAST_TICK: Duration = Duration::from_millis(200);
/// Un contrôle complet (pings, scan) tous les N ticks rapides (~1 s).
const SLOW_EVERY: u8 = 5;
/// Au-delà, on considère que le clavier n'est nulle part (éteint, batterie
/// vide, troisième machine) et on cesse de promener la souris.
const MAX_ORPHAN_SWITCHES: u8 = 3;
/// Feature « touches reprogrammables » (divert de boutons).
const FEAT_REPROG_KEYS: u16 = 0x1b04;
/// Bouton pouce des MX Master (CID par défaut du déclencheur de bascule).
const DEFAULT_BUTTON_CID: u16 = 0x00c3;

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Boucle principale, à lancer dans un thread dédié.
pub fn run(
    store: Arc<Mutex<Store>>,
    cfg: LogiConfig,
    device_id: String,
    rx: Receiver<LogiCommand>,
) {
    info!(
        "logitech: actif (mouse_host de cette machine: {})",
        cfg.mouse_host
    );
    if cfg!(target_os = "macos") && cfg.button_cid.is_some_and(|c| c != 0) {
        warn!(
            "logitech: bouton de bascule ignoré sur macOS — un handle permanent \
             sur la souris la rendrait inutilisable (accès exclusif hidapi)"
        );
    }
    let mut engine: Option<Engine> = None;
    let mut kb_present: Option<bool> = None;
    let mut last_event = Instant::now() - COOLDOWN;
    // Bascules d'affilée sans avoir revu le clavier ici. Remis à zéro dès son
    // retour ; sans ce garde-fou, un clavier éteint ferait rebondir la souris
    // d'une machine à l'autre indéfiniment, chacune constatant à son tour
    // qu'elle a la souris sans le clavier.
    let mut orphan_switches: u8 = 0;
    let mut kb_absent_ticks: u8 = 0;
    let mut tick: u8 = 0;

    loop {
        // Le canal sert aussi de tick rapide (200 ms) : chaque itération dépile
        // les notifications (lecture locale) ; les pings et scans, coûteux en
        // radio, ne tournent qu'un tick sur SLOW_EVERY (~1 s).
        match rx.recv_timeout(FAST_TICK) {
            Ok(cmd) => {
                let e = match ensure_engine(&mut engine, &cfg) {
                    Some(e) => e,
                    None => continue,
                };
                let result = match cmd {
                    LogiCommand::SwitchMouse { host } => e
                        .switch_mouse(host)
                        .map(|()| info!("logitech: souris envoyée vers l'hôte {host}")),
                    LogiCommand::SwitchBoth { host } => switch_both(e, host),
                };
                if let Err(err) = result {
                    match explain_hid_error(&err) {
                        Some(hint) => warn!("logitech: change host: {err} — {hint}"),
                        None => warn!("logitech: change host: {err}"),
                    }
                    engine = None; // matériel peut-être débranché : on ré-ouvrira
                }
                continue;
            }
            Err(RecvTimeoutError::Disconnected) => return,
            Err(RecvTimeoutError::Timeout) => {}
        }

        let e = match ensure_engine(&mut engine, &cfg) {
            Some(e) => e,
            None => continue,
        };

        // --- Tick rapide : notifications uniquement (aucune radio) ---
        let events = e.drain_events().unwrap_or_default();
        if events.button_pressed {
            let target = cfg
                .toggle_host
                .unwrap_or(if cfg.mouse_host == 1 { 2 } else { 1 });
            info!("logitech: bouton souris pressé, bascule vers l'hôte {target}");
            if let Err(err) = switch_both(e, target) {
                warn!("logitech: bascule bouton: {err}");
            }
            continue;
        }
        if events.kb_link_lost
            && orphan_switches < MAX_ORPHAN_SWITCHES
            && last_event.elapsed() >= COOLDOWN
        {
            // Le récepteur signale un lien clavier tombé : vérification express
            // (double ping) pour écarter une simple mise en veille radio.
            match e.keyboard_gone_confirmed() {
                Ok(true) => {
                    kb_present = Some(false);
                    kb_absent_ticks = 2;
                    if let Ok(true) = e.mouse_here() {
                        let target = cfg
                            .toggle_host
                            .unwrap_or(if cfg.mouse_host == 1 { 2 } else { 1 });
                        last_event = Instant::now();
                        orphan_switches += 1;
                        info!(
                            "logitech: clavier parti (notification), la souris le rejoint (hôte {target})"
                        );
                        if let Err(err) = e.switch_mouse(target) {
                            warn!("logitech: suivi souris: {err}");
                        }
                    }
                }
                Ok(false) => debug!("logitech: lien clavier tombé mais il répond (veille radio)"),
                Err(err) => debug!("logitech: vérification clavier: {err}"),
            }
            continue;
        }

        tick = tick.wrapping_add(1);
        if !tick.is_multiple_of(SLOW_EVERY) {
            continue;
        }

        // --- Tick lent (~1 s) : divert du bouton, pings de présence ---
        if let Err(err) = e.poll_button() {
            debug!("logitech: bouton: {err}");
        }

        let present = match e.keyboard_present() {
            Ok(p) => p,
            Err(err) => {
                debug!("logitech: sonde clavier: {err}");
                engine = None;
                continue;
            }
        };
        let arrived = present && kb_present != Some(true);
        kb_present = Some(present);
        if arrived && last_event.elapsed() >= COOLDOWN {
            last_event = Instant::now();
            info!("logitech: clavier détecté ici, on rapatrie la souris");
            let item = PushItem::KeyboardHere {
                device: device_id.clone(),
                mouse_host: cfg.mouse_host,
                ts: now(),
            };
            if let Err(err) = store.lock().unwrap().enqueue(&item) {
                warn!("logitech: enqueue: {err}");
            }
        }

        // Suivi local : la souris doit être là où est le clavier. Si le clavier
        // n'est pas ici et que la souris y est encore, on l'envoie rejoindre
        // l'autre machine — sans passer par le serveur, donc sans dépendre du
        // réseau ni de la latence de la sync. L'événement KeyboardHere reste
        // utile au-delà de deux machines, où « l'autre » est ambigu.
        if present {
            orphan_switches = 0;
            kb_absent_ticks = 0;
            continue;
        }
        // Un ping peut échouer transitoirement (radio occupée) : exiger deux
        // ticks absents consécutifs avant d'arracher la souris à l'utilisateur.
        kb_absent_ticks = kb_absent_ticks.saturating_add(1);
        if kb_absent_ticks < 2 {
            continue;
        }
        if orphan_switches >= MAX_ORPHAN_SWITCHES || last_event.elapsed() < COOLDOWN {
            continue;
        }
        match e.mouse_here() {
            Ok(true) => {
                let target = cfg
                    .toggle_host
                    .unwrap_or(if cfg.mouse_host == 1 { 2 } else { 1 });
                last_event = Instant::now();
                orphan_switches += 1;
                info!("logitech: souris ici sans le clavier, elle le rejoint (hôte {target})");
                if let Err(err) = e.switch_mouse(target) {
                    match explain_hid_error(&err) {
                        Some(hint) => warn!("logitech: suivi souris: {err} — {hint}"),
                        None => warn!("logitech: suivi souris: {err}"),
                    }
                }
            }
            Ok(false) => {}
            Err(err) => debug!("logitech: sonde souris: {err}"),
        }
    }
}

/// Bascule souris puis clavier, avec un court décalage entre les deux
/// (réglage validé à l'usage : la souris d'abord, le clavier 350 ms après).
fn switch_both(e: &mut Engine, host: u8) -> Result<()> {
    let m = e.switch_mouse(host);
    std::thread::sleep(Duration::from_millis(350));
    let k = e.switch_keyboard(host);
    m.and(k)
        .map(|()| info!("logitech: souris + clavier envoyés vers l'hôte {host}"))
}

fn ensure_engine<'a>(engine: &'a mut Option<Engine>, cfg: &LogiConfig) -> Option<&'a mut Engine> {
    if engine.is_none() {
        match Engine::open(cfg.clone()) {
            Ok(e) => *engine = Some(e),
            Err(err) => {
                debug!("logitech: init: {err}");
                return None;
            }
        }
    }
    engine.as_mut()
}

/// Un périphérique localisé : derrière un récepteur (slot 1-6) ou en direct
/// (Bluetooth, device index 0xFF).
#[derive(Clone)]
enum Target {
    Receiver { slot: u8 },
    /// Appairage direct (Bluetooth). Le chemin change à chaque reconnexion
    /// (sur macOS c'est un registry ID), d'où le `pid` qui, lui, est stable.
    Direct { path: std::ffi::CString, pid: u16 },
}

impl std::fmt::Debug for Target {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            // Le chemin d'un périphérique direct change à chaque reconnexion :
            // l'afficher n'aiderait personne, le product_id si.
            Target::Receiver { slot } => write!(f, "récepteur slot {slot}"),
            Target::Direct { pid, .. } => write!(f, "direct pid {pid:#06x}"),
        }
    }
}

/// Notifications dépilées lors d'un tick rapide.
#[derive(Default)]
struct DrainedEvents {
    button_pressed: bool,
    kb_link_lost: bool,
    #[allow(dead_code)]
    kb_link_back: bool,
}

/// État du bouton de bascule détourné sur la souris.
struct ButtonCtx {
    dev_idx: u8,
    feat_idx: u8,
    cid: u16,
    pressed: bool,
}

pub struct Engine {
    cfg: LogiConfig,
    api: HidApi,
    /// Interface HID++ du récepteur, si présent.
    receiver: Option<HidDevice>,
    keyboard: Option<Target>,
    mouse: Option<Target>,
    last_scan: Instant,
    /// Handle persistant de la souris en appairage direct (lecture des
    /// notifications de bouton détourné).
    mouse_handle: Option<HidDevice>,
    button: Option<ButtonCtx>,
    mouse_present: Option<bool>,
}

impl Engine {
    pub fn open(cfg: LogiConfig) -> Result<Self> {
        let api = HidApi::new()?;
        // macOS : ouvrir en NON exclusif. Par défaut hidapi confisque
        // l'appareil au système — la souris affiche son canal mais le pointeur
        // ne bouge plus tant que le daemon tient le handle.
        #[cfg(target_os = "macos")]
        api.set_open_exclusive(false);
        let mut engine = Self {
            cfg,
            api,
            receiver: None,
            keyboard: None,
            mouse: None,
            last_scan: Instant::now() - Duration::from_secs(3600),
            mouse_handle: None,
            button: None,
            mouse_present: None,
        };
        engine.scan()?;
        Ok(engine)
    }

    /// (Re)découvre récepteur et périphériques. Appelé au plus toutes les 10 s.
    fn scan(&mut self) -> Result<()> {
        if self.last_scan.elapsed() < Duration::from_secs(10)
            && (self.keyboard.is_some() || self.mouse.is_some())
        {
            return Ok(());
        }
        self.last_scan = Instant::now();
        self.api.refresh_devices()?;

        // 1. Récepteur USB (interface HID++).
        if self.receiver.is_none() {
            let path = self
                .api
                .device_list()
                .find(|d| {
                    d.vendor_id() == VID_LOGITECH
                        && RECEIVER_PIDS.contains(&d.product_id())
                        && HIDPP_USAGE_PAGES.contains(&d.usage_page())
                })
                .map(|d| d.path().to_owned());
            if let Some(p) = path {
                self.receiver = self.api.open_path(&p).ok();
                if let Some(r) = &self.receiver {
                    debug!("logitech: récepteur HID++ ouvert");
                    // Sans ce flag, le récepteur ne pousse pas les événements
                    // de connexion (0x41) dont dépend la détection rapide.
                    if let Err(e) = enable_wireless_notifications(r) {
                        debug!("logitech: activation notifications: {e}");
                    }
                }
            }
        }

        // 2. Slots du récepteur : ping + identification par nom/type.
        let mut keyboard = None;
        let mut mouse = None;
        if let Some(recv) = &self.receiver {
            for slot in 1..=6u8 {
                if hidpp_ping(recv, slot).unwrap_or(false) {
                    let name = hidpp_device_name(recv, slot).unwrap_or_default();
                    let dtype = hidpp_device_type(recv, slot).unwrap_or(0xff);
                    debug!("logitech: slot {slot}: {name:?} (type {dtype})");
                    if self.matches_keyboard(&name, dtype) {
                        keyboard = Some(Target::Receiver { slot });
                    } else if self.matches_mouse(&name, dtype) {
                        mouse = Some(Target::Receiver { slot });
                    }
                }
            }
        }

        // 3. Périphériques HID++ directs (Bluetooth).
        // Sans récepteur, pas de `getDeviceType` : le type se déduit des autres
        // interfaces HID du même périphérique (Generic Desktop, usage 6 =
        // clavier, 2 = souris). Sans cette déduction, tout HID++ trouvé
        // passerait pour un clavier et la souris et le clavier se retrouvent
        // intervertis selon l'ordre d'énumération.
        let direct: Vec<(std::ffi::CString, String, u8, u16)> = self
            .api
            .device_list()
            .filter(|d| {
                d.vendor_id() == VID_LOGITECH
                    && !RECEIVER_PIDS.contains(&d.product_id())
                    && HIDPP_USAGE_PAGES.contains(&d.usage_page())
            })
            .map(|d| {
                (
                    d.path().to_owned(),
                    d.product_string().unwrap_or_default().to_string(),
                    self.desktop_device_type(d.product_id()),
                    d.product_id(),
                )
            })
            .collect();
        for (path, name, dtype, pid) in direct {
            if dtype == DEVICE_TYPE_UNKNOWN && self.cfg.keyboard.is_none() && self.cfg.mouse.is_none()
            {
                // Type indéterminable (plateforme qui ne renseigne pas les
                // usages HID) : ne rien deviner — un Change Host envoyé au
                // mauvais périphérique est pire que pas de bascule du tout.
                debug!("logitech: {name}: type indéterminé, préciser [logitech] keyboard/mouse");
                continue;
            }
            if keyboard.is_none() && self.matches_keyboard(&name, dtype) {
                keyboard = Some(Target::Direct {
                    path: path.clone(),
                    pid,
                });
            } else if mouse.is_none() && self.matches_mouse(&name, dtype) {
                mouse = Some(Target::Direct { path, pid });
            }
        }

        if self.keyboard.is_none() && keyboard.is_some() {
            info!("logitech: clavier localisé: {keyboard:?}");
        }
        if self.mouse.is_none() && mouse.is_some() {
            info!("logitech: souris localisée: {mouse:?}");
        }
        // Le clavier peut être absent (parti sur une autre machine) : on garde
        // le dernier slot connu pour continuer à le sonder.
        if keyboard.is_some() {
            self.keyboard = keyboard;
        }
        if mouse.is_some() {
            self.mouse = mouse;
        }
        Ok(())
    }

    /// Type HID++ déduit des interfaces Generic Desktop exposées par le même
    /// périphérique (`product_id`), pour les appairages directs sans récepteur.
    /// Renvoie les mêmes codes que la feature 0x0005 : 0 = clavier, 3 = souris.
    fn desktop_device_type(&self, pid: u16) -> u8 {
        let has = |usage: u16| {
            self.api.device_list().any(|d| {
                d.vendor_id() == VID_LOGITECH
                    && d.product_id() == pid
                    && d.usage_page() == USAGE_PAGE_GENERIC_DESKTOP
                    && d.usage() == usage
            })
        };
        if has(USAGE_KEYBOARD) {
            0
        } else if has(USAGE_MOUSE) {
            3
        } else {
            DEVICE_TYPE_UNKNOWN
        }
    }

    fn matches_keyboard(&self, name: &str, dtype: u8) -> bool {
        match &self.cfg.keyboard {
            Some(sub) => name.to_lowercase().contains(&sub.to_lowercase()),
            None => dtype == 0, // 0 = Keyboard (feature 0x0005 getDeviceType)
        }
    }

    fn matches_mouse(&self, name: &str, dtype: u8) -> bool {
        match &self.cfg.mouse {
            Some(sub) => name.to_lowercase().contains(&sub.to_lowercase()),
            None => dtype == 3, // 3 = Mouse
        }
    }

    /// Le clavier répond-il ici en ce moment ?
    pub fn keyboard_present(&mut self) -> Result<bool> {
        self.scan()?;
        match self.keyboard.clone() {
            Some(Target::Receiver { slot }) => {
                let recv = self.receiver.as_ref().ok_or_else(|| anyhow!("récepteur fermé"))?;
                hidpp_ping(recv, slot)
            }
            Some(Target::Direct { pid, .. }) => {
                // En Bluetooth, un périphérique ne figure dans l'énumération HID
                // que s'il est connecté à cette machine : la présence se lit
                // sans rien ouvrir. C'est le seul moyen pour un clavier, macOS
                // refusant d'ouvrir un IOHIDDevice de ce type
                // (kIOReturnNotPrivileged), y compris avec « Saisie de contenu »
                // accordée — donc pas de ping possible.
                self.api.refresh_devices()?;
                Ok(self
                    .api
                    .device_list()
                    .any(|d| d.vendor_id() == VID_LOGITECH && d.product_id() == pid))
            }
            None => Ok(false),
        }
    }

    /// Chemin HID++ courant d'un périphérique direct : il change à chaque
    /// reconnexion Bluetooth, celui mémorisé par `scan()` peut être périmé.
    fn resolve_direct(&self, pid: u16) -> Option<std::ffi::CString> {
        self.api
            .device_list()
            .find(|d| {
                d.vendor_id() == VID_LOGITECH
                    && d.product_id() == pid
                    && HIDPP_USAGE_PAGES.contains(&d.usage_page())
            })
            .map(|d| d.path().to_owned())
    }

    /// Envoie la souris vers l'hôte Easy-Switch `host` (1-3).
    pub fn switch_mouse(&mut self, host: u8) -> Result<()> {
        self.scan()?;
        let target = self.mouse.clone();
        self.change_host(target.as_ref(), host, "souris")
    }

    /// Envoie le clavier vers l'hôte Easy-Switch `host` (1-3).
    pub fn switch_keyboard(&mut self, host: u8) -> Result<()> {
        self.scan()?;
        let target = self.keyboard.clone();
        self.change_host(target.as_ref(), host, "clavier")
    }

    fn change_host(&self, target: Option<&Target>, host: u8, label: &str) -> Result<()> {
        if host == 0 || host > 3 {
            bail!("hôte invalide: {host}");
        }
        let host0 = host - 1; // la feature 0x1814 compte à partir de 0
        match target {
            Some(Target::Receiver { slot }) => {
                let recv = self.receiver.as_ref().ok_or_else(|| anyhow!("récepteur fermé"))?;
                let fi = hidpp_feature_index(recv, *slot, 0x1814)?
                    .ok_or_else(|| anyhow!("{label}: pas de feature Change Host"))?;
                // setCurrentHost ne répond pas toujours (le lien part aussitôt) :
                // on ignore le timeout de lecture.
                let _ = hidpp_call(recv, *slot, fi, 1, &[host0]);
                Ok(())
            }
            Some(Target::Direct { pid, .. }) => {
                // Plus énuméré = déjà parti sur une autre machine. Ouvrir le
                // chemin mémorisé ne donnerait qu'une erreur hidapi opaque
                // (« mach entry not found »), le chemin changeant à chaque
                // reconnexion.
                let path = self.resolve_direct(*pid).ok_or_else(|| {
                    anyhow!("{label} introuvable ici (déjà parti sur une autre machine)")
                })?;
                let dev = self.api.open_path(&path)?;
                let fi = hidpp_feature_index(&dev, 0xff, 0x1814)?
                    .ok_or_else(|| anyhow!("{label}: pas de feature Change Host"))?;
                let _ = hidpp_call(&dev, 0xff, fi, 1, &[host0]);
                Ok(())
            }
            None => bail!("{label} introuvable ici"),
        }
    }

    /// CID du bouton de bascule (0 = désactivé).
    fn button_cid(&self) -> Option<u16> {
        // Sur macOS, hidapi ouvre un périphérique en accès EXCLUSIF : garder un
        // handle sur la souris pour lire les notifications la confisque au
        // système, qui ne la voit plus bouger — elle affiche bien son canal mais
        // le pointeur est mort. Vérifié sur MX Master 3S en Bluetooth direct :
        // toute autre ouverture échoue alors avec kIOReturnExclusiveAccess
        // (0xE00002C5), et la souris redevient normale dès que le daemon rend
        // le handle. Le divert est donc inutilisable ici, quel que soit le CID.
        if cfg!(target_os = "macos") {
            return None;
        }
        match self.cfg.button_cid {
            Some(0) => None,
            Some(cid) => Some(cid),
            None => Some(DEFAULT_BUTTON_CID),
        }
    }

    /// La souris est-elle connectée ici en ce moment ?
    fn mouse_here(&mut self) -> Result<bool> {
        match self.mouse.clone() {
            Some(Target::Receiver { slot }) => {
                let recv = self
                    .receiver
                    .as_ref()
                    .ok_or_else(|| anyhow!("récepteur fermé"))?;
                hidpp_ping(recv, slot)
            }
            Some(Target::Direct { pid, .. }) => Ok(self
                .api
                .device_list()
                .any(|d| d.vendor_id() == VID_LOGITECH && d.product_id() == pid)),
            None => Ok(false),
        }
    }

    /// Détourne le bouton `cid` de la souris (feature 0x1b04, setCidReporting).
    /// À refaire à chaque reconnexion : le divert ne survit pas au changement
    /// d'hôte ni à la mise en veille profonde.
    fn setup_button(&mut self, cid: u16) -> Result<()> {
        let (hi, lo) = ((cid >> 8) as u8, (cid & 0xff) as u8);
        match self.mouse.clone() {
            Some(Target::Receiver { slot }) => {
                let recv = self
                    .receiver
                    .as_ref()
                    .ok_or_else(|| anyhow!("récepteur fermé"))?;
                let fi = hidpp_feature_index(recv, slot, FEAT_REPROG_KEYS)?
                    .ok_or_else(|| anyhow!("souris sans feature 0x1b04"))?;
                // flags 0x03 = divert + dvalid
                hidpp_call(recv, slot, fi, 3, &[hi, lo, 0x03])?;
                self.button = Some(ButtonCtx {
                    dev_idx: slot,
                    feat_idx: fi,
                    cid,
                    pressed: false,
                });
            }
            Some(Target::Direct { path, pid }) => {
                let path = self.resolve_direct(pid).unwrap_or(path);
                let dev = self.api.open_path(&path)?;
                let fi = hidpp_feature_index(&dev, 0xff, FEAT_REPROG_KEYS)?
                    .ok_or_else(|| anyhow!("souris sans feature 0x1b04"))?;
                hidpp_call(&dev, 0xff, fi, 3, &[hi, lo, 0x03])?;
                self.mouse_handle = Some(dev);
                self.button = Some(ButtonCtx {
                    dev_idx: 0xff,
                    feat_idx: fi,
                    cid,
                    pressed: false,
                });
            }
            None => bail!("souris introuvable ici"),
        }
        info!("logitech: bouton 0x{cid:04x} de la souris détourné (bascule)");
        Ok(())
    }

    /// À appeler à chaque tick — et AVANT tout ping du même tick : les pings
    /// lisent la même file hidraw et jettent les rapports qui ne matchent pas
    /// leur réponse, donc les notifications de bouton accumulées depuis le
    /// tick précédent doivent être dépilées d'abord.
    /// Gère aussi le divert (re-pose après reconnexion). Renvoie true si le
    /// bouton de bascule vient d'être pressé.
    pub fn poll_button(&mut self) -> Result<()> {
        let Some(cid) = self.button_cid() else {
            return Ok(());
        };
        self.scan()?;
        let present = self.mouse_here()?;
        let arrived = present && self.mouse_present != Some(true);
        self.mouse_present = Some(present);
        if !present {
            // La souris est partie : le divert sera à refaire à son retour.
            self.button = None;
            self.mouse_handle = None;
            return Ok(());
        }
        if arrived || self.button.is_none() {
            self.setup_button(cid)?;
        }
        Ok(())
    }

    /// Le clavier est-il vraiment parti ? Deux pings espacés de 150 ms —
    /// distingue « a quitté ce canal » d'un simple raté radio transitoire.
    pub fn keyboard_gone_confirmed(&mut self) -> Result<bool> {
        if self.keyboard_present()? {
            return Ok(false);
        }
        std::thread::sleep(Duration::from_millis(150));
        Ok(!self.keyboard_present()?)
    }

    /// Dépile les notifications en attente : bouton détourné et événements de
    /// lien du clavier (0x41, poussés par le récepteur — détection quasi
    /// instantanée d'un départ, sans coût radio).
    fn drain_events(&mut self) -> Result<DrainedEvents> {
        let mut out = DrainedEvents::default();
        let btn = self
            .button
            .as_ref()
            .map(|c| (c.dev_idx, c.feat_idx, c.cid));
        let mut pressed = self.button.as_ref().map(|c| c.pressed).unwrap_or(false);
        let kb_slot = match &self.keyboard {
            Some(Target::Receiver { slot }) => Some(*slot),
            _ => None,
        };
        {
            // Le récepteur porte notifications de lien ET événements de bouton ;
            // en appairage direct, seul le handle souris peut parler.
            let dev: &HidDevice = match (&self.receiver, &self.mouse_handle) {
                (Some(r), _) => r,
                (None, Some(h)) => h,
                (None, None) => return Ok(out),
            };
            loop {
                let mut buf = [0u8; 32];
                let n = dev.read_timeout(&mut buf, 0)?;
                if n == 0 {
                    break;
                }
                // divertedButtonsEvent (event 0) : liste des CID pressés.
                if let Some((dev_idx, feat_idx, cid)) = btn {
                    if buf[0] == 0x11 && buf[1] == dev_idx && buf[2] == feat_idx && buf[3] == 0x00
                    {
                        debug!("logitech: divertedButtonsEvent: {:02x?}", &buf[4..12]);
                        let now_pressed = (0..4).any(|i| {
                            u16::from_be_bytes([buf[4 + 2 * i], buf[5 + 2 * i]]) == cid
                        });
                        if now_pressed && !pressed {
                            out.button_pressed = true;
                        }
                        pressed = now_pressed;
                    }
                }
                // Notification de connexion (0x41) : bit 0x40 = lien NON établi.
                if let Some(slot) = kb_slot {
                    if buf[0] == 0x10 && buf[1] == slot && buf[2] == 0x41 {
                        if buf[4] & 0x40 != 0 {
                            debug!("logitech: notification: lien clavier perdu");
                            out.kb_link_lost = true;
                        } else {
                            debug!("logitech: notification: lien clavier rétabli");
                            out.kb_link_back = true;
                        }
                    }
                }
            }
        }
        if let Some(ctx) = &mut self.button {
            ctx.pressed = pressed;
        }
        Ok(out)
    }

    /// Sonde de diagnostic : liste les slots du récepteur et les devices directs.
    pub fn probe(cfg: LogiConfig) -> Result<String> {
        let mut out = String::new();
        let engine = Self::open(cfg)?;
        if let Some(recv) = &engine.receiver {
            out.push_str("Récepteur HID++ trouvé.\n");
            for slot in 1..=6u8 {
                match hidpp_ping(recv, slot) {
                    Ok(true) => {
                        let name = hidpp_device_name(recv, slot).unwrap_or_default();
                        let dtype = hidpp_device_type(recv, slot).unwrap_or(0xff);
                        let host = hidpp_feature_index(recv, slot, 0x1814)
                            .ok()
                            .flatten()
                            .is_some();
                        out.push_str(&format!(
                            "  slot {slot}: {name} (type {dtype}, change-host: {})\n",
                            if host { "oui" } else { "non" }
                        ));
                    }
                    _ => out.push_str(&format!("  slot {slot}: absent/déconnecté\n")),
                }
            }
        } else {
            out.push_str("Pas de récepteur USB Logitech.\n");
        }
        out.push_str(&format!(
            "Clavier: {:?}\nSouris: {:?}\n",
            engine.keyboard, engine.mouse
        ));

        // Détail des périphériques appairés en direct (Bluetooth, index 0xFF) :
        // sans récepteur, la boucle ci-dessus n'affiche rien d'exploitable.
        for (label, target) in [("Clavier", &engine.keyboard), ("Souris", &engine.mouse)] {
            let Some(Target::Direct { path, .. }) = target else {
                continue;
            };
            let dev = match engine.api.open_path(path) {
                Ok(d) => d,
                Err(e) => {
                    // Deux causes bien distinctes, à ne pas confondre au
                    // diagnostic : un clavier est interdit d'ouverture par macOS
                    // (privilege violation), tandis qu'un « exclusive access »
                    // signale qu'un autre process tient déjà l'appareil — y
                    // compris un second clipvault-daemon.
                    let listed = matches!(target, Some(Target::Direct { pid, .. })
                        if engine.api.device_list().any(|d| {
                            d.vendor_id() == VID_LOGITECH && d.product_id() == *pid
                        }));
                    out.push_str(&format!(
                        "  {label} (direct): {}, non ouvrable — {e}\n",
                        if listed {
                            "connecté (présence lue par énumération)"
                        } else {
                            "absent"
                        }
                    ));
                    continue;
                }
            };
            let ping = hidpp_ping(&dev, 0xff).unwrap_or(false);
            let feat = hidpp_feature_index(&dev, 0xff, 0x1814).ok().flatten();
            let hosts = match feat {
                // 0x1814 fonction 0 (getHostInfo) : [nbHost, currHost] — lecture
                // pure, elle ne fait basculer personne.
                Some(fi) => hidpp_call(&dev, 0xff, fi, 0, &[])
                    .ok()
                    .map(|r| (r[0], r[1] + 1)),
                None => None,
            };
            out.push_str(&format!(
                "  {label} (direct): ping {}, change-host {}",
                if ping { "ok" } else { "muet" },
                match feat {
                    Some(fi) => format!("oui (index {fi})"),
                    None => "non".to_string(),
                }
            ));
            match hosts {
                Some((nb, cur)) => out.push_str(&format!(", hôte {cur}/{nb}\n")),
                None => out.push('\n'),
            }
            // Feature 0x1815 (HostsInfo) : le périphérique retient le nom de
            // chaque machine appairée. De quoi déduire les numéros de canal au
            // lieu de les écrire à la main dans [logitech].
            if let Ok(Some(fi)) = hidpp_feature_index(&dev, 0xff, 0x1815) {
                if let Ok(info) = hidpp_call(&dev, 0xff, fi, 0, &[]) {
                    let nb = info[0];
                    for h in 0..nb.min(3) {
                        let name = hidpp_host_name(&dev, 0xff, fi, h).unwrap_or_default();
                        if !name.is_empty() {
                            out.push_str(&format!("    canal {} : « {name} »\n", h + 1));
                        }
                    }
                }
            }
        }

        // Boutons détournables de la souris (candidats pour [logitech] button_cid).
        let mouse_cids = (|| -> Option<String> {
            let list = |dev: &HidDevice, dev_idx: u8| -> Option<String> {
                let fi = hidpp_feature_index(dev, dev_idx, FEAT_REPROG_KEYS)
                    .ok()
                    .flatten()?;
                let count = hidpp_call(dev, dev_idx, fi, 0, &[]).ok()?[0];
                let mut line = String::new();
                for i in 0..count {
                    if let Ok(r) = hidpp_call(dev, dev_idx, fi, 1, &[i]) {
                        line.push_str(&format!("0x{:04x} ", u16::from_be_bytes([r[0], r[1]])));
                    }
                }
                Some(line)
            };
            match &engine.mouse {
                Some(Target::Receiver { slot }) => list(engine.receiver.as_ref()?, *slot),
                Some(Target::Direct { path, pid }) => {
                    let path = engine.resolve_direct(*pid).unwrap_or_else(|| path.clone());
                    list(&engine.api.open_path(&path).ok()?, 0xff)
                }
                None => None,
            }
        })();
        if let Some(cids) = mouse_cids {
            out.push_str(&format!(
                "Boutons souris (CID 0x1b04): {cids}— bouton pouce MX Master = 0x00c3\n"
            ));
        }
        Ok(out)
    }
}

// ---- Primitives HID++ 2.0 ----

/// Nom de la machine appairée sur un canal (feature 0x1815, fonction 2).
/// Le nom arrive par tranches, indexées par l'octet de départ.
fn hidpp_host_name(dev: &HidDevice, dev_idx: u8, feat_idx: u8, host: u8) -> Result<String> {
    // Fonction 1 (getHostInfo) donne la longueur du nom.
    let info = hidpp_call(dev, dev_idx, feat_idx, 1, &[host])?;
    let len = info[4] as usize;
    let mut out = Vec::with_capacity(len);
    while out.len() < len.min(64) {
        let chunk = hidpp_call(dev, dev_idx, feat_idx, 2, &[host, out.len() as u8])?;
        // [0] = index de l'hôte, [1] = octet de départ, le reste = le texte.
        let text = &chunk[2..];
        if text.iter().all(|&b| b == 0) {
            break;
        }
        out.extend(text.iter().take_while(|&&b| b != 0));
    }
    out.truncate(len);
    Ok(String::from_utf8_lossy(&out).trim().to_string())
}

/// Traduit les refus d'ouverture HID de macOS, dont les codes se ressemblent
/// et n'ont pas du tout les mêmes causes ni les mêmes remèdes.
fn explain_hid_error(err: &anyhow::Error) -> Option<&'static str> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    let msg = err.to_string();
    if msg.contains("0xE00002E2") {
        // TCC. Piège classique : réinstaller le binaire invalide l'autorisation,
        // qui est attachée à ce binaire précis — l'entrée reste pourtant cochée
        // dans le panneau, ce qui donne l'illusion que tout va bien.
        Some(
            "autorisation « Saisie de contenu » manquante — ajouter ce binaire dans              Réglages > Confidentialité et sécurité > Saisie de contenu (à REFAIRE              après chaque mise à jour du binaire : l'ancienne entrée devient caduque)",
        )
    } else if msg.contains("0xE00002C5") {
        Some("appareil déjà ouvert par un autre process (un second clipvault-daemon ?)")
    } else if msg.contains("0xE00002C1") {
        Some("macOS interdit d'ouvrir un clavier, quelle que soit l'autorisation")
    } else {
        None
    }
}

/// Requête longue (0x11) + attente de la réponse correspondante.
fn hidpp_call(
    dev: &HidDevice,
    dev_idx: u8,
    feat_idx: u8,
    func: u8,
    params: &[u8],
) -> Result<[u8; 16]> {
    let funcsw = (func << 4) | SWID;
    let mut report = [0u8; 20];
    report[0] = 0x11;
    report[1] = dev_idx;
    report[2] = feat_idx;
    report[3] = funcsw;
    report[4..4 + params.len()].copy_from_slice(params);
    dev.write(&report)?;

    let deadline = Instant::now() + Duration::from_millis(1000);
    while Instant::now() < deadline {
        let mut buf = [0u8; 32];
        let n = dev.read_timeout(&mut buf, READ_TIMEOUT_MS)?;
        if n == 0 {
            continue;
        }
        // Réponse attendue.
        if buf[0] == 0x11 && buf[1] == dev_idx && buf[2] == feat_idx && buf[3] == funcsw {
            let mut out = [0u8; 16];
            out.copy_from_slice(&buf[4..20]);
            return Ok(out);
        }
        // Erreur HID++ 1.0 (récepteur : device déconnecté, etc.).
        if buf[0] == 0x10 && buf[1] == dev_idx && buf[2] == 0x8f {
            bail!("erreur HID++ 1.0 (code 0x{:02x})", buf[5]);
        }
        // Erreur HID++ 2.0.
        if buf[0] == 0x11 && buf[1] == dev_idx && buf[2] == 0xff && buf[3] == funcsw {
            bail!("erreur HID++ 2.0 (code 0x{:02x})", buf[5]);
        }
        // Notification sans rapport : on continue à lire.
    }
    bail!("timeout HID++")
}

/// Active les notifications sans fil du récepteur (HID++ 1.0, SET_REGISTER
/// 0x00, flag 0x000100) : il pousse alors les événements de connexion et de
/// déconnexion (0x41) des périphériques appairés. L'accusé de réception est
/// ignoré (il sera écarté par les lecteurs suivants comme rapport non attendu).
fn enable_wireless_notifications(dev: &HidDevice) -> Result<()> {
    dev.write(&[0x10, 0xff, 0x80, 0x00, 0x00, 0x01, 0x00])?;
    Ok(())
}

/// Ping (root feature 0x0000, fonction 1). true = le device répond.
fn hidpp_ping(dev: &HidDevice, dev_idx: u8) -> Result<bool> {
    match hidpp_call(dev, dev_idx, 0x00, 1, &[0, 0, 0xaa]) {
        Ok(r) => Ok(r[2] == 0xaa),
        Err(_) => Ok(false),
    }
}

/// Index de la feature `feat_id`, via la root feature (0x0000, fonction 0).
fn hidpp_feature_index(dev: &HidDevice, dev_idx: u8, feat_id: u16) -> Result<Option<u8>> {
    let r = hidpp_call(
        dev,
        dev_idx,
        0x00,
        0,
        &[(feat_id >> 8) as u8, (feat_id & 0xff) as u8],
    )?;
    Ok((r[0] != 0).then_some(r[0]))
}

/// Nom du périphérique (feature 0x0005, fonctions 0 et 1).
fn hidpp_device_name(dev: &HidDevice, dev_idx: u8) -> Result<String> {
    let Some(fi) = hidpp_feature_index(dev, dev_idx, 0x0005)? else {
        return Ok(String::new());
    };
    let count = hidpp_call(dev, dev_idx, fi, 0, &[])?[0] as usize;
    let mut name = Vec::with_capacity(count);
    let mut offset = 0u8;
    while name.len() < count {
        let chunk = hidpp_call(dev, dev_idx, fi, 1, &[offset])?;
        for b in chunk {
            if name.len() < count && b != 0 {
                name.push(b);
            }
        }
        offset = name.len() as u8;
        if offset == 0 {
            break;
        }
    }
    Ok(String::from_utf8_lossy(&name).into_owned())
}

/// Type du périphérique (feature 0x0005, fonction 2) : 0=clavier, 3=souris.
fn hidpp_device_type(dev: &HidDevice, dev_idx: u8) -> Result<u8> {
    let Some(fi) = hidpp_feature_index(dev, dev_idx, 0x0005)? else {
        return Ok(0xff);
    };
    Ok(hidpp_call(dev, dev_idx, fi, 2, &[])?[0])
}
