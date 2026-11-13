//! The UI's building blocks: buttons, filter chips, toggles, dialogs and the game card.

use crate::theme::*;
use eframe::egui::{
    self, epaint::RectShape, load::TexturePoll, text::LayoutJob, Align, Color32, CornerRadius, FontFamily, FontId, Layout, Margin, Mesh, Pos2, Rect,
    Response, RichText, Sense, Stroke, StrokeKind, TextFormat, TextureOptions, Ui, Vec2,
};
use stutterfree_core::models::{CacheState, CompilationProgress, GameInfo, GameSource};
use stutterfree_core::util::{format_bytes, n0};

pub fn bold(text: &str, size: f32) -> RichText {
    RichText::new(text).size(size).family(FontFamily::Name("bold".into()))
}

#[derive(Clone, Copy)]
pub enum Kind {
    Primary,
    Secondary,
    Danger,
    Recording,
}

struct ButtonLook {
    fill: Color32,
    hover: Color32,
    stroke: Color32,
    text: Color32,
}

fn look(kind: Kind) -> ButtonLook {
    match kind {
        Kind::Primary => ButtonLook { fill: GREEN, hover: GREEN_HOVER, stroke: Color32::from_rgb(0x2e, 0xa0, 0x43), text: Color32::WHITE },
        Kind::Secondary => ButtonLook { fill: SURFACE, hover: Color32::from_rgb(0x30, 0x36, 0x3d), stroke: BORDER, text: TEXT },
        Kind::Danger => ButtonLook { fill: RED.gamma_multiply(0.12), hover: RED.gamma_multiply(0.22), stroke: RED.gamma_multiply(0.45), text: RED },
        Kind::Recording => ButtonLook { fill: SURFACE, hover: Color32::from_rgb(0x30, 0x36, 0x3d), stroke: PURPLE, text: PURPLE },
    }
}

fn styled_button(ui: &mut Ui, text: &str, kind: Kind, size: f32, height: f32, enabled: bool) -> Response {
    let l = look(kind);
    ui.scope(|ui| {
        let w = &mut ui.style_mut().visuals.widgets;
        w.inactive.weak_bg_fill = l.fill;
        w.inactive.bg_stroke = Stroke::new(1.0, l.stroke);
        w.hovered.weak_bg_fill = l.hover;
        w.hovered.bg_stroke = Stroke::new(1.0, l.stroke);
        w.active.weak_bg_fill = l.hover;
        w.active.bg_stroke = Stroke::new(1.0, l.stroke);
        let label = bold(text, size).color(l.text);
        ui.add_enabled(enabled, egui::Button::new(label).corner_radius(6).min_size(Vec2::new(0.0, height)))
    })
    .inner
    .on_hover_cursor(egui::CursorIcon::PointingHand)
}

pub fn button(ui: &mut Ui, text: &str, kind: Kind) -> Response {
    styled_button(ui, text, kind, 14.0, 36.0, true)
}

fn small_button(ui: &mut Ui, text: &str, kind: Kind, enabled: bool) -> Response {
    styled_button(ui, text, kind, 13.0, 32.0, enabled)
}

