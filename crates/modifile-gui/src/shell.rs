//! The window's frame: the rail of games, the game grid, and a game's page.
//!
//! The shape is borrowed from CurseForge's client, which gets the information
//! architecture right even though the app around it is heavy: a narrow rail of
//! games down the side, a page per game, and tabs for the things you do with
//! one. What is *not* borrowed is everything else — no ads, no account, no
//! background service, and the pages below are the same profile sections that
//! were there before, rehomed rather than rewritten.

use eframe::egui;

use crate::{art, theme, App, GameTab, View};

/// Width of the game rail. Enough for a 48px tile and its padding.
const RAIL_WIDTH: f32 = 72.0;
const TILE: f32 = 48.0;

/// The first whole sentence of a pack's description.
///
/// Pack descriptions are several sentences across several lines, and a banner
/// has room for one. Taking a fixed number of characters cuts mid-word; taking
/// the first line cuts mid-sentence, because the text is hard-wrapped in the
/// TOML. A sentence is the unit that reads as finished.
fn first_sentence(text: &str) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match flat.find(". ") {
        Some(end) => flat[..=end].to_string(),
        // No sentence break: it is one sentence, or one fragment. Either way
        // there is nothing better to cut at.
        None => flat,
    }
}

impl App {
    // -----------------------------------------------------------------------
    // The rail
    // -----------------------------------------------------------------------

    /// One tile per installed game, down the left edge.
    pub(crate) fn rail(&mut self, ui: &mut egui::Ui) {
        egui::Panel::left("rail")
            .exact_size(RAIL_WIDTH)
            // The rail is a fixed strip of tiles. Panels are resizable by
            // default, so without this egui puts a resize cursor on the edge
            // and offers a drag that `exact_size` then refuses — a hint for an
            // interaction that does not exist.
            .resizable(false)
            .frame(
                egui::Frame::NONE
                    .fill(theme::RAIL)
                    .inner_margin(egui::Margin::symmetric(0, 10)),
            )
            .show(ui, |ui| {
                let games: Vec<(String, String, Option<String>)> = self
                    .games
                    .iter()
                    .map(|g| (g.id.clone(), g.name.clone(), g.icon.clone()))
                    .collect();

                let mut go_home = false;
                let mut chosen: Option<String> = None;

                ui.vertical_centered(|ui| {
                    // Home: the "choose a game" grid. Modifile's own icon
                    // rather than a glyph — it is the one tile in the rail
                    // that is not a game, and the app's mark is what every
                    // other program puts in this corner.
                    if self
                        .rail_home(ui, matches!(self.view, View::Games))
                        .on_hover_text("All games")
                        .clicked()
                    {
                        go_home = true;
                    }

                    // Modifile is not a game, so a rule separates it from the
                    // ones that are. Inset from the rail's edges, because a
                    // line running wall to wall would cut the rail in two
                    // rather than group what is below it.
                    ui.add_space(9.0);
                    let (rule, _) =
                        ui.allocate_exact_size(egui::vec2(TILE, 1.0), egui::Sense::hover());
                    ui.painter().rect_filled(
                        egui::Rect::from_min_max(
                            egui::pos2(rule.left() + 6.0, rule.top()),
                            egui::pos2(rule.right() - 6.0, rule.top() + 1.0),
                        ),
                        egui::CornerRadius::ZERO,
                        theme::LINE,
                    );
                    ui.add_space(9.0);

                    for (id, name, icon) in &games {
                        let selected = self.game.as_deref() == Some(id.as_str())
                            && !matches!(self.view, View::Games | View::Settings);
                        if self.rail_tile(ui, id, name, icon.as_deref(), selected) {
                            chosen = Some(id.clone());
                        }
                        ui.add_space(6.0);
                    }
                });

                // Settings sits at the bottom, where it does in every app that
                // has a rail.
                let mut to_settings = false;
                ui.with_layout(egui::Layout::bottom_up(egui::Align::Center), |ui| {
                    ui.add_space(4.0);
                    if self
                        .rail_button(ui, theme::Glyph::Sliders, self.view == View::Settings)
                        .on_hover_text("Settings")
                        .clicked()
                    {
                        to_settings = true;
                    }
                });

                if go_home {
                    self.view = View::Games;
                }
                if to_settings {
                    self.view = View::Settings;
                    self.refresh_storage();
                }
                if let Some(id) = chosen {
                    self.open_game(&id);
                }
            });
    }

