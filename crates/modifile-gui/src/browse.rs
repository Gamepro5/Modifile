//! Browsing: finding mods and modpacks, as cards with artwork.
//!
//! The list this replaces was a column of text rows, which is fine for reading
//! and useless for choosing. Choosing is what this screen is for, so a result
//! carries the things that actually decide it: what it looks like, who wrote
//! it, how many people run it, and whether the code can be read.

use eframe::egui;

use crate::{art, theme, App, View};

impl App {
    /// The search box and its results, for mods or for modpacks.
    pub(crate) fn browse_tab(&mut self, ui: &mut egui::Ui, game: &str, packs: bool) {
        ui.horizontal(|ui| {
            let field = ui.add(
                egui::TextEdit::singleline(&mut self.search_input)
                    .desired_width(320.0)
                    .hint_text(if packs {
                        "Search modpacks…"
                    } else {
                        "Search mods…"
                    }),
            );
            let entered = field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));

            if (ui
                .add_enabled(!self.searching, egui::Button::new("Search"))
                .clicked()
                || entered)
                && !self.search_input.trim().is_empty()
            {
                let ctx = ui.ctx().clone();
                self.do_browse(&ctx, game.to_string(), packs);
            }
            if self.searching {
                ui.add(egui::Spinner::new());
            }

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                // Pasting a link is still the fastest way in when you already
                // know what you want, so it does not get buried.
                let paste = ui.add(
                    egui::TextEdit::singleline(&mut self.add_input)
                        .desired_width(200.0)
                        .hint_text(if packs { "or paste a pack link" } else { "or paste a link" }),
                );
                let go = paste.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                if (ui.button("Add").clicked() || go) && !self.add_input.trim().is_empty() {
                    if packs {
                        let source = modifile_core::engine::ModpackSource::parse(
                            self.add_input.trim(),
                        );
                        let ctx = ui.ctx().clone();
                        self.add_input.clear();
                        self.do_import_modpack(source, &ctx);
                    } else {
                        self.do_add();
                    }
                }
            });
        });

        // Where an Add lands. Worth stating plainly: browsing is done from a
        // game's page, so the answer is not always obvious and is sometimes
        // "nowhere yet".
        if !packs {
            ui.add_space(6.0);
            let mine: Vec<modifile_core::profile::ProfileId> = self
                .profiles
                .iter()
                .filter(|p| p.game_id == game)
                .map(|p| p.id.clone())
                .collect();
            let mut switch_to: Option<modifile_core::profile::ProfileId> = None;
            let mut make_one = false;

            ui.horizontal(|ui| match self.selected.clone() {
                // Only this game's profiles: a Minecraft `main` is not where a
                // Valheim mod goes, even though the names match.
                Some(current) if mine.contains(&current) => {
                    ui.label(
                        egui::RichText::new("Adding to").small().color(theme::MUTED),
                    );
                    if mine.len() > 1 {
                        egui::ComboBox::from_id_salt("add-target")
                            .selected_text(current.name.clone())
                            .show_ui(ui, |ui| {
                                for id in &mine {
                                    if ui
                                        .selectable_label(*id == current, &id.name)
                                        .clicked()
                                    {
                                        switch_to = Some(id.clone());
                                    }
                                }
                            });
                    } else {
                        ui.label(egui::RichText::new(&current.name).small().strong());
                    }
                }
                _ => {
                    ui.label(
                        egui::RichText::new(
                            "No profile selected — a mod you add needs somewhere to go.",
                        )
                        .small()
                        .color(theme::WARN),
                    );
                    if mine.is_empty() {
                        if ui.small_button("Make one").clicked() {
                            make_one = true;
                        }
                    } else if ui.small_button("Pick one").clicked() {
                        switch_to = mine.first().cloned();
                    }
                }
            });

            if let Some(id) = switch_to {
                self.select(&id);
            }
            if make_one {
                self.new_profile_game = game.to_string();
                self.open_new_profile();
            }
        }

        ui.add_space(10.0);

        if self.search_results.is_empty() {
            ui.add_space(24.0);
            ui.vertical_centered(|ui| {
                ui.label(
                    egui::RichText::new(if self.searching {
                        "Searching…"
                    } else if packs {
                        "Search for a modpack, or paste a link to one."
                    } else {
                        "Search for a mod, or paste a link to one."
                    })
                    .color(theme::MUTED),
                );
                if !self.searching {
                    ui.add_space(4.0);
                    ui.label(
                        egui::RichText::new(self.where_we_look(game, packs))
                            .small()
                            .color(theme::MUTED),
                    );
                }
            });
            return;
        }

        let hits = self.search_results.clone();
        let mut add: Option<modifile_core::ModId> = None;
        let mut open: Option<modifile_core::ModId> = None;
        let mut install_pack: Option<modifile_core::ModId> = None;

        for hit in &hits {
            let already = self.rows.iter().any(|r| r.id == hit.id);
            let blocked = hit.installable == Some(false);

            let response = theme::card_frame()
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        // Icon.
                        let (rect, _) = ui.allocate_exact_size(
                            egui::vec2(52.0, 52.0),
                            egui::Sense::hover(),
                        );
                        let texture = hit
                            .icon_url
                            .as_deref()
                            .and_then(|url| self.art.texture(ui.ctx(), url));
                        art::draw(
                            ui,
                            rect,
                            texture.as_ref(),
                            &hit.id.to_string(),
                            &hit.label(),
                            8,
                        );
                        ui.add_space(6.0);

                        ui.vertical(|ui| {
                            ui.horizontal(|ui| {
                                ui.label(
                                    egui::RichText::new(hit.label()).strong().size(14.0),
                                );
                                ui.label(
                                    egui::RichText::new(hit.id.kind.label())
                                        .small()
                                        .color(theme::MUTED),
                                );
                                if blocked {
                                    ui.label(
                                        egui::RichText::new("cannot be fetched")
                                            .small()
                                            .color(theme::WARN),
                                    )
                                    .on_hover_text(
                                        "This project's author has disabled third-party \
                                         downloads.",
                                    );
                                }
                            });

                            ui.horizontal(|ui| {
                                if let Some(author) = &hit.author {
                                    ui.label(
                                        egui::RichText::new(format!("by {author}"))
                                            .small()
                                            .color(theme::MUTED),
                                    );
                                }
                                ui.label(
                                    egui::RichText::new(hit.popularity())
                                        .small()
                                        .color(theme::MUTED),
                                );
                                // The thing this project cares about most.
                                match &hit.source_url {
                                    Some(url) => {
                                        ui.hyperlink_to(
                                            egui::RichText::new("source").small(),
                                            url,
                                        );
                                    }
                                    None => {
                                        ui.label(
                                            egui::RichText::new("no source published")
                                                .small()
                                                .color(theme::WARN),
                                        );
                                    }
                                }
                            });

                            if !hit.description.is_empty() {
                                ui.label(
                                    egui::RichText::new(modifile_core::text::truncate(
                                        &hit.description,
                                        150,
                                    ))
                                    .small()
                                    .color(theme::MUTED),
                                );
                            }
                        });

                        ui.with_layout(
                            egui::Layout::right_to_left(egui::Align::Center),
                            |ui| {
                                if packs {
                                    if ui
                                        .add(
                                            egui::Button::new("Install pack")
                                                .fill(theme::ACCENT_DIM),
                                        )
                                        .on_hover_text(
                                            "Creates a profile from this pack. Nothing \
                                             is put in the game until you activate it.",
                                        )
                                        .clicked()
                                    {
                                        install_pack = Some(hit.id.clone());
                                    }
                                } else if ui
                                    .add_enabled(
                                        !already && !blocked,
                                        egui::Button::new(if already {
                                            "added"
                                        } else {
                                            "Add"
                                        }),
                                    )
                                    .on_disabled_hover_text(if already {
                                        "Already in this profile"
                                    } else {
                                        "This project publishes nothing installable"
                                    })
                                    .clicked()
                                {
                                    add = Some(hit.id.clone());
                                }
                                if ui.small_button("Details").clicked() {
                                    open = Some(hit.id.clone());
                                }
                            },
                        );
                    });
                })
                .response;

            // Deliberately no click sense on the card itself. `interact` on a
            // frame's response registers the whole card rect *after* the
            // widgets inside it, so it wins the click and Add and Details stop
            // responding entirely. A double-click shortcut is not worth
            // breaking the buttons it sits on top of.
            let _ = response;
            ui.add_space(6.0);
        }

        if let Some(id) = add {
            self.add_id(&id);
        }
        if let Some(id) = open {
            let ctx = ui.ctx().clone();
            let from = if packs {
                crate::GameTab::Packs
            } else {
                crate::GameTab::Mods
            };
            self.open_details(&ctx, id, from);
        }
        if let Some(id) = install_pack {
            let source = pack_source_for(&id);
            let ctx = ui.ctx().clone();
            self.do_import_modpack(source, &ctx);
        }
    }

    /// One line naming the indexes this game's pack actually searches.
    fn where_we_look(&self, game: &str, packs: bool) -> String {
        let Some(pack) = self.engine().and_then(|e| e.pack(game)) else {
            return String::new();
        };
        let rules = &pack.pack.search;
        let mut sources = Vec::new();
        if rules.modrinth {
            sources.push("Modrinth");
        }
        if rules.thunderstore_community.is_some() {
            sources.push("Thunderstore");
        }
        // Only when a key is actually set: naming a source that will answer
        // nothing is worse than not naming it.
        if rules.curseforge_game_id.is_some()
            && self.engine().is_some_and(|e| e.curseforge.is_some())
        {
            sources.push("CurseForge");
        }
        if !packs && (!rules.github_topics.is_empty() || !rules.github_terms.is_empty()) {
            sources.push("GitHub");
        }

        if sources.is_empty() {
            "This game's pack does not say where to search.".to_string()
        } else {
            format!("Searching {}.", sources.join(", "))
        }
    }

    /// Open the page for one mod or pack.
    pub(crate) fn open_details(
        &mut self,
        ctx: &egui::Context,
        id: modifile_core::ModId,
        from: crate::GameTab,
    ) {
        self.detail = Some(crate::DetailState {
            id: id.clone(),
            loading: true,
            details: None,
            error: None,
            from,
        });
        self.view = View::Detail;

        let Some(game) = self.game.clone() else {
            // Otherwise the page sits on "Loading…" for ever with nothing on
            // its way to answer it.
            if let Some(state) = self.detail.as_mut() {
                state.loading = false;
                state.error = Some(
                    "No game is selected, so there is nothing to look this up against."
                        .to_string(),
                );
            }
            return;
        };
        let paths = self.paths.clone();
        let tx = self.tx.clone();
        let ctx = ctx.clone();

        let handle = self.runtime.handle().clone();
        std::thread::spawn(move || {
            let result = handle.block_on(async {
                let engine =
                    modifile_core::Engine::open(paths.clone(), crate::load_token(&paths))?;
                let pack = engine
                    .pack(&game)
                    .ok_or_else(|| modifile_core::Error::NotFound(game.clone()))?;
                engine.details(pack, &id).await
            });
            let _ = match result {
                Ok(details) => tx.send(crate::Msg::Details(Box::new(details))),
                Err(e) => tx.send(crate::Msg::DetailsFailed(e.to_string())),
            };
            ctx.request_repaint();
        });
    }
}

/// How to fetch a pack, given the id a search result carries.
///
/// Search returns project ids; `ModpackSource` understands them, but only
/// spelled the way a person would type them.
pub(crate) fn pack_source_for(
    id: &modifile_core::ModId,
) -> modifile_core::engine::ModpackSource {
    modifile_core::engine::ModpackSource::parse(&id.to_string())
}
