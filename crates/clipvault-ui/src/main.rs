//! clipvault — popup d'historique du presse-papier (egui).
//! Client léger : tout l'état vit dans le daemon, interrogé via socket Unix.

mod theme;

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use clipvault_core::config::SyncConfig;
use clipvault_core::ipc::{Request, Response};
use clipvault_core::sync::ServerStatus;
use clipvault_core::types::{ContentKind, EntryMeta};
use eframe::egui::{self, Align2, Color32, FontId, RichText};

const SEARCH_LIMIT: u32 = 100;
const ROW_HEIGHT: f32 = 46.0;
const WINDOW_ROUNDING: u8 = 14;

struct Client {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
}

impl Client {
    fn connect() -> Result<Self> {
        let path = clipvault_core::socket_path();
        let stream = UnixStream::connect(&path)
            .with_context(|| format!("connexion au daemon ({})", path.display()))?;
        Ok(Self {
            reader: BufReader::new(stream.try_clone()?),
            writer: stream,
        })
    }

    fn request(&mut self, req: &Request) -> Result<Response> {
        let mut line = serde_json::to_string(req)?;
        line.push('\n');
        self.writer.write_all(line.as_bytes())?;
        let mut buf = String::new();
        self.reader.read_line(&mut buf)?;
        if buf.is_empty() {
            bail!("le daemon a fermé la connexion");
        }
        Ok(serde_json::from_str(&buf)?)
    }

    fn search(&mut self, query: &str, device: Option<&str>) -> Result<Vec<EntryMeta>> {
        match self.request(&Request::Search {
            query: query.to_string(),
            limit: SEARCH_LIMIT,
            offset: 0,
            device: device.map(str::to_string),
        })? {
            Response::Entries { entries } => Ok(entries),
            Response::Error { message } => bail!(message),
            other => bail!("réponse inattendue: {other:?}"),
        }
    }

    fn devices(&mut self) -> Result<Vec<String>> {
        match self.request(&Request::Devices)? {
            Response::Devices { devices } => Ok(devices),
            Response::Error { message } => bail!(message),
            other => bail!("réponse inattendue: {other:?}"),
        }
    }

    fn stats(&mut self) -> Result<(u64, u64)> {
        match self.request(&Request::Stats)? {
            Response::Stats { entries, bytes } => Ok((entries, bytes)),
            other => bail!("réponse inattendue: {other:?}"),
        }
    }

    fn sync_status(&mut self) -> Result<SyncStatusData> {
        match self.request(&Request::SyncStatus)? {
            Response::SyncStatus {
                device,
                enabled,
                server,
                connected,
                outbox,
                last_seq,
            } => Ok(SyncStatusData {
                device,
                enabled,
                server,
                connected,
                outbox,
                last_seq,
            }),
            other => bail!("réponse inattendue: {other:?}"),
        }
    }
}

#[derive(Clone)]
struct SyncStatusData {
    device: String,
    enabled: bool,
    server: Option<String>,
    connected: bool,
    outbox: u64,
    last_seq: i64,
}

/// Écran affiché dans le popup.
#[derive(PartialEq)]
enum Screen {
    List,
    Manage,
}

/// État de l'écran de gestion (rafraîchi périodiquement).
struct ManageState {
    sync_status: Option<SyncStatusData>,
    stats: Option<(u64, u64)>,
    /// Résultat du GET /v1/status, rempli par un thread de fond.
    server: Arc<Mutex<Option<Result<ServerStatus, String>>>>,
    /// URL d'un serveur détecté en local quand aucune sync n'est configurée.
    probe_local: Arc<Mutex<Option<String>>>,
    last_fetch: Option<Instant>,
    /// Formulaire de connexion (affiché tant qu'aucune sync n'est configurée).
    form_url: String,
    form_token: String,
    show_token: bool,
    /// L'URL a déjà été préremplie depuis `probe_local` (ne pas écraser la saisie).
    form_prefilled: bool,
    /// Résultat du bouton « Tester », rempli par un thread de fond.
    test: Arc<Mutex<Option<Result<ServerStatus, String>>>>,
    testing: bool,
    /// Résultat du bouton « Enregistrer » : chemin écrit, ou message d'erreur.
    saved: Option<Result<String, String>>,
}