    /// The home tile: Modifile's own icon, at the top of the rail.
    ///
    /// The same square a game gets, marked the same way. It is a destination
    /// in the same list, so it should be the same size and shape — a smaller
    /// button would read as a toolbar control that happens to live above the
    /// games rather than as the first thing in the list.
    fn rail_home(&mut self, ui: &mut egui::Ui, selected: bool) -> egui::Response {
        let (rect, response) =
            ui.allocate_exact_size(egui::vec2(TILE, TILE), egui::Sense::click());

        match self.art.logo(ui.ctx()) {
            // Full bleed, like box art. The icon already carries its own
            // rounded corners and dark ground, which the grey rail now sets
            // off instead of swallowing.
            Some(logo) => {
                ui.painter().add(
                    egui::epaint::RectShape::filled(
                        rect,
                        egui::CornerRadius::same(10),
                        egui::Color32::WHITE,
                    )
                    .with_texture(
                        logo.id(),
                        egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                    ),
                );
            }
            // Bundled art that fails to decode means a broken build, not a
            // broken network — but a missing logo is no reason for a missing
            // button.
            None => {
                ui.painter()
                    .rect_filled(rect, egui::CornerRadius::same(10), theme::CARD);
                theme::paint_glyph(
                    ui.painter(),
                    egui::Rect::from_center_size(rect.center(), egui::vec2(24.0, 24.0)),
                    theme::Glyph::Grid,
                    theme::TEXT,
                );
            }
        }

        // Marked exactly as `rail_tile` marks a game: a bar for the current
        // page, a ring under the pointer. Two spellings of the same state
        // would be worse than none.
        let painter = ui.painter();
        if selected {
            painter.rect_filled(
                egui::Rect::from_min_size(
                    egui::pos2(rect.left() - 8.0, rect.top() + 6.0),
                    egui::vec2(3.0, rect.height() - 12.0),
                ),
                egui::CornerRadius::same(2),
                theme::ACCENT,
            );
        } else if response.hovered() {
            painter.rect_stroke(
                rect,
                egui::CornerRadius::same(10),
                egui::Stroke::new(2.0, theme::ACCENT.gamma_multiply(0.8)),
                egui::StrokeKind::Inside,
            );
        }
        response
    }

    /// A small square button in the rail, for things that are not games.
    fn rail_button(
        &mut self,
        ui: &mut egui::Ui,
        glyph: theme::Glyph,
        selected: bool,
    ) -> egui::Response {
        let (rect, response) =
            ui.allocate_exact_size(egui::vec2(TILE, 34.0), egui::Sense::click());
        let painter = ui.painter();
        let fill = if selected {
            theme::ACCENT_DIM
        } else if response.hovered() {
            theme::CARD_HOVER
        } else {
            theme::RAIL
        };
        painter.rect_filled(rect, egui::CornerRadius::same(8), fill);
        theme::paint_glyph(
            painter,
            egui::Rect::from_center_size(rect.center(), egui::vec2(20.0, 20.0)),
            glyph,
            if selected || response.hovered() {
                theme::TEXT
            } else {
                theme::MUTED
            },
        );
        response
    }

    /// One game's tile: its box art, or a generated tile when it has none.
    fn rail_tile(
        &mut self,
        ui: &mut egui::Ui,
        id: &str,
        name: &str,
        icon: Option<&str>,
        selected: bool,
    ) -> bool {
        let (rect, response) =
            ui.allocate_exact_size(egui::vec2(TILE, TILE), egui::Sense::click());

        let texture = icon.and_then(|url| self.art.texture(ui.ctx(), url));
        art::draw(ui, rect, texture.as_ref(), id, name, 10);

        let painter = ui.painter();
        // The selected game is marked by a bar rather than a border, so the
        // art itself is never obscured.
        if selected {
            painter.rect_filled(
                egui::Rect::from_min_size(
                    egui::pos2(rect.left() - 8.0, rect.top() + 6.0),
                    egui::vec2(3.0, rect.height() - 12.0),
                ),
                egui::CornerRadius::same(2),
                theme::ACCENT,
            );
        } else if response.hovered() {
            painter.rect_stroke(
                rect,
                egui::CornerRadius::same(10),
                egui::Stroke::new(2.0, theme::ACCENT.gamma_multiply(0.8)),
                egui::StrokeKind::Inside,
            );
        }

        response.on_hover_text(name).clicked()
    }

