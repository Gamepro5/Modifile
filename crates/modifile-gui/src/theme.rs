//! A small, quiet palette. The UI should look like a tool, not a storefront.

use eframe::egui::{self, Color32};
use modifile_core::TrustLevel;

// A neutral grey ramp rather than the near-black it started as.
//
// Two reasons it changed. Black backgrounds swallow dark artwork — the app's
// own icon sits on a near-black rounded square, and on the old rail it had no
// edge at all. And the old ramp carried a blue cast (seven points more blue
// than red), which reads as a tint rather than as a neutral surface. These
// keep about three points of blue, enough to stay cool without looking dyed.
//
// The order is the hierarchy and has to hold: RAIL < BG < BAR < CARD. The rail
// is darkest so it reads as the window's edge; cards are lightest so they sit
// above the page rather than in it.
pub const BG: Color32 = Color32::from_rgb(0x28, 0x2a, 0x2e);
pub const BAR: Color32 = Color32::from_rgb(0x2e, 0x31, 0x36);
pub const CARD: Color32 = Color32::from_rgb(0x35, 0x38, 0x3e);
/// The narrow strip of game tiles. Darker than the bar so the rail reads as
/// the edge of the window rather than as another panel.
pub const RAIL: Color32 = Color32::from_rgb(0x20, 0x22, 0x25);
/// A card the pointer is over.
pub const CARD_HOVER: Color32 = Color32::from_rgb(0x40, 0x44, 0x4b);
/// Hairlines: card borders, separators, the edge of a tile.
pub const LINE: Color32 = Color32::from_rgb(0x4a, 0x4e, 0x55);
pub const TEXT: Color32 = Color32::from_rgb(0xe4, 0xe6, 0xea);
/// Lifted along with the surfaces, and by the amount that keeps it where it
/// was: muted text on a card measured 4.91:1 against the old near-black, and
/// the old grey on the new card would have been 3.75:1 — too little for the
/// 10 and 11px labels this is mostly used for. This lands at 4.69:1.
pub const MUTED: Color32 = Color32::from_rgb(0x9d, 0xa4, 0xb0);
pub const ACCENT: Color32 = Color32::from_rgb(0x6e, 0xa8, 0xfe);
/// Fill for primary buttons — readable against light text, unlike ACCENT.
///
/// Lifted with the surfaces. A primary button has to sit *above* a card, and
/// the old value is now darker than one: it would have read as a hole punched
/// in the page rather than as the thing to press.
pub const ACCENT_DIM: Color32 = Color32::from_rgb(0x3a, 0x5c, 0x92);
pub const GOOD: Color32 = Color32::from_rgb(0x69, 0xc7, 0x8e);
/// Card background for the "this profile is active" banner. A tinted card, so
/// it tracks CARD rather than sinking below it.
pub const GOOD_DIM: Color32 = Color32::from_rgb(0x30, 0x4a, 0x39);
pub const WARN: Color32 = Color32::from_rgb(0xe0, 0xb1, 0x4f);
pub const BAD: Color32 = Color32::from_rgb(0xe0, 0x6c, 0x6c);

/// Small icons, painted rather than typed.
///
/// eframe's bundled fonts cover Latin text and emoji but not the geometric
/// shapes and arrows blocks, so `▶`, `＋`, `←` and friends come out as empty
/// tofu boxes. `status_dot` already learned this the hard way; these exist so
/// nothing else has to. A painted shape looks the same on every machine and
/// needs no font at all.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Glyph {
    Play,
    Plus,
    /// Four squares: "everything", for the all-games button.
    Grid,
    /// Sliders, for settings.
    Sliders,
    Back,
}

