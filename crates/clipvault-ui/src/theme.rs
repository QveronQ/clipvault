//! Thème visuel du popup — palette sombre (Catppuccin Mocha) + fontes système.

use eframe::egui::{self, Color32};

pub const BASE: Color32 = Color32::from_rgb(0x1e, 0x1e, 0x2e);
pub const SURFACE0: Color32 = Color32::from_rgb(0x31, 0x32, 0x44);
pub const SURFACE1: Color32 = Color32::from_rgb(0x45, 0x47, 0x5a);
pub const TEXT: Color32 = Color32::from_rgb(0xcd, 0xd6, 0xf4);
pub const SUBTEXT: Color32 = Color32::from_rgb(0xa6, 0xad, 0xc8);
pub const OVERLAY: Color32 = Color32::from_rgb(0x6c, 0x70, 0x86);
pub const ACCENT: Color32 = Color32::from_rgb(0x89, 0xb4, 0xfa);
pub const OK: Color32 = Color32::from_rgb(0xa6, 0xe3, 0xa1);
pub const PIN: Color32 = Color32::from_rgb(0xf9, 0xe2, 0xaf);
pub const ERROR: Color32 = Color32::from_rgb(0xf3, 0x8b, 0xa8);

/// Fontes UI candidates, par ordre de préférence (fallback : fontes egui).
const FONT_CANDIDATES: &[&str] = &[
    // macOS : San Francisco (les chemins Linux ci-dessous n'existent pas, on les saute).
    "/System/Library/Fonts/SFNS.ttf",
    "/usr/share/fonts/inter/InterVariable.ttf",
    "/usr/share/fonts/TTF/Inter-Regular.ttf",
    "/usr/share/fonts/noto/NotoSans-Regular.ttf",
    "/usr/share/fonts/cantarell/Cantarell-VF.otf",
];

/// Fallback pour les glyphes absents de la fonte UI (flèches, ⏎, symboles Nerd Font).
const SYMBOL_FALLBACKS: &[&str] = &[
    // macOS : couvre flèches, ⏎ et la ponctuation technique des keycaps du footer.
    "/System/Library/Fonts/Apple Symbols.ttf",
    "/usr/share/fonts/TTF/JetBrainsMonoNerdFont-Regular.ttf",
    "/usr/share/fonts/noto/NotoSansSymbols-Regular.ttf",
];

pub fn setup(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    for path in FONT_CANDIDATES {
        if let Ok(bytes) = std::fs::read(path) {
            fonts
                .font_data
                .insert("ui".into(), egui::FontData::from_owned(bytes).into());
            fonts
                .families
                .get_mut(&egui::FontFamily::Proportional)
                .unwrap()
                .insert(0, "ui".into());
            break;
        }
    }
    for path in SYMBOL_FALLBACKS {
        if let Ok(bytes) = std::fs::read(path) {
            fonts
                .font_data
                .insert("symbols".into(), egui::FontData::from_owned(bytes).into());
            for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
                fonts
                    .families
                    .get_mut(&family)
                    .unwrap()
                    .push("symbols".into());
            }
            break;
        }
    }
    ctx.set_fonts(fonts);

    ctx.all_styles_mut(|style| {
        let v = &mut style.visuals;
        v.dark_mode = true;
        v.override_text_color = Some(TEXT);
        v.panel_fill = BASE;
        v.window_fill = BASE;
        v.extreme_bg_color = BASE;
        v.selection.bg_fill = ACCENT.gamma_multiply(0.35);
        v.selection.stroke = egui::Stroke::new(1.0, ACCENT);
        v.text_cursor.stroke = egui::Stroke::new(2.0, ACCENT);
        v.widgets.noninteractive.bg_stroke = egui::Stroke::new(1.0, SURFACE0);

        style.spacing.item_spacing = egui::vec2(8.0, 4.0);
        style.spacing.scroll = egui::style::ScrollStyle::thin();
    });
}