    // -----------------------------------------------------------------------
    // Choose a game
    // -----------------------------------------------------------------------

    pub(crate) fn games_grid(&mut self, ui: &mut egui::Ui) {
        ui.add_space(4.0);
        // The app's mark beside the heading, which is the one place the home
        // page says what program this is.
        ui.horizontal(|ui| {
            if let Some(logo) = self.art.logo(ui.ctx()) {
                let (rect, _) =
                    ui.allocate_exact_size(egui::vec2(44.0, 44.0), egui::Sense::hover());
                ui.painter().add(
                    egui::epaint::RectShape::filled(
                        rect,
                        egui::CornerRadius::same(10),
                        egui::Color32::WHITE,
                    )
                    .with_texture(
                        logo.id(),
                        egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                    ),
                );
                ui.add_space(4.0);
            }
            ui.vertical(|ui| {
                ui.heading("Choose a game");
                ui.label(
                    egui::RichText::new(
                        "Every game Modifile has a pack for. A pack is a TOML file — \
                         adding a game means writing one, not recompiling.",
                    )
                    .color(theme::MUTED),
                );
            });
        });
        ui.add_space(14.0);

        struct Tile {
            id: String,
            name: String,
            icon: Option<String>,
            profiles: usize,
            active: usize,
            found: bool,
        }

        let tiles: Vec<Tile> = self
            .games
            .iter()
            .map(|game| {
                let profiles: Vec<_> = self
                    .profiles
                    .iter()
                    .filter(|p| p.game_id == game.id)
                    .collect();
                Tile {
                    id: game.id.clone(),
                    name: game.name.clone(),
                    icon: game.icon.clone(),
                    profiles: profiles.len(),
                    active: profiles.iter().filter(|p| p.active).count(),
                    found: game.found(),
                }
            })
            .collect();

        // Box art is portrait, so the tiles are too.
        const W: f32 = 148.0;
        const H: f32 = 208.0;
        let mut chosen: Option<String> = None;

        let available = ui.available_width();
        let per_row = ((available + 12.0) / (W + 12.0)).floor().max(1.0) as usize;

        for row in tiles.chunks(per_row) {
            ui.horizontal(|ui| {
                for tile in row {
                    let (rect, response) =
                        ui.allocate_exact_size(egui::vec2(W, H), egui::Sense::click());

                    let art_rect =
                        egui::Rect::from_min_size(rect.min, egui::vec2(W, H - 46.0));
                    let texture = tile
                        .icon
                        .as_deref()
                        .and_then(|url| self.art.texture(ui.ctx(), url));
                    art::draw(ui, art_rect, texture.as_ref(), &tile.id, &tile.name, 10);

                    let painter = ui.painter();
                    if response.hovered() {
                        painter.rect_stroke(
                            art_rect,
                            egui::CornerRadius::same(10),
                            egui::Stroke::new(2.0, theme::ACCENT),
                            egui::StrokeKind::Inside,
                        );
                    }

                    painter.text(
                        egui::pos2(rect.left(), art_rect.bottom() + 8.0),
                        egui::Align2::LEFT_TOP,
                        &tile.name,
                        egui::FontId::proportional(13.0),
                        theme::TEXT,
                    );
                    // The line CurseForge gets right: say what you already have
                    // for this game, not just that the game exists.
                    let (note, colour) = if tile.active > 0 {
                        (
                            format!(
                                "Mods installed · {} profile{}",
                                tile.profiles,
                                if tile.profiles == 1 { "" } else { "s" }
                            ),
                            theme::GOOD,
                        )
                    } else if tile.profiles > 0 {
                        (
                            format!(
                                "{} profile{}",
                                tile.profiles,
                                if tile.profiles == 1 { "" } else { "s" }
                            ),
                            theme::MUTED,
                        )
                    } else if tile.found {
                        ("Installed · no profiles yet".to_string(), theme::MUTED)
                    } else {
                        ("Not found on this machine".to_string(), theme::MUTED)
                    };
                    painter.text(
                        egui::pos2(rect.left(), art_rect.bottom() + 26.0),
                        egui::Align2::LEFT_TOP,
                        note,
                        egui::FontId::proportional(11.0),
                        colour,
                    );

                    if response.clicked() {
                        chosen = Some(tile.id.clone());
                    }
                    ui.add_space(12.0);
                }
            });
            ui.add_space(12.0);
        }

        ui.add_space(8.0);
        ui.horizontal(|ui| {
            // The only place that acts across games, and it says so. Check for
            // updates on a game's own page does that game and nothing else.
            let total = self.profiles.len();
            if ui
                .add_enabled(
                    total > 0 && !self.busy,
                    egui::Button::new("Check every game for updates"),
                )
                .on_hover_text(format!(
                    "Checks all {total} profile(s) across every game and downloads what \
                     is missing. Nothing is put into any game — activate the ones you \
                     want afterwards."
                ))
                .clicked()
            {
                let ctx = ui.ctx().clone();
                self.do_sync_all(&ctx);
            }
            if ui.button("Add another game…").clicked() {
                crate::reveal(&self.paths.packs);
            }
            ui.label(
                egui::RichText::new(
                    "Drop a pack TOML into the packs folder and press Refresh in Settings.",
                )
                .small()
                .color(theme::MUTED),
            );
        });

        if let Some(id) = chosen {
            self.open_game(&id);
        }
    }