pub fn paint_glyph(painter: &egui::Painter, rect: egui::Rect, glyph: Glyph, color: Color32) {
    let c = rect.center();
    let s = rect.height().min(rect.width());
    let stroke = egui::Stroke::new((s * 0.11).max(1.3), color);

    match glyph {
        Glyph::Play => {
            let h = s * 0.46;
            let w = s * 0.40;
            painter.add(egui::Shape::convex_polygon(
                vec![
                    egui::pos2(c.x - w * 0.5, c.y - h),
                    egui::pos2(c.x - w * 0.5, c.y + h),
                    egui::pos2(c.x + w, c.y),
                ],
                color,
                egui::Stroke::NONE,
            ));
        }
        Glyph::Plus => {
            let r = s * 0.30;
            painter.line_segment([egui::pos2(c.x - r, c.y), egui::pos2(c.x + r, c.y)], stroke);
            painter.line_segment([egui::pos2(c.x, c.y - r), egui::pos2(c.x, c.y + r)], stroke);
        }
        Glyph::Grid => {
            let d = s * 0.30;
            let box_size = egui::vec2(d * 0.78, d * 0.78);
            for (dx, dy) in [(-1.0, -1.0), (1.0, -1.0), (-1.0, 1.0), (1.0, 1.0)] {
                let centre = egui::pos2(c.x + dx * d * 0.62, c.y + dy * d * 0.62);
                painter.rect_filled(
                    egui::Rect::from_center_size(centre, box_size),
                    egui::CornerRadius::same(2),
                    color,
                );
            }
        }
        Glyph::Sliders => {
            let w = s * 0.32;
            // Two rails with a knob on each, offset so it reads as controls
            // rather than as a list.
            for (row, knob) in [(-0.22, -0.30), (0.22, 0.30)] {
                let y = c.y + s * row;
                painter.line_segment(
                    [egui::pos2(c.x - w, y), egui::pos2(c.x + w, y)],
                    stroke,
                );
                painter.circle_filled(egui::pos2(c.x + w * knob, y), s * 0.13, color);
            }
        }
        Glyph::Back => {
            let w = s * 0.28;
            let tip = egui::pos2(c.x - w, c.y);
            painter.line_segment([tip, egui::pos2(c.x + w, c.y)], stroke);
            painter.line_segment([tip, egui::pos2(c.x, c.y - w * 0.85)], stroke);
            painter.line_segment([tip, egui::pos2(c.x, c.y + w * 0.85)], stroke);
        }
    }
}

/// A button with a painted icon before its label.
///
/// The label carries leading spaces to reserve the room, and the icon is
/// painted into that gap afterwards — which keeps egui's own button styling,
/// sizing and interaction rather than reimplementing them.
pub fn glyph_button(
    ui: &mut egui::Ui,
    glyph: Glyph,
    label: &str,
    fill: Option<Color32>,
    enabled: bool,
) -> egui::Response {
    let mut button = egui::Button::new(format!("     {label}"));
    if let Some(fill) = fill {
        button = button.fill(fill);
    }
    let response = ui.add_enabled(enabled, button);

    let size = response.rect.height() * 0.5;
    let icon = egui::Rect::from_center_size(
        egui::pos2(
            response.rect.left() + ui.style().spacing.button_padding.x + size * 0.5,
            response.rect.center().y,
        ),
        egui::vec2(size, size),
    );
    let color = if enabled { TEXT } else { MUTED };
    paint_glyph(ui.painter(), icon, glyph, color);
    response
}

pub fn trust_color(level: TrustLevel) -> Color32 {
    match level {
        TrustLevel::Verified => GOOD,
        TrustLevel::Readable => ACCENT,
        TrustLevel::Unchecked => WARN,
        TrustLevel::Blocked => BAD,
    }
}

/// A card: the unit the new layout is built from.
pub fn card_frame() -> egui::Frame {
    egui::Frame::NONE
        .fill(CARD)
        .corner_radius(8)
        .inner_margin(egui::Margin::symmetric(12, 10))
        .stroke(egui::Stroke::new(1.0, LINE))
}

/// A card that responds to the pointer, for one that is clickable.
///
/// egui draws a frame before it knows whether the pointer is inside it, so the
/// caller passes last frame's answer. One frame of lag on a hover highlight is
/// imperceptible; restructuring every card to avoid it would not be.
pub fn card_frame_hovered(hovered: bool) -> egui::Frame {
    card_frame()
        .fill(if hovered { CARD_HOVER } else { CARD })
        .stroke(egui::Stroke::new(
            1.0,
            if hovered { ACCENT.gamma_multiply(0.55) } else { LINE },
        ))
}

/// A small coloured pill.
///
/// Used for states that are easy to confuse with each other, which is exactly
/// why they get distinct shapes and colours rather than distinct wordings that
/// a reader has to parse. "These mods are installed" and "the game is running"
/// are different facts, and a label like "in game" that could mean either is
/// worse than no label.
pub fn badge(ui: &mut egui::Ui, text: &str, color: Color32) -> egui::Response {
    let font = egui::FontId::proportional(10.0);
    let galley =
        ui.painter()
            .layout_no_wrap(text.to_uppercase(), font, color);
    let pad = egui::vec2(7.0, 3.0);
    let (rect, response) =
        ui.allocate_exact_size(galley.size() + pad * 2.0, egui::Sense::hover());

    ui.painter().rect_filled(
        rect,
        egui::CornerRadius::same((rect.height() / 2.0) as u8),
        color.gamma_multiply(0.20),
    );
    ui.painter().rect_stroke(
        rect,
        egui::CornerRadius::same((rect.height() / 2.0) as u8),
        egui::Stroke::new(1.0, color.gamma_multiply(0.55)),
        egui::StrokeKind::Inside,
    );
    ui.painter()
        .galley(rect.min + pad, galley, color);
    response
}