impl Default for ManageState {
    fn default() -> Self {
        Self {
            sync_status: None,
            stats: None,
            server: Arc::new(Mutex::new(None)),
            probe_local: Arc::new(Mutex::new(None)),
            last_fetch: None,
            form_url: String::new(),
            form_token: String::new(),
            show_token: false,
            form_prefilled: false,
            test: Arc::new(Mutex::new(None)),
            testing: false,
            saved: None,
        }
    }
}

fn http_agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(3)))
        .build()
        .into()
}

fn fetch_server_status(cfg: &SyncConfig) -> Result<ServerStatus, String> {
    http_agent()
        .get(format!("{}/v1/status", cfg.server))
        .header("Authorization", format!("Bearer {}", cfg.token))
        .call()
        .map_err(|e| format!("serveur injoignable: {e}"))?
        .body_mut()
        .read_json()
        .map_err(|e| format!("réponse invalide: {e}"))
}

struct App {
    client: Option<Client>,
    error: Option<String>,
    query: String,
    entries: Vec<EntryMeta>,
    /// Machines présentes dans l'historique ; filtre actif (None = toutes).
    devices: Vec<String>,
    device_filter: Option<String>,
    selected: usize,
    /// Cache des miniatures chargées (id -> texture).
    thumbs: HashMap<String, Option<egui::TextureHandle>>,
    needs_refresh: bool,
    themed: bool,
    screen: Screen,
    manage: ManageState,
    sync_cfg: Option<SyncConfig>,
}

impl App {
    fn new() -> Self {
        let (mut client, error) = match Client::connect() {
            Ok(c) => (Some(c), None),
            Err(e) => (
                None,
                Some(format!("{e:#}\n\nLe daemon tourne-t-il ? (clipvault-daemon)")),
            ),
        };
        let devices = client
            .as_mut()
            .and_then(|c| c.devices().ok())
            .unwrap_or_default();
        Self {
            client,
            error,
            query: String::new(),
            entries: Vec::new(),
            devices,
            device_filter: None,
            selected: 0,
            thumbs: HashMap::new(),
            needs_refresh: true,
            themed: false,
            screen: Screen::List,
            manage: ManageState::default(),
            sync_cfg: clipvault_core::config::Config::load().sync,
        }
    }

    /// Rafraîchit les données de l'écran de gestion (toutes les 2 s).
    fn refresh_manage(&mut self) {
        let due = self
            .manage
            .last_fetch
            .is_none_or(|t| t.elapsed() > Duration::from_secs(2));
        if !due {
            return;
        }
        self.manage.last_fetch = Some(Instant::now());
        self.manage.sync_status = self.client.as_mut().and_then(|c| c.sync_status().ok());
        self.manage.stats = self.client.as_mut().and_then(|c| c.stats().ok());

        match self.sync_cfg.clone() {
            Some(cfg) => {
                let slot = Arc::clone(&self.manage.server);
                std::thread::spawn(move || {
                    let res = fetch_server_status(&cfg);
                    *slot.lock().unwrap() = Some(res);
                });
            }
            None => {
                // Pas de sync configurée : on regarde si un serveur tourne en local.
                let slot = Arc::clone(&self.manage.probe_local);
                std::thread::spawn(move || {
                    let url = "http://127.0.0.1:7700";
                    let found = http_agent().get(format!("{url}/v1/health")).call().is_ok();
                    *slot.lock().unwrap() = found.then(|| url.to_string());
                });
            }
        }
    }

    fn refresh(&mut self) {
        let device = self.device_filter.clone();
        if let Some(client) = self.client.as_mut() {
            match client.search(&self.query, device.as_deref()) {
                Ok(entries) => {
                    self.entries = entries;
                    self.error = None;
                }
                Err(e) => self.error = Some(e.to_string()),
            }
        }
        self.selected = self.selected.min(self.entries.len().saturating_sub(1));
        self.needs_refresh = false;
    }

    fn send(&mut self, req: Request) {
        if let Some(client) = self.client.as_mut() {
            match client.request(&req) {
                Ok(Response::Error { message }) => self.error = Some(message),
                Ok(_) => {}
                Err(e) => self.error = Some(e.to_string()),
            }
        }
    }

