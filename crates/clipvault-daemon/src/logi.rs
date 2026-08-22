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

/// Ordre reçu du serveur de sync (via le thread de réception).
pub enum LogiCommand {
    /// Envoyer la souris vers cet hôte Easy-Switch (1-3).
    SwitchMouse { host: u8 },
}

const VID_LOGITECH: u16 = 0x046d;
/// Récepteurs USB connus (Unifying, Bolt, nano).
const RECEIVER_PIDS: &[u16] = &[0xc52b, 0xc548, 0xc534, 0xc539, 0xc53f];
/// Page HID vendor des récepteurs (HID++) et des périphériques BLE.
const HIDPP_USAGE_PAGES: &[u16] = &[0xff00, 0xff43];
const SWID: u8 = 0x0d;
const READ_TIMEOUT_MS: i32 = 300;
/// Anti-rebond : pas deux événements KeyboardHere en moins de 5 s.
const COOLDOWN: Duration = Duration::from_secs(5);

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
    let mut engine: Option<Engine> = None;
    let mut kb_present: Option<bool> = None;
    let mut last_event = Instant::now() - COOLDOWN;

    loop {
        // Le canal sert aussi de tick (1 s).
        match rx.recv_timeout(Duration::from_secs(1)) {
            Ok(LogiCommand::SwitchMouse { host }) => {
                let e = match ensure_engine(&mut engine, &cfg) {
                    Some(e) => e,
                    None => continue,
                };
                match e.switch_mouse(host) {
                    Ok(()) => info!("logitech: souris envoyée vers l'hôte {host}"),
                    Err(err) => {
                        warn!("logitech: change host: {err}");
                        engine = None; // matériel peut-être débranché : on ré-ouvrira
                    }
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
    }
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
#[derive(Clone, Debug)]
enum Target {
    Receiver { slot: u8 },
    Direct { path: std::ffi::CString },
}

pub struct Engine {
    cfg: LogiConfig,
    api: HidApi,
    /// Interface HID++ du récepteur, si présent.
    receiver: Option<HidDevice>,
    keyboard: Option<Target>,
    mouse: Option<Target>,
    last_scan: Instant,
}

impl Engine {
    pub fn open(cfg: LogiConfig) -> Result<Self> {
        let api = HidApi::new()?;
        let mut engine = Self {
            cfg,
            api,
            receiver: None,
            keyboard: None,
            mouse: None,
            last_scan: Instant::now() - Duration::from_secs(3600),
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
                if self.receiver.is_some() {
                    debug!("logitech: récepteur HID++ ouvert");
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
        let direct: Vec<(std::ffi::CString, String)> = self
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
                )
            })
            .collect();
        for (path, name) in direct {
            if keyboard.is_none() && self.matches_keyboard(&name, 0) {
                keyboard = Some(Target::Direct { path: path.clone() });
            } else if mouse.is_none() && self.matches_mouse(&name, 3) {
                mouse = Some(Target::Direct { path });
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
        match &self.keyboard {
            Some(Target::Receiver { slot }) => {
                let recv = self.receiver.as_ref().ok_or_else(|| anyhow!("récepteur fermé"))?;
                hidpp_ping(recv, *slot)
            }
            Some(Target::Direct { path }) => {
                // En direct (BT), la présence = le device HID répond au ping.
                match self.api.open_path(path) {
                    Ok(dev) => hidpp_ping(&dev, 0xff),
                    Err(_) => Ok(false),
                }
            }
            None => Ok(false),
        }
    }

    /// Envoie la souris vers l'hôte Easy-Switch `host` (1-3).
    pub fn switch_mouse(&mut self, host: u8) -> Result<()> {
        if host == 0 || host > 3 {
            bail!("hôte invalide: {host}");
        }
        self.scan()?;
        let host0 = host - 1; // la feature 0x1814 compte à partir de 0
        match &self.mouse {
            Some(Target::Receiver { slot }) => {
                let recv = self.receiver.as_ref().ok_or_else(|| anyhow!("récepteur fermé"))?;
                let fi = hidpp_feature_index(recv, *slot, 0x1814)?
                    .ok_or_else(|| anyhow!("la souris n'a pas la feature Change Host"))?;
                // setCurrentHost ne répond pas toujours (le lien part aussitôt) :
                // on ignore le timeout de lecture.
                let _ = hidpp_call(recv, *slot, fi, 1, &[host0]);
                Ok(())
            }
            Some(Target::Direct { path }) => {
                let dev = self.api.open_path(path)?;
                let fi = hidpp_feature_index(&dev, 0xff, 0x1814)?
                    .ok_or_else(|| anyhow!("la souris n'a pas la feature Change Host"))?;
                let _ = hidpp_call(&dev, 0xff, fi, 1, &[host0]);
                Ok(())
            }
            None => bail!("souris introuvable ici"),
        }
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
        Ok(out)
    }
}

// ---- Primitives HID++ 2.0 ----

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