/// A launcher / filter pill with its count.
pub fn chip(ui: &mut Ui, name: &str, count: usize, active: bool) -> Response {
    let mut job = LayoutJob::default();
    let fg = if active { ACCENT } else { TEXT };
    job.append(name, 0.0, TextFormat { font_id: FontId::new(13.5, FontFamily::Name("bold".into())), color: fg, ..Default::default() });
    job.append(&format!("  {count}"), 0.0, TextFormat { font_id: FontId::proportional(12.5), color: MUTED, ..Default::default() });
    ui.scope(|ui| {
        let w = &mut ui.style_mut().visuals.widgets;
        let (fill, stroke) = if active { (ACCENT.gamma_multiply(0.15), ACCENT.gamma_multiply(0.7)) } else { (SURFACE, BORDER) };
        w.inactive.weak_bg_fill = fill;
        w.inactive.bg_stroke = Stroke::new(1.0, stroke);
        w.hovered.weak_bg_fill = if active { fill } else { Color32::from_rgb(0x2a, 0x31, 0x39) };
        w.hovered.bg_stroke = Stroke::new(1.0, if active { stroke } else { Color32::from_rgb(0x48, 0x4f, 0x58) });
        ui.add(egui::Button::new(job).corner_radius(16).min_size(Vec2::new(0.0, 32.0)))
    })
    .inner
    .on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// An on/off switch.
pub fn toggle(ui: &mut Ui, on: &mut bool) -> Response {
    let (rect, mut response) = ui.allocate_exact_size(Vec2::new(46.0, 26.0), Sense::click());
    if response.clicked() {
        *on = !*on;
        response.mark_changed();
    }
    let t = ui.ctx().animate_bool_responsive(response.id, *on);
    let radius = rect.height() / 2.0;
    let fill = Color32::from_rgb(
        egui::lerp(SURFACE.r() as f32..=GREEN.r() as f32, t) as u8,
        egui::lerp(SURFACE.g() as f32..=GREEN.g() as f32, t) as u8,
        egui::lerp(SURFACE.b() as f32..=GREEN.b() as f32, t) as u8,
    );
    ui.painter().rect(rect, radius, fill, Stroke::new(1.0, BORDER), StrokeKind::Inside);
    let x = egui::lerp((rect.left() + radius)..=(rect.right() - radius), t);
    ui.painter().circle_filled(Pos2::new(x, rect.center().y), radius - 4.0, Color32::WHITE);
    response.on_hover_cursor(egui::CursorIcon::PointingHand)
}

pub fn modal_frame() -> egui::Frame {
    egui::Frame::new()
        .fill(CARD)
        .stroke(Stroke::new(1.0, BORDER))
        .corner_radius(12)
        .inner_margin(Margin::same(22))
        .shadow(egui::Shadow { offset: [0, 16], blur: 48, spread: 0, color: Color32::from_black_alpha(150) })
}

/// The dialog's title row; true when its "✕ Close" was clicked.
pub fn modal_title(ui: &mut Ui, title: &str) -> bool {
    let close = ui
        .horizontal(|ui| {
            ui.label(bold(title, 18.0).color(TEXT));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| small_button(ui, "✕ Close", Kind::Secondary, true).clicked()).inner
        })
        .inner;
    ui.add_space(6.0);
    ui.separator();
    ui.add_space(8.0);
    close
}

pub fn setting_row(ui: &mut Ui, title: &str, note: &str, desc: &str, control: impl FnOnce(&mut Ui)) {
    egui::Frame::new().fill(BG).stroke(Stroke::new(1.0, BORDER)).corner_radius(10).inner_margin(Margin::same(14)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal_top(|ui| {
            let text_w = (ui.available_width() - 170.0).max(200.0);
            ui.allocate_ui_with_layout(Vec2::new(text_w, 0.0), Layout::top_down(Align::Min), |ui| {
                ui.set_width(text_w);
                ui.horizontal_wrapped(|ui| {
                    ui.label(bold(title, 14.5).color(TEXT));
                    ui.label(RichText::new(note).size(11.0).color(MUTED));
                });
                ui.label(RichText::new(desc).size(12.5).color(MUTED));
            });
            ui.with_layout(Layout::right_to_left(Align::Min), control);
        });
    });
    ui.add_space(8.0);
}

// ---- the game card -----------------------------------------------------------------------------------------------------

pub enum CardAction {
    Compile,
    ClearCache,
    ToggleRecorder,
}

fn pill(ui: &mut Ui, text: &str, fg: Color32, tip: Option<&str>) {
    let r = egui::Frame::new()
        .fill(fg.gamma_multiply(0.16))
        .stroke(Stroke::new(1.0, fg.gamma_multiply(0.45)))
        .corner_radius(6)
        .inner_margin(Margin::symmetric(10, 4))
        .show(ui, |ui| ui.label(bold(text, 12.5).color(fg)));
    if let Some(t) = tip {
        r.response.on_hover_text(t);
    }
}

fn badge(ui: &mut Ui, text: &str, fg: Color32, bg: Color32, border: Color32) {
    egui::Frame::new()
        .fill(bg)
        .stroke(Stroke::new(1.0, border))
        .corner_radius(12)
        .inner_margin(Margin::symmetric(9, 2))
        .show(ui, |ui| ui.label(bold(&text.to_uppercase(), 11.0).color(fg)));
}

