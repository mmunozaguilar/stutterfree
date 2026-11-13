//! Colors, fonts and the global style: the same dark palette as the HTML UI.

use eframe::egui::{self, Color32, FontData, FontDefinitions, FontFamily, FontId, Stroke, TextStyle};
use std::sync::Arc;

pub const BG: Color32 = Color32::from_rgb(0x0d, 0x11, 0x17);
pub const HEADER: Color32 = Color32::from_rgb(0x11, 0x16, 0x1d);
pub const CARD: Color32 = Color32::from_rgb(0x16, 0x1b, 0x22);
pub const BORDER: Color32 = Color32::from_rgb(0x30, 0x36, 0x3d);
pub const SURFACE: Color32 = Color32::from_rgb(0x21, 0x26, 0x2d);
pub const TEXT: Color32 = Color32::from_rgb(0xf0, 0xf6, 0xfc);
pub const MUTED: Color32 = Color32::from_rgb(0x8b, 0x94, 0x9e);
pub const ACCENT: Color32 = Color32::from_rgb(0x58, 0xa6, 0xff);
pub const GREEN: Color32 = Color32::from_rgb(0x23, 0x86, 0x36);
pub const GREEN_HOVER: Color32 = Color32::from_rgb(0x2e, 0xa0, 0x43);
pub const GREEN_TEXT: Color32 = Color32::from_rgb(0x3f, 0xb9, 0x50);
pub const ORANGE: Color32 = Color32::from_rgb(0xd2, 0x99, 0x22);
pub const RED: Color32 = Color32::from_rgb(0xf8, 0x51, 0x49);
pub const PURPLE: Color32 = Color32::from_rgb(0xbc, 0x8c, 0xff);

/// The system's sans-serif (regular, bold) through fontconfig, so text matches the desktop; None without fontconfig.
fn system_font(pattern: &str) -> Option<Vec<u8>> {
    let out = std::process::Command::new("fc-match").args(["-f", "%{file}", pattern]).output().ok()?;
    let path = String::from_utf8(out.stdout).ok()?;
    let p = std::path::Path::new(path.trim());
    let ext = p.extension()?.to_string_lossy().to_ascii_lowercase();
    if !(ext == "ttf" || ext == "otf") {
        return None; // egui reads TrueType / OpenType only
    }
    std::fs::read(p).ok()
}

pub fn apply(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();
    let regular = system_font("sans-serif:weight=regular");
    let bold = system_font("sans-serif:weight=bold");
    if let Some(r) = regular {
        fonts.font_data.insert("system".into(), Arc::new(FontData::from_owned(r)));
        fonts.families.get_mut(&FontFamily::Proportional).unwrap().insert(0, "system".into());
    }
    let mut bold_family = fonts.families.get(&FontFamily::Proportional).cloned().unwrap_or_default();
    if let Some(b) = bold {
        fonts.font_data.insert("system-bold".into(), Arc::new(FontData::from_owned(b)));
        bold_family.insert(0, "system-bold".into());
    }
    fonts.families.insert(FontFamily::Name("bold".into()), bold_family);
    ctx.set_fonts(fonts);

    ctx.all_styles_mut(|s| {
        s.text_styles = [
            (TextStyle::Small, FontId::proportional(11.0)),
            (TextStyle::Body, FontId::proportional(14.0)),
            (TextStyle::Button, FontId::proportional(14.0)),
            (TextStyle::Heading, FontId::new(20.0, FontFamily::Name("bold".into()))),
            (TextStyle::Monospace, FontId::monospace(12.5)),
        ]
        .into();
        s.spacing.item_spacing = egui::vec2(8.0, 6.0);
        s.spacing.button_padding = egui::vec2(14.0, 7.0);
        s.spacing.scroll = egui::style::ScrollStyle::solid();
        let v = &mut s.visuals;
        *v = egui::Visuals::dark();
        v.panel_fill = BG;
        v.window_fill = CARD;
        v.extreme_bg_color = BG;
        v.faint_bg_color = SURFACE;
        v.override_text_color = None;
        v.hyperlink_color = ACCENT;
        v.selection.bg_fill = ACCENT.gamma_multiply(0.35);
        v.selection.stroke = Stroke::new(1.0, ACCENT);
        v.window_stroke = Stroke::new(1.0, BORDER);
        v.widgets.inactive.bg_fill = SURFACE;
        v.widgets.inactive.weak_bg_fill = SURFACE;
        v.widgets.inactive.bg_stroke = Stroke::new(1.0, BORDER);
        v.widgets.inactive.fg_stroke = Stroke::new(1.0, TEXT);
        v.widgets.hovered.bg_fill = Color32::from_rgb(0x2a, 0x31, 0x39);
        v.widgets.hovered.weak_bg_fill = Color32::from_rgb(0x2a, 0x31, 0x39);
        v.widgets.hovered.bg_stroke = Stroke::new(1.0, Color32::from_rgb(0x48, 0x4f, 0x58));
        v.widgets.active.bg_fill = Color32::from_rgb(0x30, 0x36, 0x3d);
        v.widgets.active.weak_bg_fill = Color32::from_rgb(0x30, 0x36, 0x3d);
        v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, BORDER);
        v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, MUTED);
        v.text_edit_bg_color = Some(BG);
    });
}
