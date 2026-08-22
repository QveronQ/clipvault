//! clipvault — popup d'historique du presse-papier (egui).
//! Client léger : tout l'état vit dans le daemon, interrogé via socket Unix.

mod theme;

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;

use anyhow::{bail, Context, Result};
use clipvault_core::ipc::{Request, Response};
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
        if tab && self.devices.len() > 1 {
            self.cycle_device_filter();
        }
        if esc {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
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
                    .desired_width(ui.available_width() - 18.0),
            );
            search.request_focus();
            if search.changed() {
                self.selected = 0;
                self.needs_refresh = true;
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
        ui.painter().hline(
            rect.left()..=rect.right(),
            y,
            egui::Stroke::new(1.0, theme::SURFACE0),
        );
        let hints = if self.devices.len() > 1 {
            "↑↓ naviguer    ⏎ copier    ctrl+p épingler    suppr effacer    tab machine"
        } else {
            "↑↓ naviguer    ⏎ copier    ctrl+p épingler    suppr effacer"
        };
        ui.painter().text(
            egui::pos2(rect.left() + 18.0, y + 15.0),
            Align2::LEFT_CENTER,
            hints,
            FontId::proportional(11.0),
            theme::OVERLAY,
        );
        let count = format!("{} entrées", self.entries.len());
        ui.painter().text(
            egui::pos2(rect.right() - 18.0, y + 15.0),
            Align2::RIGHT_CENTER,
            count,
            FontId::proportional(11.0),
            theme::OVERLAY,
        );
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
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_app_id("clipvault")
            .with_inner_size([700.0, 480.0])
            .with_decorations(false)
            .with_transparent(true)
            .with_resizable(false),
        ..Default::default()
    };
    eframe::run_native(
        "clipvault",
        options,
        Box::new(|_cc| Ok(Box::new(App::new()))),
    )
}