    fn activate_selected(&mut self, ctx: &egui::Context) {
        if let Some(entry) = self.entries.get(self.selected) {
            let id = entry.id.clone();
            self.send(Request::Activate { id });
            if self.error.is_none() {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
    }

    fn thumb_texture(
        &mut self,
        ctx: &egui::Context,
        entry: &EntryMeta,
    ) -> Option<egui::TextureHandle> {
        if let Some(cached) = self.thumbs.get(&entry.id) {
            return cached.clone();
        }
        let loaded = entry.thumb_path.as_ref().and_then(|p| {
            let img = image::open(p).ok()?.to_rgba8();
            let (w, h) = img.dimensions();
            let color =
                egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], img.as_raw());
            Some(ctx.load_texture(&entry.id, color, egui::TextureOptions::LINEAR))
        });
        self.thumbs.insert(entry.id.clone(), loaded.clone());
        loaded
    }

    fn handle_keys(&mut self, ctx: &egui::Context) {
        let (esc, enter, up, down, pin, del) = ctx.input(|i| {
            (
                i.key_pressed(egui::Key::Escape),
                i.key_pressed(egui::Key::Enter),
                i.key_pressed(egui::Key::ArrowUp),
                i.key_pressed(egui::Key::ArrowDown),
                i.modifiers.ctrl && i.key_pressed(egui::Key::P),
                i.key_pressed(egui::Key::Delete),
            )
        });
        let tab = ctx.input(|i| i.key_pressed(egui::Key::Tab));
        if tab && self.screen == Screen::List && self.devices.len() > 1 {
            self.cycle_device_filter();
        }
        if esc {
            if self.screen == Screen::Manage {
                self.screen = Screen::List;
            } else {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
        if self.screen != Screen::List {
            return;
        }
        if down && !self.entries.is_empty() {
            self.selected = (self.selected + 1).min(self.entries.len() - 1);
        }
        if up {
            self.selected = self.selected.saturating_sub(1);
        }
        if pin {
            if let Some(e) = self.entries.get(self.selected) {
                self.send(Request::SetPinned {
                    id: e.id.clone(),
                    pinned: !e.pinned,
                });
                self.needs_refresh = true;
            }
        }
        if del {
            if let Some(e) = self.entries.get(self.selected) {
                self.send(Request::Delete { id: e.id.clone() });
                self.needs_refresh = true;
            }
        }
        if enter {
            self.activate_selected(ctx);
        }
    }

    fn cycle_device_filter(&mut self) {
        let next = match &self.device_filter {
            None => self.devices.first().cloned(),
            Some(cur) => {
                let idx = self.devices.iter().position(|d| d == cur);
                idx.and_then(|i| self.devices.get(i + 1).cloned())
            }
        };
        self.device_filter = next;
        self.selected = 0;
        self.needs_refresh = true;
    }

    /// Couleur stable par machine (pour distinguer les sources d'un coup d'œil).
    fn device_color(name: &str) -> Color32 {
        const PALETTE: &[Color32] = &[
            Color32::from_rgb(0x89, 0xb4, 0xfa), // bleu
            Color32::from_rgb(0xa6, 0xe3, 0xa1), // vert
            Color32::from_rgb(0xfa, 0xb3, 0x87), // pêche
            Color32::from_rgb(0xcb, 0xa6, 0xf7), // mauve
            Color32::from_rgb(0x94, 0xe2, 0xd5), // sarcelle
            Color32::from_rgb(0xf5, 0xc2, 0xe7), // rose
        ];
        let h = name
            .bytes()
            .fold(0usize, |acc, b| acc.wrapping_mul(31).wrapping_add(b as usize));
        PALETTE[h % PALETTE.len()]
    }

    fn draw_device_chips(&mut self, ui: &mut egui::Ui) {
        if self.devices.len() < 2 {
            return;
        }
        let mut new_filter: Option<Option<String>> = None;
        ui.horizontal(|ui| {
            ui.add_space(16.0);
            let chip = |ui: &mut egui::Ui, label: &str, active: bool, color: Color32| -> bool {
                let text = RichText::new(label).font(FontId::proportional(12.0)).color(
                    if active { theme::BASE } else { color },
                );
                let fill = if active { color } else { theme::SURFACE0 };
                ui.add(
                    egui::Button::new(text)
                        .fill(fill)
                        .corner_radius(10.0)
                        .stroke(egui::Stroke::NONE),
                )
                .clicked()
            };
            if chip(ui, "Toutes", self.device_filter.is_none(), theme::SUBTEXT) {
                new_filter = Some(None);
            }
            for d in self.devices.clone() {
                let active = self.device_filter.as_deref() == Some(d.as_str());
                if chip(ui, &d, active, Self::device_color(&d)) {
                    new_filter = Some(Some(d.clone()));
                }
            }
            ui.label(
                RichText::new("tab")
                    .font(FontId::proportional(10.0))
                    .color(theme::OVERLAY),
            );
        });
        ui.add_space(6.0);
        if let Some(f) = new_filter {
            self.device_filter = f;
            self.selected = 0;
            self.needs_refresh = true;
        }
    }

    fn draw_search_bar(&mut self, ui: &mut egui::Ui) {
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            ui.add_space(18.0);
            ui.label(
                RichText::new("🔍")
                    .font(FontId::proportional(16.0))
                    .color(theme::OVERLAY),
            );
            ui.add_space(4.0);
            let search = ui.add(
                egui::TextEdit::singleline(&mut self.query)
                    .frame(egui::Frame::NONE)
                    .font(FontId::proportional(17.0))
                    .hint_text(
                        RichText::new("Rechercher dans l'historique…").color(theme::OVERLAY),
                    )
                    .desired_width(ui.available_width() - 46.0),
            );
            search.request_focus();
            if search.changed() {
                self.selected = 0;
                self.needs_refresh = true;
            }
            let gear = ui.add(
                egui::Button::new(
                    RichText::new("⚙")
                        .font(FontId::proportional(16.0))
                        .color(theme::OVERLAY),
                )
                .frame(false),
            );
            if gear.on_hover_text("Gestion (état, serveur, connexions)").clicked() {
                self.screen = Screen::Manage;
            }
        });
        ui.add_space(10.0);
        let line_y = ui.cursor().top();
        let rect = ui.max_rect();
        ui.painter().hline(
            rect.left()..=rect.right(),
            line_y,
            egui::Stroke::new(1.0, theme::SURFACE0),
        );
        ui.add_space(6.0);
    }