    // -----------------------------------------------------------------------
    // One game
    // -----------------------------------------------------------------------

    pub(crate) fn game_page(&mut self, ui: &mut egui::Ui, tab: GameTab) {
        let Some(game) = self.game.clone() else {
            self.view = View::Games;
            return;
        };
        let Some((name, banner, description, has_packs)) = self.game_info(&game).map(|g| {
            (
                g.name.clone(),
                g.banner.clone(),
                g.description.clone(),
                g.has_modpacks,
            )
        }) else {
            self.view = View::Games;
            return;
        };

        self.game_header(ui, &game, &name, banner.as_deref(), &description);
        ui.add_space(10.0);

        // Tabs.
        let mut next = tab;
        ui.horizontal(|ui| {
            for (label, value, enabled) in [
                ("My profiles", GameTab::Profiles, true),
                ("Browse mods", GameTab::Mods, true),
                ("Browse modpacks", GameTab::Packs, has_packs),
            ] {
                let selected = tab == value;
                let text = egui::RichText::new(label).size(13.0).color(if selected {
                    theme::TEXT
                } else {
                    theme::MUTED
                });
                let clicked = ui
                    .add_enabled_ui(enabled, |ui| ui.selectable_label(selected, text))
                    .inner
                    .on_disabled_hover_text(
                        "This game's pack does not say where to look for modpacks.",
                    )
                    .clicked();
                if clicked {
                    next = value;
                }
            }
        });
        if next != tab {
            self.view = View::Game(next);
            // A fresh tab should not show the previous one's results.
            self.search_results.clear();
        }
        ui.add_space(4.0);
        ui.separator();
        ui.add_space(10.0);

        match next {
            GameTab::Profiles => self.profiles_tab(ui, &game),
            GameTab::Mods => self.browse_tab(ui, &game, false),
            GameTab::Packs => self.browse_tab(ui, &game, true),
        }
    }