/// The texture coordinates that crop an image of `tex` size to fill `rect` (CSS object-fit: cover), its outer texels
/// left out: Steam headers often end in a light column or two (Wanderburg's, Rift of the NecroDancer's) that reads as a seam.
fn cover_uv(tex: Vec2, rect: Vec2) -> Rect {
    let inset = Vec2::new(6.0 / tex.x.max(1.0), 2.0 / tex.y.max(1.0)).min(Vec2::splat(0.1));
    let tex = tex * (Vec2::splat(1.0) - 2.0 * inset); // the part kept
    let ta = tex.x / tex.y.max(1.0);
    let ra = rect.x / rect.y.max(1.0);
    let (w, h) = if ta > ra { (ra / ta, 1.0) } else { (1.0, ta / ra) };
    let half = Vec2::new(w, h) * (Vec2::splat(0.5) - inset);
    Rect::from_center_size(Pos2::new(0.5, 0.5), half * 2.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cover_crops_the_edges_and_keeps_the_aspect() {
        let uv = cover_uv(Vec2::new(460.0, 215.0), Vec2::new(800.0, 150.0));
        assert!(uv.left() >= 6.0 / 460.0 - 1e-6 && uv.right() <= 1.0 - 6.0 / 460.0 + 1e-6);
        let shown = Vec2::new(uv.width() * 460.0, uv.height() * 215.0);
        assert!((shown.x / shown.y - 800.0 / 150.0).abs() < 1e-3); // no stretching
    }
}

fn banner(ui: &mut Ui, g: &GameInfo, width: f32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, 150.0), Sense::hover());
    // on whole pixels: a fractional edge column gets the image's anti-aliased edge but not the fade, and flickers on scroll
    let rect = egui::emath::GuiRounding::round_to_pixels(rect, ui.pixels_per_point());
    let painter = ui.painter_at(rect);
    let cr = CornerRadius { nw: 10, ne: 10, sw: 0, se: 0 };
    let mut drew = false;
    if let Some(p) = &g.banner_path {
        let uri = format!("file://{p}");
        if let Ok(TexturePoll::Ready { texture }) = ui.ctx().try_load_texture(&uri, TextureOptions::LINEAR, Default::default()) {
            painter.add(RectShape::filled(rect, cr, Color32::WHITE).with_texture(texture.id, cover_uv(texture.size, rect.size())));
            drew = true;
        }
    }
    if !drew {
        painter.rect_filled(rect, cr, Color32::from_rgb(0x1f, 0x2a, 0x44));
        let mut m = Mesh::default();
        let (a, b) = (Color32::from_rgb(0x1f, 0x2a, 0x44).gamma_multiply(0.0), Color32::from_rgb(0x2d, 0x1f, 0x4a));
        m.colored_vertex(rect.left_top() + Vec2::new(0.0, 10.0), a);
        m.colored_vertex(rect.right_top() + Vec2::new(0.0, 10.0), b);
        m.colored_vertex(rect.right_bottom(), b);
        m.colored_vertex(rect.left_bottom(), a);
        m.add_triangle(0, 1, 2);
        m.add_triangle(0, 2, 3);
        painter.add(m);
        let initial = g.name.trim().chars().next().map(|c| c.to_uppercase().to_string()).unwrap_or_else(|| "?".into());
        painter.text(rect.center(), egui::Align2::CENTER_CENTER, initial, FontId::new(56.0, FontFamily::Name("bold".into())), Color32::from_white_alpha(34));
    }
    // fade into the card body; 1 px wider than the image on each side (the painter clips it) so no edge column escapes it
    let fade_top = rect.top() + rect.height() * 0.55;
    let rect = rect.expand2(Vec2::new(1.0, 0.0));
    let mut m = Mesh::default();
    let clear = Color32::from_rgba_unmultiplied(CARD.r(), CARD.g(), CARD.b(), 0);
    m.colored_vertex(Pos2::new(rect.left(), fade_top), clear);
    m.colored_vertex(Pos2::new(rect.right(), fade_top), clear);
    m.colored_vertex(rect.right_bottom(), CARD);
    m.colored_vertex(rect.left_bottom(), CARD);
    m.add_triangle(0, 1, 2);
    m.add_triangle(0, 2, 3);
    painter.add(m);
}

fn meta_row(ui: &mut Ui, label: &str, value: RichText) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(label).size(13.5).color(MUTED));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| ui.label(value));
    });
}