/// A section heading: uppercase, small, muted. Used everywhere.
pub fn caption(ui: &mut egui::Ui, text: &str) {
    ui.label(
        egui::RichText::new(text.to_uppercase())
            .small()
            .color(MUTED)
            .strong(),
    );
    ui.add_space(4.0);
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

/// Put a pointing hand over anything clickable. Call once, at the end of a
/// frame.
///
/// egui leaves the cursor alone for buttons, so the arrow never changes and
/// there is no way to tell a card that opens from one that just sits there.
/// Annotating every call site with `on_hover_cursor` would work and would be
/// wrong: it is a rule about the whole UI, and the one place it can be stated
/// as a rule is here. egui already knows which widget the pointer is over and
/// whether it senses clicks, so this asks it.
///
/// The guard matters. `set_cursor_icon` is last-call-wins, so this only
/// upgrades the default arrow — a text field keeps its I-beam and a resize
/// handle keeps its arrows, because those widgets set a cursor of their own.
pub fn pointer_cursor(ctx: &egui::Context) {
    if ctx.output(|o| o.cursor_icon) != egui::CursorIcon::Default {
        return;
    }
    // Usually 0 or 1 ids, so this is a couple of map lookups per frame.
    let hovered = ctx.interaction_snapshot(|snapshot| snapshot.hovered.clone());
    let clickable = hovered.iter().any(|id| {
        ctx.read_response(*id)
            .is_some_and(|r| r.enabled() && r.sense.senses_click())
    });
    if clickable {
        ctx.set_cursor_icon(egui::CursorIcon::PointingHand);
    }
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
    style.visuals.widgets.noninteractive.bg_stroke.color = LINE;
    style.visuals.widgets.inactive.bg_fill = CARD;
    style.visuals.widgets.inactive.weak_bg_fill = CARD;
    // Hover has to be *lighter* than rest. These were literal values that
    // happened to be lighter than the old near-black card and are darker than
    // the new one, which would have made every button dim on hover.
    style.visuals.widgets.hovered.bg_fill = CARD_HOVER;
    style.visuals.widgets.hovered.weak_bg_fill = CARD_HOVER;
    style.visuals.widgets.active.bg_fill = ACCENT.gamma_multiply(0.35);
    style.visuals.selection.bg_fill = ACCENT.gamma_multiply(0.35);
    style.visuals.selection.stroke.color = TEXT;
    style.visuals.window_corner_radius = 8.into();

    // Labels are text, not a text field. egui makes them selectable by
    // default, which means every one of them senses clicks and drags: they
    // show an I-beam over buttons and cards, and a click on the name inside a
    // clickable card is eaten by the label instead of opening it. Nothing here
    // is text anyone wants to select.
    style.interaction.selectable_labels = false;

    style.spacing.item_spacing = egui::vec2(8.0, 6.0);
    style.spacing.button_padding = egui::vec2(10.0, 5.0);
    style.spacing.interact_size.y = 26.0;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Walks the pointer through `path`, two passes per stop, and reports the
    /// cursor the OS would be asked for at the last one. Two passes, because
    /// egui decides what is hovered from the widget rects of the pass before.
    fn cursor_path(path: &[egui::Pos2], build: impl Fn(&mut egui::Ui)) -> egui::CursorIcon {
        let ctx = egui::Context::default();
        apply(&ctx);
        let mut icon = egui::CursorIcon::Default;
        for pointer in path {
            for _ in 0..2 {
                let input = egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::pos2(0.0, 0.0),
                        egui::vec2(300.0, 200.0),
                    )),
                    events: vec![egui::Event::PointerMoved(*pointer)],
                    ..Default::default()
                };
                let output = ctx.run_ui(input, |ui| {
                    egui::CentralPanel::default().show(ui, |ui| build(ui));
                    pointer_cursor(ui.ctx());
                });
                icon = output.platform_output.cursor_icon;
            }
        }
        icon
    }

    fn cursor_at(pointer: egui::Pos2, build: impl Fn(&mut egui::Ui)) -> egui::CursorIcon {
        cursor_path(&[pointer], build)
    }

    #[test]
    fn clickable_widgets_get_a_hand() {
        let icon = cursor_at(egui::pos2(30.0, 20.0), |ui| {
            let _ = ui.button("Play");
        });
        assert_eq!(icon, egui::CursorIcon::PointingHand);
    }

    #[test]
    fn empty_space_gets_the_arrow_back() {
        // Onto the button, then off it. `cursor_icon` carries over between
        // frames, so this is the case that catches a rule that can only ever
        // set a hand and never take one away.
        let icon = cursor_path(
            &[egui::pos2(30.0, 20.0), egui::pos2(280.0, 180.0)],
            |ui| {
                let _ = ui.button("Play");
            },
        );
        assert_eq!(icon, egui::CursorIcon::Default);
    }

    /// Text fields still say "you can type here". The rule only upgrades the
    /// plain arrow, so a widget that asked for a cursor keeps it.
    #[test]
    fn text_fields_keep_the_i_beam() {
        let text = String::from("profile");
        let icon = cursor_at(egui::pos2(30.0, 20.0), |ui| {
            ui.text_edit_singleline(&mut text.clone());
        });
        assert_eq!(icon, egui::CursorIcon::Text);
    }

    /// Relative luminance, the WCAG definition.
    fn luminance(c: Color32) -> f64 {
        let ch = |v: u8| {
            let v = v as f64 / 255.0;
            if v <= 0.03928 {
                v / 12.92
            } else {
                ((v + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * ch(c.r()) + 0.7152 * ch(c.g()) + 0.0722 * ch(c.b())
    }

    fn contrast(a: Color32, b: Color32) -> f64 {
        let (x, y) = (luminance(a), luminance(b));
        (x.max(y) + 0.05) / (x.min(y) + 0.05)
    }

    /// The ramp *is* the hierarchy, and any edit to it has to keep the order.
    ///
    /// Not a subtle failure when it breaks: lightening the surfaces once left
    /// two colours behind, and a primary button ended up darker than the card
    /// it sat on — a hole in the page rather than the thing to press — while
    /// hover dimmed buttons instead of lifting them. Easy to reintroduce by
    /// hand, so it is checked rather than remembered.
    #[test]
    fn the_surface_ramp_gets_lighter_in_order() {
        let ramp = [
            ("RAIL", RAIL),
            ("BG", BG),
            ("BAR", BAR),
            ("CARD", CARD),
            ("CARD_HOVER", CARD_HOVER),
            ("LINE", LINE),
        ];
        for pair in ramp.windows(2) {
            let ((a_name, a), (b_name, b)) = (pair[0], pair[1]);
            assert!(
                luminance(a) < luminance(b),
                "{a_name} must be darker than {b_name}"
            );
        }

        // Filled surfaces that sit on a card have to be lighter than one.
        for (name, colour) in [("ACCENT_DIM", ACCENT_DIM), ("GOOD_DIM", GOOD_DIM)] {
            assert!(
                luminance(CARD) < luminance(colour),
                "{name} must sit above CARD, not below it"
            );
        }
    }

    /// Small print is the first thing a lighter background costs.
    #[test]
    fn text_stays_legible_on_every_surface() {
        for (name, bg) in [
            ("BG", BG),
            ("BAR", BAR),
            ("CARD", CARD),
            ("RAIL", RAIL),
            ("ACCENT_DIM", ACCENT_DIM),
            ("GOOD_DIM", GOOD_DIM),
        ] {
            assert!(
                contrast(TEXT, bg) >= 4.5,
                "TEXT on {name} is {:.2}:1",
                contrast(TEXT, bg)
            );
        }
        // MUTED carries 10 and 11px labels, so it is held to the same bar on
        // the surfaces it actually appears on. CARD_HOVER is excluded: it is a
        // transient state, not somewhere text is read.
        for (name, bg) in [("BG", BG), ("BAR", BAR), ("CARD", CARD), ("RAIL", RAIL)] {
            assert!(
                contrast(MUTED, bg) >= 4.5,
                "MUTED on {name} is {:.2}:1",
                contrast(MUTED, bg)
            );
        }
    }

    /// The bug behind "clicking the text on a profile does nothing": a label
    /// inside a clickable card ate the click and showed an I-beam.
    #[test]
    fn labels_are_not_selectable() {
        let mut style = egui::Style::default();
        style_for(&mut style);
        assert!(!style.interaction.selectable_labels);
    }
}