    fn draw_row(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, i: usize) -> bool {
        let entry = self.entries[i].clone();
        let is_selected = i == self.selected;

        let (rect, resp) = ui.allocate_exact_size(
            egui::vec2(ui.available_width(), ROW_HEIGHT),
            egui::Sense::click(),
        );
        if is_selected {
            ui.painter().rect_filled(rect, 8.0, theme::SURFACE0);
            let bar = egui::Rect::from_min_size(rect.min, egui::vec2(3.0, rect.height()));
            ui.painter().rect_filled(bar, 2.0, theme::ACCENT);
            resp.scroll_to_me(None);
        } else if resp.hovered() {
            ui.painter()
                .rect_filled(rect, 8.0, theme::SURFACE0.gamma_multiply(0.5));
        }

        // Icône de type, à gauche.
        let icon_color = if is_selected {
            theme::ACCENT
        } else {
            theme::OVERLAY
        };
        ui.painter().text(
            egui::pos2(rect.left() + 24.0, rect.center().y),
            Align2::CENTER_CENTER,
            kind_icon(entry.kind),
            FontId::proportional(15.0),
            icon_color,
        );
        let mut content_x = rect.left() + 44.0;

        // Miniature pour les images.
        if entry.kind == ContentKind::Image {
            if let Some(tex) = self.thumb_texture(ctx, &entry) {
                let size = tex.size_vec2();
                let h = ROW_HEIGHT - 12.0;
                let w = (size.x * h / size.y).min(72.0);
                let img_rect = egui::Rect::from_min_size(
                    egui::pos2(content_x, rect.center().y - h / 2.0),
                    egui::vec2(w, h),
                );
                ui.painter().add(
                    egui::Shape::image(
                        tex.id(),
                        img_rect,
                        egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                        Color32::WHITE,
                    ),
                );
                content_x += w + 10.0;
            }
        }

        // Zone droite : horodatage (+ épingle), largeur réservée.
        let time_galley = ui.painter().layout_no_wrap(
            ago(entry.last_used_at),
            FontId::proportional(11.5),
            theme::OVERLAY,
        );
        let mut right_x = rect.right() - 14.0 - time_galley.size().x;
        ui.painter().galley(
            egui::pos2(right_x, rect.center().y - time_galley.size().y / 2.0),
            time_galley,
            theme::OVERLAY,
        );
        if entry.pinned {
            right_x -= 22.0;
            ui.painter().text(
                egui::pos2(right_x + 8.0, rect.center().y),
                Align2::CENTER_CENTER,
                "📌",
                FontId::proportional(13.0),
                theme::PIN,
            );
        }

        // Étiquette machine, seulement quand plusieurs sources coexistent.
        if self.devices.len() > 1 {
            let dev_galley = ui.painter().layout_no_wrap(
                entry.device_id.clone(),
                FontId::proportional(11.0),
                Self::device_color(&entry.device_id),
            );
            right_x -= dev_galley.size().x + 14.0;
            ui.painter().galley(
                egui::pos2(right_x, rect.center().y - dev_galley.size().y / 2.0),
                dev_galley,
                theme::OVERLAY,
            );
        }

        // Aperçu, tronqué avant la zone droite.
        let max_w = (right_x - 10.0 - content_x).max(0.0);
        let text_color = if is_selected {
            Color32::WHITE
        } else {
            theme::TEXT
        };
        let mut job = egui::text::LayoutJob::simple_singleline(
            entry.preview.clone(),
            FontId::proportional(14.0),
            text_color,
        );
        job.wrap = egui::text::TextWrapping::truncate_at_width(max_w);
        let galley = ui.painter().layout_job(job);
        ui.painter().galley(
            egui::pos2(content_x, rect.center().y - galley.size().y / 2.0),
            galley,
            text_color,
        );

        resp.clicked()
    }