    /// The banner across the top of a game's page.
    fn game_header(
        &mut self,
        ui: &mut egui::Ui,
        game: &str,
        name: &str,
        banner: Option<&str>,
        description: &str,
    ) {
        const H: f32 = 96.0;
        let width = ui.available_width();
        let (rect, _) = ui.allocate_exact_size(egui::vec2(width, H), egui::Sense::hover());

        let texture = banner.and_then(|url| self.art.texture(ui.ctx(), url));
        art::draw(ui, rect, texture.as_ref(), game, name, 10);

        // A gradient keeps the title readable over whatever the art happens to
        // be.
        //
        // Painted as one mesh with per-vertex colours. It used to be a stack
        // of translucent bands, each drawn a pixel taller than its slot so
        // rounding could not leave a gap — but two translucent blacks over the
        // same pixel are darker than one, so every overlap became a visible
        // line and the banner picked up two dozen dark stripes. Vertices are
        // shared here, so neighbouring strips meet at one position with one
        // colour: no overlap to double up, and no gap either.
        let painter = ui.painter();
        let steps = 24;
        let mut mesh = egui::Mesh::default();
        for i in 0..=steps {
            let t = i as f32 / steps as f32;
            let y = rect.top() + rect.height() * t;
            // Quadratic, so the art stays visible at the top and the text sits
            // on near-solid black at the bottom.
            let shade = egui::Color32::from_black_alpha((t * t * 210.0) as u8);
            mesh.colored_vertex(egui::pos2(rect.left(), y), shade);
            mesh.colored_vertex(egui::pos2(rect.right(), y), shade);
        }
        for i in 0..steps {
            let top = (i * 2) as u32;
            mesh.add_triangle(top, top + 1, top + 2);
            mesh.add_triangle(top + 1, top + 3, top + 2);
        }
        painter.add(egui::Shape::mesh(mesh));

        painter.text(
            egui::pos2(rect.left() + 16.0, rect.bottom() - 34.0),
            egui::Align2::LEFT_TOP,
            name,
            egui::FontId::proportional(22.0),
            theme::TEXT,
        );
        // One complete sentence, cut to what the banner is actually wide
        // enough for. A fixed character count stops mid-word on a narrow
        // window and wastes half the bar on a wide one.
        let font = egui::FontId::proportional(11.0);
        let room = ((rect.width() - 32.0) / 4.9) as usize;
        painter.text(
            egui::pos2(rect.left() + 16.0, rect.bottom() - 14.0),
            egui::Align2::LEFT_TOP,
            modifile_core::text::truncate(&first_sentence(description), room),
            font,
            theme::MUTED,
        );
    }

