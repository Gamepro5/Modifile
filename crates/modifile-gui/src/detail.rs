//! The page about one mod or modpack.
//!
//! What it exists to answer, in order: what is this, who wrote it, can I read
//! its code, and do I want it. The last question is the reason for the
//! screenshots; the third is the reason this page says "no source published"
//! in plain words rather than leaving a gap where a link would be.

use eframe::egui;

use crate::{art, theme, App, View};

impl App {
    pub(crate) fn detail_page(&mut self, ui: &mut egui::Ui) {
        let Some(state) = self.detail.as_ref() else {
            self.view = View::Games;
            return;
        };
        let id = state.id.clone();
        let from = state.from;

        ui.horizontal(|ui| {
            if theme::glyph_button(ui, theme::Glyph::Back, "Back", None, true).clicked() {
                self.view = View::Game(from);
            }
            ui.label(
                egui::RichText::new(id.to_string())
                    .small()
                    .color(theme::MUTED),
            );
        });
        ui.add_space(10.0);

        if let Some(error) = state.error.clone() {
            theme::card_frame().show(ui, |ui| {
                ui.label(egui::RichText::new("Could not load this page").strong());
                ui.label(egui::RichText::new(error).small().color(theme::BAD));
                ui.add_space(4.0);
                ui.hyperlink_to("Open it on the web instead", id.web_url());
            });
            return;
        }

        if state.loading {
            ui.horizontal(|ui| {
                ui.add(egui::Spinner::new());
                ui.label(egui::RichText::new("Loading…").color(theme::MUTED));
            });
            return;
        }

        let Some(details) = state.details.clone() else {
            return;
        };

        // --- header ------------------------------------------------------
        let already = self.rows.iter().any(|r| r.id == id);
        let mut add = false;
        let mut install_pack = false;

        theme::card_frame().show(ui, |ui| {
            ui.horizontal(|ui| {
                let (rect, _) =
                    ui.allocate_exact_size(egui::vec2(88.0, 88.0), egui::Sense::hover());
                let texture = details
                    .icon_url
                    .as_deref()
                    .and_then(|url| self.art.texture(ui.ctx(), url));
                art::draw(
                    ui,
                    rect,
                    texture.as_ref(),
                    &id.to_string(),
                    &details.title,
                    10,
                );
                ui.add_space(10.0);

                ui.vertical(|ui| {
                    ui.label(egui::RichText::new(&details.title).size(19.0).strong());
                    if !details.summary.is_empty() {
                        ui.label(
                            egui::RichText::new(&details.summary).color(theme::MUTED),
                        );
                    }
                    ui.add_space(4.0);

                    ui.horizontal(|ui| {
                        if !details.authors.is_empty() {
                            ui.label(
                                egui::RichText::new(format!(
                                    "by {}",
                                    details.authors.join(", ")
                                ))
                                .small()
                                .color(theme::MUTED),
                            );
                        }
                        if details.downloads > 0 {
                            ui.label(
                                egui::RichText::new(format!(
                                    "{} downloads",
                                    compact(details.downloads)
                                ))
                                .small()
                                .color(theme::MUTED),
                            );
                        }
                        match &details.license {
                            Some(license) => {
                                ui.label(
                                    egui::RichText::new(license)
                                        .small()
                                        .color(theme::MUTED),
                                );
                            }
                            None => {
                                ui.label(
                                    egui::RichText::new("no licence declared")
                                        .small()
                                        .color(theme::WARN),
                                )
                                .on_hover_text(
                                    "Not the same as being unlicensed by intent — plenty \
                                     of projects simply never set one.",
                                );
                            }
                        }
                    });

                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        match &details.source_url {
                            Some(url) => {
                                ui.hyperlink_to("Source code", url);
                            }
                            None => {
                                ui.label(
                                    egui::RichText::new("No source published")
                                        .color(theme::WARN),
                                )
                                .on_hover_text(
                                    "Nothing about this can be checked — not by \
                                     Modifile, not by you. Installing it needs the \
                                     switch in Settings.",
                                );
                            }
                        }
                        if !details.web_url.is_empty() {
                            ui.hyperlink_to("Web page", &details.web_url);
                        }
                    });
                });

                ui.with_layout(egui::Layout::right_to_left(egui::Align::TOP), |ui| {
                    if details.is_pack {
                        if ui
                            .add(
                                egui::Button::new("  Install pack  ")
                                    .fill(theme::ACCENT_DIM),
                            )
                            .on_hover_text(
                                "Creates a profile from this pack. Nothing reaches the \
                                 game until you activate it.",
                            )
                            .clicked()
                        {
                            install_pack = true;
                        }
                    } else if ui
                        .add_enabled(
                            !already,
                            egui::Button::new(if already {
                                "  Added  "
                            } else {
                                "  Add to profile  "
                            })
                            .fill(theme::ACCENT_DIM),
                        )
                        .on_disabled_hover_text("Already in the selected profile")
                        .clicked()
                    {
                        add = true;
                    }
                });
            });
        });

        // --- screenshots ---------------------------------------------------
        if !details.gallery.is_empty() && self.art.enabled() {
            ui.add_space(12.0);
            theme::caption(ui, "Screenshots");
            egui::ScrollArea::horizontal()
                .id_salt("gallery")
                .max_height(150.0)
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        for url in details.gallery.iter().take(12) {
                            let (rect, response) = ui.allocate_exact_size(
                                egui::vec2(220.0, 124.0),
                                egui::Sense::click(),
                            );
                            let texture = self.art.texture(ui.ctx(), url);
                            art::draw(
                                ui,
                                rect,
                                texture.as_ref(),
                                url,
                                &details.title,
                                8,
                            );
                            if response.clicked() {
                                crate::open_url(url);
                            }
                            ui.add_space(8.0);
                        }
                    });
                });
        }

        // --- description ---------------------------------------------------
        ui.add_space(12.0);
        theme::caption(ui, "About");
        theme::card_frame().show(ui, |ui| {
            match &details.body {
                Some(body) => {
                    // Long descriptions are long. Enough to judge by, with the
                    // web page one click away for the rest.
                    let shown = modifile_core::text::truncate(body, 2600);
                    ui.label(shown);
                    if body.chars().count() > 2600 {
                        ui.add_space(6.0);
                        ui.hyperlink_to("Read the rest on the web", &details.web_url);
                    }
                }
                None if !details.summary.is_empty() => {
                    ui.label(&details.summary);
                }
                None => {
                    ui.label(
                        egui::RichText::new("This project publishes no description.")
                            .color(theme::MUTED),
                    );
                }
            }
        });
        ui.add_space(16.0);

        if add {
            self.add_id(&id);
        }
        if install_pack {
            let source = crate::browse::pack_source_for(&id);
            let ctx = ui.ctx().clone();
            self.do_import_modpack(source, &ctx);
        }
    }
}

/// 1_234_567 -> "1.2M".
fn compact(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}