    fn draw_footer(&self, ui: &mut egui::Ui) {
        let rect = ui.max_rect();
        let y = rect.bottom() - 30.0;
        let center_y = y + 15.0;
        ui.painter().hline(
            rect.left()..=rect.right(),
            y,
            egui::Stroke::new(1.0, theme::SURFACE0),
        );

        // Raccourcis rendus comme des touches de clavier ("keycaps").
        let mut hints: Vec<(&str, &str)> = vec![
            ("↑↓", "naviguer"),
            ("entrée", "copier"),
            ("ctrl+p", "épingler"),
            ("suppr", "effacer"),
        ];
        if self.devices.len() > 1 {
            hints.push(("tab", "machine"));
        }
        let mut x = rect.left() + 16.0;
        for (key, action) in hints {
            let key_galley = ui.painter().layout_no_wrap(
                key.to_string(),
                FontId::proportional(10.5),
                theme::SUBTEXT,
            );
            let pad = 6.0;
            let key_rect = egui::Rect::from_min_size(
                egui::pos2(x, center_y - 9.0),
                egui::vec2(key_galley.size().x + pad * 2.0, 18.0),
            );
            ui.painter().rect_filled(key_rect, 4.0, theme::SURFACE0);
            ui.painter().rect_stroke(
                key_rect,
                4.0,
                egui::Stroke::new(1.0, theme::SURFACE1),
                egui::StrokeKind::Inside,
            );
            let key_size = key_galley.size();
            ui.painter().galley(
                egui::pos2(x + pad, center_y - key_size.y / 2.0),
                key_galley,
                theme::SUBTEXT,
            );
            x = key_rect.right() + 6.0;
            let action_galley = ui.painter().layout_no_wrap(
                action.to_string(),
                FontId::proportional(11.0),
                theme::OVERLAY,
            );
            let action_size = action_galley.size();
            ui.painter().galley(
                egui::pos2(x, center_y - action_size.y / 2.0),
                action_galley,
                theme::OVERLAY,
            );
            x += action_size.x + 16.0;
        }

        let count = format!("{} entrées", self.entries.len());
        ui.painter().text(
            egui::pos2(rect.right() - 18.0, center_y),
            Align2::RIGHT_CENTER,
            count,
            FontId::proportional(11.0),
            theme::OVERLAY,
        );
    }
}