fn source_badge(g: &GameInfo) -> (String, Color32, Color32) {
    let (name, fg, bg) = match g.source {
        GameSource::Steam => ("Steam", Color32::from_rgb(0x66, 0xc0, 0xf4), Color32::from_rgb(0x1b, 0x28, 0x38)),
        GameSource::Heroic => ("Heroic", Color32::from_rgb(0xf4, 0x7b, 0xcf), Color32::from_rgb(0x33, 0x15, 0x2c)),
        _ => ("Lutris", Color32::from_rgb(0xf5, 0xa6, 0x23), Color32::from_rgb(0x38, 0x24, 0x0f)),
    };
    (if g.is_flatpak { format!("{name} (Flatpak)") } else { name.to_string() }, fg, bg)
}

/// One game's card; returns the action the user clicked.
pub fn game_card(ui: &mut Ui, g: &GameInfo, progress: Option<&CompilationProgress>, compiling_any: bool, width: f32) -> Option<CardAction> {
    let id = egui::Id::unique(("card", &g.id));
    let hovered = ui.ctx().data(|d| d.get_temp::<bool>(id)).unwrap_or(false);
    let mut action = None;
    let resp = egui::Frame::new()
        .fill(CARD)
        .stroke(Stroke::new(1.0, if hovered { ACCENT.gamma_multiply(0.4) } else { BORDER }))
        .corner_radius(10)
        .show(ui, |ui| {
            ui.set_width(width);
            ui.spacing_mut().item_spacing.y = 0.0;
            banner(ui, g, width);
            egui::Frame::new().inner_margin(Margin { left: 16, right: 16, top: 2, bottom: 16 }).show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 8.0;
                let cached = g.state == CacheState::Cached;
                let stale = g.state == CacheState::Outdated;
                // status pill: a compile in this session wins over the scanned state
                let (status, color, tip): (String, Color32, Option<&str>) = match progress {
                    Some(p) if !p.is_complete => ("Compiling...".into(), ACCENT, None),
                    Some(p) if p.failed => ("Action Needed".into(), ORANGE, None),
                    Some(p) if !p.cache_verified => ("Couldn't Confirm".into(), ORANGE, None),
                    Some(_) => ("Cache Installed".into(), GREEN_TEXT, None),
                    None if cached && g.new_recorded_psos > 0 => (format!("Cached (+{} new)", n0(g.new_recorded_psos as i64)), GREEN_TEXT, None),
                    None if cached => ("Cached (Ready)".into(), GREEN_TEXT, None),
                    None if stale && g.stale_reason.as_deref() == Some("cache truncated") => (
                        "Stale (>2 GB Truncated)".into(),
                        Color32::from_rgb(0xe3, 0xb3, 0x41),
                        Some("Legacy single-file NVIDIA cache hit NVIDIA's ~2 GB limit. Click Re-Compile once to split into multi-part cache files."),
                    ),
                    None if stale => (
                        g.stale_reason.as_ref().map(|r| format!("Stale ({r})")).unwrap_or_else(|| "Stale".into()),
                        Color32::from_rgb(0xe3, 0xb3, 0x41),
                        None,
                    ),
                    None => ("Needs Pre-Compilation".into(), ORANGE, None),
                };
                ui.horizontal_top(|ui| {
                    let left_w = (ui.available_width() - 175.0).max(120.0);
                    ui.allocate_ui_with_layout(Vec2::new(left_w, 0.0), Layout::top_down(Align::Min), |ui| {
                        ui.set_width(left_w);
                        ui.label(bold(&g.name, 17.0).color(TEXT));
                        ui.horizontal_wrapped(|ui| {
                            ui.spacing_mut().item_spacing.x = 6.0;
                            let (src, fg, bg) = source_badge(g);
                            badge(ui, &src, fg, bg, fg.gamma_multiply(0.33));
                            badge(ui, g.engine.name(), MUTED, SURFACE, BORDER);
                        });
                    });
                    ui.with_layout(Layout::right_to_left(Align::Min), |ui| pill(ui, &status, color, tip));
                });
                ui.add_space(2.0);
                meta_row(ui, "Estimated Shaders:", bold(&format!("~{}", n0(g.total_shaders_estimated.max(0) as i64)), 13.5).color(TEXT));
                meta_row(
                    ui,
                    "On-Disk Shader Cache:",
                    bold(&format_bytes(g.cache_size_bytes), 13.5).color(if g.cache_size_bytes > 0 { GREEN_TEXT } else { TEXT }),
                );
                let mut rec = n0(g.recorded_psos as i64);
                if g.new_recorded_psos > 0 {
                    rec.push_str(&format!(" (+{} new)", n0(g.new_recorded_psos as i64)));
                }
                if g.recorder_installed {
                    rec.push_str(" • recording");
                }
                meta_row(ui, "Recorded Pipelines:", bold(&rec, 13.5).color(if g.recorded_psos > 0 { PURPLE } else { TEXT }));
                if let Some(plan) = &g.plan_label {
                    meta_row(ui, "From Game Files:", bold(plan, 13.5).color(ACCENT));
                }
                if let (Some(note), true) = (&g.compile_note, cached || stale) {
                    ui.label(RichText::new(format!("⚠ {note}")).size(12.0).color(ORANGE));
                }
                if let Some(p) = progress {
                    progress_box(ui, p);
                }
                ui.add_space(4.0);
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.spacing_mut().item_spacing.x = 8.0;
                    let label = if cached || stale || progress.map(|p| p.is_complete && !p.failed).unwrap_or(false) { "⚡ Re-Compile" } else { "⚡ Kill Stutter" };
                    if small_button(ui, label, Kind::Primary, !compiling_any).clicked() {
                        action = Some(CardAction::Compile);
                    }
                    let (rec_label, rec_kind) = if g.recorder_installed { ("● Recording (on)", Kind::Recording) } else { ("● Record", Kind::Secondary) };
                    if small_button(ui, rec_label, rec_kind, true)
                        .on_hover_text(
                            "Record mode: a d3d12.dll next to the game records every pipeline it creates while you play, so the next compile covers exactly what the game uses",
                        )
                        .clicked()
                    {
                        action = Some(CardAction::ToggleRecorder);
                    }
                    if g.cache_size_bytes > 0
                        && small_button(ui, "🗑 Clear Cache", Kind::Danger, !compiling_any)
                            .on_hover_text("Delete compiled shader cache on disk for this game")
                            .clicked()
                    {
                        action = Some(CardAction::ClearCache);
                    }
                });
            });
        });
    let now_hovered = resp.response.contains_pointer();
    if now_hovered != hovered {
        ui.ctx().data_mut(|d| d.insert_temp(id, now_hovered));
        ui.ctx().request_repaint();
    }
    action
}