    /// Where this game lives on disk, and how to correct it.
    ///
    /// On the game's own page rather than in a settings screen, because "it
    /// found the wrong folder" is a thing you discover while looking at the
    /// game, and hunting for a separate page to fix it is the sort of small
    /// indignity this rewrite is meant to remove.
    fn folders_card(&mut self, ui: &mut egui::Ui, game: &str) {
        struct Row {
            target: modifile_core::pack::Target,
            path: Option<std::path::PathBuf>,
            remembered: bool,
        }

        let rows: Vec<Row> = self
            .game_info(game)
            .map(|g| {
                g.targets
                    .iter()
                    .map(|(target, path, remembered)| Row {
                        target: target.clone(),
                        path: path.clone(),
                        remembered: *remembered,
                    })
                    .collect()
            })
            .unwrap_or_default();
        if rows.is_empty() {
            return;
        }

        let mut choose: Option<modifile_core::pack::Target> = None;
        let mut forget: Option<String> = None;

        theme::card_frame().show(ui, |ui| {
            theme::caption(ui, "Game folders");
            for row in &rows {
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new(row.target.kind.label().to_uppercase())
                            .small()
                            .color(theme::MUTED),
                    );
                    ui.label(&row.target.name);
                    ui.with_layout(
                        egui::Layout::right_to_left(egui::Align::Center),
                        |ui| {
                            if ui
                                .small_button(if row.path.is_some() {
                                    "Change…"
                                } else {
                                    "Choose folder…"
                                })
                                .clicked()
                            {
                                choose = Some(row.target.clone());
                            }
                            if row.remembered && ui.small_button("Forget").clicked() {
                                forget = Some(row.target.id.clone());
                            }
                            match &row.path {
                                Some(path) => {
                                    ui.label(
                                        egui::RichText::new(crate::display_path(path))
                                            .small()
                                            .color(theme::GOOD),
                                    );
                                }
                                None => {
                                    ui.label(
                                        egui::RichText::new("not found")
                                            .small()
                                            .color(theme::MUTED),
                                    );
                                }
                            }
                        },
                    );
                });
            }
        });

        if let Some(target) = choose {
            self.open_root_dialog(game, &target);
        }
        if let Some(id) = forget {
            self.forget_root(game, &id);
        }
    }

    /// How Play starts this game, and how to change it.
    ///
    /// On the game's page because it is a property of the game, not of any one
    /// profile — every profile for a game starts it the same way.
    fn launch_card(&mut self, ui: &mut egui::Ui, game: &str) {
        let current: Option<String> = self.game_info(game).and_then(|g| g.launch_how.clone());

        let saved = self
            .engine()
            .map(|e| e.launch_settings())
            .and_then(|s| s.get(game).map(str::to_string));

        // Seed the edit box once, so typing is not overwritten every frame.
        let key = egui::Id::new(("launch-input", game));
        let mut text = ui.data(|d| {
            d.get_temp::<String>(key)
                .unwrap_or_else(|| saved.clone().unwrap_or_default())
        });
        let mut save = false;
        let mut clear = false;

        let instanced = self.game_info(game).is_some_and(|g| g.instanced);

        theme::card_frame().show(ui, |ui| {
            theme::caption(ui, "Starting the game");

            // The thing worth saying plainly, because it is what makes Play
            // safe rather than a convenience with a hidden cost.
            if instanced {
                ui.label(
                    egui::RichText::new(
                        "Play gives each profile its own folder and points the game at it \
                         for that run. Your game install is never modified, so closing \
                         the game — or crashing, or losing power — leaves nothing behind \
                         to undo.",
                    )
                    .small()
                    .color(theme::GOOD),
                );
            } else {
                ui.label(
                    egui::RichText::new(
                        "This game reads its mods from one fixed place inside its own \
                         folder, so there is no way to hand it a profile for a single \
                         run. Modifile would have to modify the install and put it back \
                         afterwards, and an interrupted session would leave it modified \
                         — so there is no Play button here. Activate a profile and start \
                         the game however you normally do.",
                    )
                    .small()
                    .color(theme::MUTED),
                );
                return;
            }
            ui.add_space(6.0);

            match &current {
                Some(how) => {
                    ui.horizontal(|ui| {
                        let (rect, _) = ui.allocate_exact_size(
                            egui::vec2(14.0, 14.0),
                            egui::Sense::hover(),
                        );
                        theme::paint_glyph(
                            ui.painter(),
                            rect,
                            theme::Glyph::Play,
                            theme::GOOD,
                        );
                        ui.label(
                            egui::RichText::new(format!("Play will start: {how}"))
                                .color(theme::MUTED),
                        );
                    });
                }
                None => {
                    ui.label(
                        egui::RichText::new(
                            "Nothing here knows how to start this game. Its pack names no \
                             Steam app id and no executable — and a pack is not allowed to \
                             name a command, because packs come from strangers. Give it one \
                             yourself:",
                        )
                        .color(theme::WARN),
                    );
                }
            }
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                let changed = ui
                    .add(
                        egui::TextEdit::singleline(&mut text)
                            .desired_width(360.0)
                            .hint_text("your own command, e.g. a launcher"),
                    )
                    .changed();
                if changed {
                    ui.data_mut(|d| d.insert_temp(key, text.clone()));
                }
                if ui.button("Save").clicked() {
                    save = true;
                }
                if saved.is_some() && ui.button("Clear").clicked() {
                    clear = true;
                }
            });
            ui.label(
                egui::RichText::new(
                    "Yours overrides everything else. It runs from the game folder.",
                )
                .small()
                .color(theme::MUTED),
            );
        });

        if save || clear {
            let value = if clear { None } else { Some(text.trim()) };
            if let Some(engine) = self.engine() {
                match engine.set_launch_command(game, value) {
                    Ok(()) => {
                        if clear {
                            ui.data_mut(|d| d.insert_temp(key, String::new()));
                            self.log_line("Cleared this game's launch command.");
                        } else {
                            self.log_line("Saved this game's launch command.");
                        }
                        // The cache holds how Play would start this game.
                        self.reload_games();
                    }
                    Err(e) => self.log_line(e.to_string()),
                }
            }
        }
    }

    /// The profiles this game has, as cards.
    fn profiles_tab(&mut self, ui: &mut egui::Ui, game: &str) {
        ui.horizontal(|ui| {
            if theme::glyph_button(
                ui,
                theme::Glyph::Plus,
                "New profile",
                Some(theme::ACCENT_DIM),
                true,
            )
            .clicked()
            {
                self.new_profile_game = game.to_string();
                self.open_new_profile();
            }
            // Named for whose format it reads. The other menu also imports
            // modpacks — Modifile's own — so "a modpack" alone does not tell
            // anyone which of the two they want.
            ui.menu_button("From CurseForge, Modrinth or Thunderstore…", |ui| {
                ui.label(
                    egui::RichText::new(
                        "A modpack becomes a profile: its mod list, its game version \
                         and loader, and its config files.",
                    )
                    .small()
                    .color(theme::MUTED),
                );
                ui.separator();
                if ui.button("From a file…").clicked() {
                    let ctx = ui.ctx().clone();
                    self.pick_modpack_file(&ctx);
                    ui.close();
                }
                if ui.button("From a link…").clicked() {
                    self.modpack_input.clear();
                    self.show_modpack_link = true;
                    ui.close();
                }
            });
            ui.menu_button("Open a .mfpack…", |ui| {
                ui.label(
                    egui::RichText::new(
                        "Modifile's own pack, and the profiles people shared before \
                         it existed.",
                    )
                    .small()
                    .color(theme::MUTED),
                );
                ui.separator();
                if ui
                    .button("Exactly as they had it")
                    .on_hover_text("Holds every mod at the version the sender was running.")
                    .clicked()
                {
                    self.do_import_bundle(true);
                    ui.close();
                }
                if ui.button("But take the newest versions").clicked() {
                    self.do_import_bundle(false);
                    ui.close();
                }
            });
        });
        ui.add_space(12.0);

        let entries: Vec<crate::ProfileEntry> = self
            .profiles
            .iter()
            .filter(|p| p.game_id == game)
            .cloned()
            .collect();

        if entries.is_empty() {
            ui.add_space(16.0);
            ui.vertical_centered(|ui| {
                ui.label(
                    egui::RichText::new("No profiles for this game yet.")
                        .color(theme::MUTED),
                );
                ui.label(
                    egui::RichText::new(
                        "A profile is a named set of mods. Make one, or import a modpack.",
                    )
                    .small()
                    .color(theme::MUTED),
                );
            });
            ui.add_space(16.0);
            // Still shown: a game with no profiles is exactly when someone
            // needs to check Modifile found the right folder.
            self.folders_card(ui, game);
            return;
        }

        let mut open: Option<modifile_core::profile::ProfileId> = None;
        let mut rename: Option<modifile_core::profile::ProfileId> = None;
        let mut delete: Option<modifile_core::profile::ProfileId> = None;

        // Read from the cache: answering this properly means probing the
        // process list, which is far too expensive to do once per card per
        // frame.
        let running = self.game_info(game).and_then(|g| g.running.clone());

        for entry in &entries {
            // Last frame's hover state; see `card_frame_hovered`.
            let hover_id = egui::Id::new(("profile-card", &entry.name));
            let was_hovered = ui.data(|d| d.get_temp::<bool>(hover_id).unwrap_or(false));

            // The whole card opens the profile. `scope_builder` is what makes
            // that safe: a `Ui` created with a sense registers its rect when
            // the `Ui` is built, before its children, so the buttons inside
            // still take precedence. Sensing the frame's response afterwards
            // instead registers the card *last*, and it swallows every click
            // meant for a button inside it.
            let response = ui
                .scope_builder(
                    egui::UiBuilder::new().sense(egui::Sense::click()),
                    |ui| {
                        theme::card_frame_hovered(was_hovered).show(ui, |ui| {
                    ui.horizontal(|ui| {
                        crate::status_dot(ui, entry.active).on_hover_text(if entry.active {
                            "Active — these mods are in the game folder right now"
                        } else {
                            "Not active"
                        });
                        ui.add_space(4.0);
                        ui.vertical(|ui| {
                            ui.label(egui::RichText::new(&entry.name).strong().size(14.0));
                            ui.label(
                                egui::RichText::new(format!(
                                    "{} mod{}{}",
                                    entry.mods,
                                    if entry.mods == 1 { "" } else { "s" },
                                    match (&entry.loader, &entry.game_version) {
                                        (Some(l), Some(v)) => format!(" · {l} · {v}"),
                                        (Some(l), None) => format!(" · {l}"),
                                        (None, Some(v)) => format!(" · {v}"),
                                        (None, None) => String::new(),
                                    }
                                ))
                                .small()
                                .color(theme::MUTED),
                            );
                        });
                        ui.with_layout(
                            egui::Layout::right_to_left(egui::Align::Center),
                            |ui| {
                                // No Play here. The card is a link to the
                                // profile, and a button inside a clickable
                                // card is a target you have to aim at — the
                                // whole card reacts, so a miss opens the
                                // profile instead of starting the game. Play
                                // lives on the profile page beside Activate,
                                // where the state it depends on is visible.
                                //
                                // Two separate facts, two separate badges.
                                // "in game" used to mean the first and was
                                // read as the second.
                                if let Some(found) = &running {
                                    theme::badge(ui, "game open", theme::WARN)
                                        .on_hover_text(format!(
                                            "{found}. Mods cannot be changed until it \
                                             closes."
                                        ));
                                }
                                if entry.active {
                                    theme::badge(ui, "mods installed", theme::GOOD)
                                        .on_hover_text(
                                            "This profile's mods are in the game folder \
                                             right now — whether or not the game is \
                                             running. Deactivate puts the game back to \
                                             vanilla.",
                                        );
                                }
                            },
                        );
                    });
                        });
                    },
                )
                .response;
            ui.data_mut(|d| d.insert_temp(hover_id, response.hovered()));

            if response.clicked() {
                open = Some(entry.id.clone());
            }
            // The cursor is `theme::pointer_cursor`'s job now — the card
            // senses clicks, which is all that rule needs to know.

            response.context_menu(|ui| {
                ui.label(egui::RichText::new(&entry.name).small().color(theme::MUTED));
                ui.separator();
                if ui.button("Open").clicked() {
                    open = Some(entry.id.clone());
                    ui.close();
                }
                if ui.button("Rename…").clicked() {
                    rename = Some(entry.id.clone());
                    ui.close();
                }
                if ui
                    .button(egui::RichText::new("Delete…").color(theme::BAD))
                    .clicked()
                {
                    delete = Some(entry.id.clone());
                    ui.close();
                }
            });
            ui.add_space(6.0);
        }

        ui.add_space(10.0);
        self.folders_card(ui, game);
        ui.add_space(10.0);
        self.launch_card(ui, game);

        if let Some(id) = open {
            self.select(&id);
            self.view = View::Profile;
        }
        if let Some(id) = rename {
            self.select(&id);
            self.open_rename();
        }
        if let Some(id) = delete {
            self.select(&id);
            self.delete_input.clear();
            self.show_delete = true;
        }
    }

    /// Switch to a game's page, selecting something sensible to show.
    pub(crate) fn open_game(&mut self, id: &str) {
        self.game = Some(id.to_string());
        self.view = View::Game(GameTab::Profiles);
        self.search_results.clear();
        self.search_input.clear();

        // Opening a game with exactly one profile should land on it, because
        // that is plainly what was meant.
        let mine: Vec<modifile_core::profile::ProfileId> = self
            .profiles
            .iter()
            .filter(|p| p.game_id == id)
            .map(|p| p.id.clone())
            .collect();
        if let [only] = mine.as_slice() {
            self.select(only);
        } else if let Some(active) = self
            .profiles
            .iter()
            .find(|p| p.game_id == id && p.active)
            .map(|p| p.id.clone())
        {
            self.select(&active);
        } else {
            // Nothing obvious to select here, so select nothing. Leaving the
            // previous game's profile in place meant the page said Valheim
            // while every action still operated on a R.E.P.O. profile — press
            // Check for updates and you would download ninety mods for a game
            // you were not looking at.
            self.clear_selection();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::first_sentence;

    #[test]
    fn a_banner_gets_a_whole_sentence() {
        // Pack descriptions are hard-wrapped across several lines, so taking
        // the first *line* cut mid-sentence: the Valheim banner read
        // "BepInEx plugins. The dedicated server takes the same plugins as
        // the client, so" and stopped.
        let description = "BepInEx plugins. The dedicated server takes the same plugins\n\
                           as the client, so both are targets here.";
        assert_eq!(first_sentence(description), "BepInEx plugins.");
    }

    #[test]
    fn a_description_with_no_break_survives_whole() {
        assert_eq!(
            first_sentence("Loader mods for Fabric and Forge"),
            "Loader mods for Fabric and Forge"
        );
        assert_eq!(first_sentence(""), "");
    }

    #[test]
    fn a_decimal_point_is_not_a_sentence_end() {
        // "1.20.1" must not split the sentence; only ". " does.
        assert_eq!(
            first_sentence("Needs Minecraft 1.20.1 or newer to run."),
            "Needs Minecraft 1.20.1 or newer to run."
        );
    }
}