impl App {
    fn draw_manage(&mut self, ui: &mut egui::Ui) {
        let inset = egui::Margin {
            left: 18,
            right: 18,
            top: 8,
            bottom: 8,
        };
        egui::Frame::new().inner_margin(inset).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new("Gestion")
                        .font(FontId::proportional(17.0))
                        .color(theme::TEXT),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(RichText::new("échap retour").font(FontId::proportional(11.0)).color(theme::OVERLAY));
                });
            });
            ui.add_space(10.0);

            // --- Cette machine ---
            section_title(ui, "Cette machine");
            match (&self.manage.sync_status, &self.manage.stats) {
                (Some(st), stats) => {
                    if let Some((entries, bytes)) = stats {
                        kv(ui, "Machine", &st.device, theme::TEXT);
                        kv(ui, "Historique", &format!("{entries} entrées · {}", fmt_bytes(*bytes)), theme::TEXT);
                    }
                    if st.enabled {
                        let (label, color) = if st.connected {
                            ("● connectée", theme::OK)
                        } else {
                            ("● hors-ligne (retente)", theme::ERROR)
                        };
                        kv(ui, "Sync", label, color);
                        if let Some(server) = &st.server {
                            kv(ui, "Serveur", server, theme::SUBTEXT);
                        }
                        kv(
                            ui,
                            "File d'envoi",
                            &if st.outbox == 0 {
                                "vide".to_string()
                            } else {
                                format!("{} en attente", st.outbox)
                            },
                            if st.outbox == 0 { theme::SUBTEXT } else { theme::PIN },
                        );
                        kv(ui, "Curseur reçu", &format!("seq {}", st.last_seq), theme::SUBTEXT);
                    } else {
                        kv(ui, "Sync", "désactivée (pas de [sync] dans config.toml)", theme::OVERLAY);
                    }
                }
                _ => {
                    ui.label(RichText::new("Daemon injoignable").color(theme::ERROR));
                }
            }
            ui.add_space(14.0);

            // --- Serveur ---
            section_title(ui, "Serveur");
            if self.sync_cfg.is_some() {
                let status = self.manage.server.lock().unwrap().clone();
                match status {
                    None => {
                        ui.label(RichText::new("interrogation…").color(theme::OVERLAY));
                    }
                    Some(Err(e)) => {
                        ui.label(RichText::new(e).color(theme::ERROR));
                    }
                    Some(Ok(s)) => {
                        kv(ui, "Version", &s.version, theme::SUBTEXT);
                        kv(ui, "En ligne depuis", &ago(s.started_at).replace("il y a ", ""), theme::SUBTEXT);
                        kv(ui, "Journal", &format!("{} événements", s.events), theme::TEXT);
                        kv(
                            ui,
                            "Objets",
                            &format!("{} · {}", s.objects, fmt_bytes(s.objects_bytes)),
                            theme::TEXT,
                        );
                        ui.add_space(6.0);
                        ui.label(
                            RichText::new(format!("Machines connectées ({})", s.clients.len()))
                                .font(FontId::proportional(12.5))
                                .color(theme::SUBTEXT),
                        );
                        for c in &s.clients {
                            ui.horizontal(|ui| {
                                ui.add_space(8.0);
                                ui.label(RichText::new("●").color(theme::OK).font(FontId::proportional(10.0)));
                                ui.label(
                                    RichText::new(&c.device)
                                        .color(Self::device_color(&c.device))
                                        .font(FontId::proportional(13.0)),
                                );
                                ui.label(
                                    RichText::new(format!("connectée {}", ago(c.connected_at)))
                                        .color(theme::OVERLAY)
                                        .font(FontId::proportional(11.5)),
                                );
                            });
                        }
                        if s.clients.is_empty() {
                            ui.label(RichText::new("aucune").color(theme::OVERLAY));
                        }
                    }
                }
            } else {
                self.draw_connect_form(ui);
            }
        });
    }
}

