//! A small, quiet palette. The UI should look like a tool, not a storefront.

use eframe::egui::{self, Color32};
use modifile_core::TrustLevel;

pub const BG: Color32 = Color32::from_rgb(0x16, 0x18, 0x1d);
pub const BAR: Color32 = Color32::from_rgb(0x1b, 0x1e, 0x24);
pub const CARD: Color32 = Color32::from_rgb(0x21, 0x25, 0x2c);
pub const TEXT: Color32 = Color32::from_rgb(0xe4, 0xe6, 0xea);
pub const MUTED: Color32 = Color32::from_rgb(0x8b, 0x92, 0x9e);
pub const ACCENT: Color32 = Color32::from_rgb(0x6e, 0xa8, 0xfe);
/// Fill for primary buttons — readable against light text, unlike ACCENT.
pub const ACCENT_DIM: Color32 = Color32::from_rgb(0x2f, 0x4a, 0x78);
pub const GOOD: Color32 = Color32::from_rgb(0x69, 0xc7, 0x8e);
/// Card background for the "this profile is active" banner.
pub const GOOD_DIM: Color32 = Color32::from_rgb(0x1e, 0x33, 0x28);
pub const WARN: Color32 = Color32::from_rgb(0xe0, 0xb1, 0x4f);
pub const BAD: Color32 = Color32::from_rgb(0xe0, 0x6c, 0x6c);

pub fn trust_color(level: TrustLevel) -> Color32 {
    match level {
        TrustLevel::Verified => GOOD,
        TrustLevel::Readable => ACCENT,
        TrustLevel::Unchecked => WARN,
        TrustLevel::Blocked => BAD,
    }
}

pub fn panel_frame() -> egui::Frame {
    egui::Frame::NONE
        .fill(BG)
        .inner_margin(egui::Margin::symmetric(14, 8))
}

pub fn bar_frame() -> egui::Frame {
    egui::Frame::NONE
        .fill(BAR)
        .inner_margin(egui::Margin::symmetric(14, 8))
}

pub fn apply(ctx: &egui::Context) {
    ctx.set_theme(egui::ThemePreference::Dark);
    ctx.all_styles_mut(style_for);
}

fn style_for(style: &mut egui::Style) {
    style.visuals = egui::Visuals::dark();
    style.visuals.panel_fill = BG;
    style.visuals.window_fill = BAR;
    style.visuals.extreme_bg_color = BG;
    style.visuals.override_text_color = Some(TEXT);
    style.visuals.widgets.noninteractive.bg_stroke.color = Color32::from_rgb(0x2b, 0x30, 0x38);
    style.visuals.widgets.inactive.bg_fill = CARD;
    style.visuals.widgets.inactive.weak_bg_fill = CARD;
    style.visuals.widgets.hovered.bg_fill = Color32::from_rgb(0x2c, 0x31, 0x3a);
    style.visuals.widgets.hovered.weak_bg_fill = Color32::from_rgb(0x2c, 0x31, 0x3a);
    style.visuals.widgets.active.bg_fill = ACCENT.gamma_multiply(0.35);
    style.visuals.selection.bg_fill = ACCENT.gamma_multiply(0.35);
    style.visuals.selection.stroke.color = TEXT;
    style.visuals.window_corner_radius = 8.into();

    style.spacing.item_spacing = egui::vec2(8.0, 6.0);
    style.spacing.button_padding = egui::vec2(10.0, 5.0);
    style.spacing.interact_size.y = 26.0;
}