fn progress_box(ui: &mut Ui, p: &CompilationProgress) {
    let color = if p.failed {
        RED
    } else if p.is_complete {
        if p.cache_verified {
            GREEN_TEXT
        } else {
            ORANGE
        }
    } else {
        ACCENT
    };
    egui::Frame::new().fill(BG).stroke(Stroke::new(1.0, BORDER)).corner_radius(8).inner_margin(Margin::same(10)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.spacing_mut().item_spacing.y = 6.0;
        let (track, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 6.0), Sense::hover());
        ui.painter().rect_filled(track, 3.0, SURFACE);
        let frac = (p.percentage / 100.0).clamp(0.0, 1.0) as f32;
        let mut fill = track;
        fill.set_width(track.width() * if p.is_complete && !p.failed { 1.0 } else { frac });
        ui.painter().rect_filled(fill, 3.0, color);
        let text_color = if p.is_complete || p.failed { color } else { TEXT };
        let right = if p.is_complete {
            if p.failed {
                "✗ Failed".to_string()
            } else {
                "✓ Done".to_string()
            }
        } else {
            format!("{}%", p.percentage)
        };
        ui.horizontal_top(|ui| {
            let w = (ui.available_width() - 70.0).max(80.0);
            ui.allocate_ui_with_layout(Vec2::new(w, 0.0), Layout::top_down(Align::Min), |ui| {
                ui.set_width(w);
                ui.label(RichText::new(&p.current_action).size(12.5).color(text_color));
            });
            ui.with_layout(Layout::right_to_left(Align::Min), |ui| ui.label(bold(&right, 12.5).color(text_color)));
        });
        if !p.is_complete {
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(250));
        }
    });
}