impl App {
    /// Formulaire de connexion, affiché tant qu'aucune section [sync] n'existe.
    fn draw_connect_form(&mut self, ui: &mut egui::Ui) {
        let probe = self.manage.probe_local.lock().unwrap().clone();
        // Prérempli une seule fois : la saisie de l'utilisateur prime ensuite.
        if let Some(url) = &probe {
            if !self.manage.form_prefilled && self.manage.form_url.trim().is_empty() {
                self.manage.form_url = url.clone();
                self.manage.form_prefilled = true;
            }
        }
        let hint = match &probe {
            Some(url) => format!("Serveur détecté en local ({url})."),
            None => "Aucun serveur configuré ni détecté en local.".to_string(),
        };
        ui.label(
            RichText::new(hint)
                .color(if probe.is_some() { theme::PIN } else { theme::OVERLAY })
                .font(FontId::proportional(12.5)),
        );
        ui.add_space(6.0);

        let field_width = (ui.available_width() - 128.0).max(120.0);
        ui.horizontal(|ui| {
            ui.add_sized(
                [120.0, 18.0],
                egui::Label::new(
                    RichText::new("URL du serveur")
                        .font(FontId::proportional(12.5))
                        .color(theme::OVERLAY),
                ),
            );
            ui.add(
                egui::TextEdit::singleline(&mut self.manage.form_url)
                    .hint_text("http://omarchie2:7700")
                    .desired_width(field_width),
            );
        });
        ui.horizontal(|ui| {
            ui.add_sized(
                [120.0, 18.0],
                egui::Label::new(
                    RichText::new("Jeton partagé")
                        .font(FontId::proportional(12.5))
                        .color(theme::OVERLAY),
                ),
            );
            ui.add(
                egui::TextEdit::singleline(&mut self.manage.form_token)
                    .password(!self.manage.show_token)
                    .hint_text("le même que CLIPVAULT_TOKEN côté serveur")
                    .desired_width(field_width),
            );
        });

        ui.add_space(8.0);
        let ready = !self.manage.form_url.trim().is_empty()
            && !self.manage.form_token.trim().is_empty();
        let (mut do_test, mut do_save) = (false, false);
        ui.horizontal(|ui| {
            ui.add_space(120.0);
            do_test = ui
                .add_enabled(ready && !self.manage.testing, egui::Button::new("Tester"))
                .clicked();
            do_save = ui
                .add_enabled(ready, egui::Button::new("Enregistrer"))
                .clicked();
            ui.add_space(6.0);
            ui.checkbox(&mut self.manage.show_token, "afficher le jeton");
        });

        if do_test {
            let cfg = self.form_sync_cfg();
            *self.manage.test.lock().unwrap() = None;
            self.manage.testing = true;
            let slot = Arc::clone(&self.manage.test);
            std::thread::spawn(move || {
                *slot.lock().unwrap() = Some(fetch_server_status(&cfg));
            });
        }
        if do_save {
            let cfg = self.form_sync_cfg();
            self.manage.saved = Some(
                clipvault_core::config::Config::save_sync(&cfg)
                    .map(|p| p.display().to_string()),
            );
        }

        ui.add_space(6.0);
        let test = self.manage.test.lock().unwrap().clone();
        if test.is_some() {
            self.manage.testing = false;
        }
        match test {
            Some(Ok(st)) => {
                ui.label(
                    RichText::new(format!(
                        "Connexion établie — serveur {}, {} événement(s), {} machine(s) connectée(s).",
                        st.version,
                        st.events,
                        st.clients.len()
                    ))
                    .color(theme::OK)
                    .font(FontId::proportional(12.5)),
                );
            }
            Some(Err(e)) => {
                ui.label(RichText::new(e).color(theme::ERROR).font(FontId::proportional(12.5)));
            }
            None if self.manage.testing => {
                ui.label(
                    RichText::new("test en cours…")
                        .color(theme::OVERLAY)
                        .font(FontId::proportional(12.5)),
                );
            }
            None => {}
        }

        match &self.manage.saved {
            Some(Ok(path)) => {
                ui.label(
                    RichText::new(format!("Enregistré dans {path}"))
                        .color(theme::OK)
                        .font(FontId::proportional(12.5)),
                );
                ui.label(
                    RichText::new("Redémarre le daemon pour activer la synchronisation.")
                        .color(theme::SUBTEXT)
                        .font(FontId::proportional(12.5)),
                );
            }
            Some(Err(e)) => {
                ui.label(RichText::new(e).color(theme::ERROR).font(FontId::proportional(12.5)));
            }
            None => {}
        }
    }

    /// La configuration décrite par le formulaire (valeurs nettoyées).
    fn form_sync_cfg(&self) -> SyncConfig {
        SyncConfig {
            server: self.manage.form_url.trim().to_string(),
            token: self.manage.form_token.trim().to_string(),
        }
    }
}

fn section_title(ui: &mut egui::Ui, title: &str) {
    ui.label(
        RichText::new(title.to_uppercase())
            .font(FontId::proportional(11.0))
            .color(theme::ACCENT),
    );
    ui.add_space(4.0);
}

