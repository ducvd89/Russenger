//! Telegram-desktop-like palettes (dark primary) and font helpers.
use eframe::egui::{self, Color32, FontFamily, FontId};

pub struct Palette {
    pub chat_bg: Color32,
    /// far-left tab rail
    pub rail: Color32,
    pub sidebar: Color32,
    pub row_hover: Color32,
    pub row_selected: Color32,
    pub accent: Color32,
    pub bubble_in: Color32,
    pub bubble_out: Color32,
    pub text: Color32,
    /// text on outgoing bubbles
    pub text_out: Color32,
    pub secondary: Color32,
    pub time_out: Color32,
    pub link: Color32,
    pub unread: Color32,
    pub unread_muted: Color32,
    pub separator: Color32,
    pub input: Color32,
    pub danger: Color32,
    pub chip: Color32,
}

const fn c(r: u8, g: u8, b: u8) -> Color32 {
    Color32::from_rgb(r, g, b)
}

static DARK: Palette = Palette {
    chat_bg: c(0x0E, 0x16, 0x21),
    rail: c(0x0B, 0x11, 0x1A),
    sidebar: c(0x17, 0x21, 0x2B),
    row_hover: c(0x20, 0x2B, 0x36),
    row_selected: c(0x18, 0x5C, 0xC8),
    accent: c(0x18, 0x77, 0xF2),
    bubble_in: c(0x18, 0x25, 0x33),
    bubble_out: c(0x08, 0x66, 0xFF),
    text: c(0xF5, 0xF5, 0xF5),
    text_out: c(0xFF, 0xFF, 0xFF),
    secondary: c(0x7F, 0x91, 0xA4),
    time_out: c(0xCF, 0xE0, 0xFF),
    link: c(0x6C, 0xB0, 0xFF),
    unread: c(0x18, 0x77, 0xF2),
    unread_muted: c(0x3E, 0x54, 0x6A),
    separator: c(0x0A, 0x12, 0x1B),
    input: c(0x24, 0x2F, 0x3D),
    danger: c(0xE5, 0x39, 0x35),
    chip: Color32::from_rgba_premultiplied(0, 0, 0, 90),
};

static LIGHT: Palette = Palette {
    chat_bg: c(0xE6, 0xEB, 0xEE),
    rail: c(0xF3, 0xF5, 0xF7),
    sidebar: c(0xFF, 0xFF, 0xFF),
    row_hover: c(0xF1, 0xF3, 0xF4),
    row_selected: c(0x4A, 0x90, 0xF5),
    accent: c(0x08, 0x66, 0xFF),
    bubble_in: c(0xFF, 0xFF, 0xFF),
    bubble_out: c(0x08, 0x66, 0xFF),
    text: c(0x00, 0x00, 0x00),
    text_out: c(0xFF, 0xFF, 0xFF),
    secondary: c(0x70, 0x75, 0x79),
    time_out: c(0xCF, 0xE0, 0xFF),
    link: c(0x08, 0x66, 0xFF),
    unread: c(0x08, 0x66, 0xFF),
    unread_muted: c(0xC4, 0xC9, 0xCC),
    separator: c(0xDF, 0xE1, 0xE5),
    input: c(0xF1, 0xF3, 0xF4),
    danger: c(0xE5, 0x39, 0x35),
    chip: Color32::from_rgba_premultiplied(0, 0, 0, 70),
};

pub fn palette(dark: bool) -> &'static Palette {
    if dark { &DARK } else { &LIGHT }
}

pub fn semibold(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("semibold".into()))
}

pub fn body(size: f32) -> FontId {
    FontId::proportional(size)
}

pub fn apply(ctx: &egui::Context, dark: bool) {
    let p = palette(dark);
    let mut v = if dark { egui::Visuals::dark() } else { egui::Visuals::light() };
    v.panel_fill = p.chat_bg;
    v.window_fill = p.sidebar;
    v.extreme_bg_color = p.input;
    v.faint_bg_color = p.row_hover;
    v.selection.bg_fill = p.accent;
    v.hyperlink_color = p.link;
    v.window_corner_radius = egui::CornerRadius::same(12);
    v.widgets.noninteractive.bg_stroke = egui::Stroke::new(1.0, p.separator);
    v.widgets.noninteractive.fg_stroke = egui::Stroke::new(1.0, p.text);
    v.widgets.inactive.fg_stroke = egui::Stroke::new(1.0, p.text);
    v.widgets.inactive.weak_bg_fill = p.input;
    v.widgets.hovered.weak_bg_fill = p.row_hover;
    ctx.set_visuals(v);
    ctx.style_mut(|s| {
        s.spacing.item_spacing = egui::vec2(8.0, 6.0);
        s.spacing.scroll.bar_width = 6.0;
    });
}

/// Stable avatar colour from an id (Telegram-like hues).
pub fn avatar_color(key: &str) -> Color32 {
    const HUES: [Color32; 7] = [c(0xE1, 0x70, 0x76), c(0x65, 0xAA, 0xDD), c(0xA6, 0x95, 0xE7), c(0xEE, 0x7A, 0xAE), c(0x6E, 0xC9, 0xCB), c(0xFA, 0xA7, 0x74), c(0x7B, 0xC8, 0x62)];
    let mut h: u32 = 2166136261;
    for b in key.bytes() {
        h = (h ^ b as u32).wrapping_mul(16777619);
    }
    HUES[(h % 7) as usize]
}