fn kv(ui: &mut egui::Ui, key: &str, value: &str, color: Color32) {
    ui.horizontal(|ui| {
        ui.add_sized(
            [120.0, 18.0],
            egui::Label::new(
                RichText::new(key)
                    .font(FontId::proportional(12.5))
                    .color(theme::OVERLAY),
            ),
        );
        ui.label(RichText::new(value).font(FontId::proportional(13.0)).color(color));
    });
}

fn fmt_bytes(b: u64) -> String {
    match b {
        0..=1023 => format!("{b} o"),
        1024..=1048575 => format!("{:.1} Ko", b as f64 / 1024.0),
        1048576..=1073741823 => format!("{:.1} Mo", b as f64 / 1048576.0),
        _ => format!("{:.2} Go", b as f64 / 1073741824.0),
    }
}

fn ago(ts: i64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let d = (now - ts).max(0);
    match d {
        0..=59 => "à l'instant".into(),
        60..=3599 => format!("il y a {} min", d / 60),
        3600..=86_399 => format!("il y a {} h", d / 3600),
        _ => format!("il y a {} j", d / 86_400),
    }
}

fn kind_icon(kind: ContentKind) -> &'static str {
    match kind {
        ContentKind::Text => "📄",
        ContentKind::Image => "🖼",
        ContentKind::Binary => "📦",
    }
}

impl eframe::App for App {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        // Fond du viewport transparent : la fenêtre arrondie est dessinée par le panel.
        [0.0, 0.0, 0.0, 0.0]
    }

    fn ui(&mut self, root: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = root.ctx().clone();
        let ctx = &ctx;
        if !self.themed {
            theme::setup(ctx);
            self.themed = true;
        }
        if self.needs_refresh {
            self.refresh();
        }
        self.handle_keys(ctx);

        let frame = egui::Frame::new()
            .fill(theme::BASE)
            .stroke(egui::Stroke::new(1.0, theme::SURFACE1))
            .corner_radius(WINDOW_ROUNDING);
        egui::CentralPanel::default().frame(frame).show(root, |ui| {
            if self.screen == Screen::Manage {
                self.refresh_manage();
                ctx.request_repaint_after(Duration::from_millis(800));
                self.draw_manage(ui);
                return;
            }
            self.draw_search_bar(ui);
            self.draw_device_chips(ui);

            if let Some(err) = self.error.clone() {
                ui.add_space(30.0);
                ui.vertical_centered(|ui| {
                    ui.label(RichText::new(err).color(theme::ERROR));
                });
                return;
            }

            self.draw_footer(ui);
            let list_height = ui.available_height() - 34.0;

            let mut clicked: Option<usize> = None;
            egui::ScrollArea::vertical()
                .max_height(list_height)
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    ui.add_space(2.0);
                    let inset = egui::Margin {
                        left: 10,
                        right: 10,
                        top: 0,
                        bottom: 0,
                    };
                    egui::Frame::new().inner_margin(inset).show(ui, |ui| {
                        for i in 0..self.entries.len() {
                            if self.draw_row(ui, ctx, i) {
                                clicked = Some(i);
                            }
                        }
                        if self.entries.is_empty() {
                            ui.add_space(30.0);
                            ui.vertical_centered(|ui| {
                                ui.label(
                                    RichText::new("Aucune entrée").color(theme::OVERLAY),
                                );
                            });
                        }
                    });
                });
            if let Some(i) = clicked {
                self.selected = i;
                self.activate_selected(ctx);
            }
        });
    }
}

fn main() -> eframe::Result<()> {
    #[allow(unused_mut)]
    let mut options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_app_id("clipvault")
            .with_inner_size([700.0, 480.0])
            .with_decorations(false)
            .with_transparent(true)
            .with_resizable(false),
        ..Default::default()
    };
    // macOS : `Accessory` = pas d'icône dans le Dock ni dans Cmd+Tab (le popup
    // est éphémère), mais il faut alors demander explicitement le premier plan,
    // sinon la fenêtre s'ouvre derrière et ne reçoit pas les touches.
    #[cfg(target_os = "macos")]
    {
        use winit::platform::macos::{ActivationPolicy, EventLoopBuilderExtMacOS};
        options.event_loop_builder = Some(Box::new(|builder| {
            builder
                .with_activation_policy(ActivationPolicy::Accessory)
                .with_activate_ignoring_other_apps(true);
        }));
    }
    eframe::run_native(
        "clipvault",
        options,
        Box::new(|_cc| Ok(Box::new(App::new()))),
    )
}
