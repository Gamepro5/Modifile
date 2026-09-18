//! The desktop front end.
//!
//! egui on glow: one static binary, no webview, no Node, no bundled browser.
//!
//! This is a complete front end, not a viewer over the CLI. Anything you can do
//! with `modifile` you can do here: pick game folders, save a token, create
//! profiles, add mods, sync, deploy. If a task needs the terminal, that is a
//! bug in this file.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod theme;

use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};

use eframe::egui;
use modifile_core::deploy::LinkMode;
use modifile_core::engine::{format_bytes, Event};
use modifile_core::pack::Target;
use modifile_core::profile::ModEntry;
use modifile_core::{Engine, Lock, ModId, Paths, Profile, TrustLevel};

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1100.0, 720.0])
            .with_min_inner_size([820.0, 560.0])
            .with_title("Modifile"),
        ..Default::default()
    };
    eframe::run_native(
        "modifile",
        options,
        Box::new(|cc| {
            theme::apply(&cc.egui_ctx);
            Ok(Box::new(App::new()))
        }),
    )
}

/// Background threads report terminal states here; progress goes to the log.
enum Msg {
    Error(String),
    Finished,
}

#[derive(PartialEq, Clone, Copy)]
enum View {
    Profile,
    Games,
    Settings,
}

struct ModRow {
    id: ModId,
    enabled: bool,
    version: String,
    trust: Option<TrustLevel>,
    note: String,
    pinned: bool,
}

struct TargetRow {
    target: Target,
    root: Option<PathBuf>,
    /// True when the path came from the user rather than autodetection.
    remembered: bool,
    deployed: Option<(String, usize, Option<LinkMode>)>,
    /// Set while the game is running; mod changes are refused until it closes.
    running: Option<String>,
    /// A folder on another machine, whose processes we cannot see.
    remote: bool,
}

struct App {
    paths: Paths,
    engine: Option<Engine>,
    runtime: tokio::runtime::Runtime,

    view: View,
    profiles: Vec<String>,
    selected: Option<String>,

    profile: Option<Profile>,
    lock: Lock,
    targets: Vec<TargetRow>,
    rows: Vec<ModRow>,
    /// Config files this profile is keeping, across all its targets.
    config_files: Vec<PathBuf>,

    add_input: String,
    token_input: String,
    /// Open folder chooser, as (game id, target).
    root_dialog: Option<(String, Target)>,
    root_input: String,
    /// Ticked by the user for game folders on another machine, which we cannot
    /// check ourselves. Deliberately not remembered between runs.
    assume_stopped: bool,
    new_profile_name: String,
    new_profile_game: String,
    show_new_profile: bool,

    log: Arc<Mutex<Vec<String>>>,
    busy: bool,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
}

impl App {
    fn new() -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        let paths = Paths::discover().unwrap_or_else(|_| Paths::rooted("."));
        let _ = paths.ensure();
        // A fresh install is useful immediately: the bundled packs land on
        // first run, so the Games list is never empty.
        let _ = modifile_core::install_bundled_packs(&paths);

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("tokio runtime");

        let token = load_token(&paths);
        let mut app = Self {
            engine: Engine::open(paths.clone(), token.clone()).ok(),
            paths,
            runtime,
            view: View::Profile,
            profiles: Vec::new(),
            selected: None,
            profile: None,
            lock: Lock::default(),
            targets: Vec::new(),
            rows: Vec::new(),
            config_files: Vec::new(),
            add_input: String::new(),
            token_input: token.unwrap_or_default(),
            root_dialog: None,
            root_input: String::new(),
            assume_stopped: false,
            new_profile_name: String::new(),
            new_profile_game: String::new(),
            show_new_profile: false,
            log: Arc::new(Mutex::new(Vec::new())),
            busy: false,
            tx,
            rx,
        };
        app.reload_profiles();
        if let Some(first) = app.profiles.first().cloned() {
            app.select(&first);
        }
        app
    }

    fn engine(&self) -> Option<&Engine> {
        self.engine.as_ref()
    }

    fn reload_profiles(&mut self) {
        self.profiles.clear();
        if let Ok(entries) = std::fs::read_dir(&self.paths.profiles) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) == Some("toml") {
                    if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                        self.profiles.push(stem.to_string());
                    }
                }
            }
        }
        self.profiles.sort();
    }

    fn select(&mut self, name: &str) {
        self.selected = Some(name.to_string());
        self.view = View::Profile;
        self.rows.clear();
        self.targets.clear();
        self.config_files.clear();
        self.lock = Lock::default();

        let Ok(profile) = Profile::load(&self.paths.profile_file(name)) else {
            self.profile = None;
            return;
        };
        self.lock = Lock::load(&self.paths.lock_file(name)).unwrap_or_default();

        let mut targets = Vec::new();
        let mut configs = Vec::new();
        if let Some(engine) = self.engine() {
            if let Ok(pack) = engine.pack_for(&profile) {
                for (target, root) in engine.targets(pack, &profile) {
                    configs.extend(engine.saved_configs(pack, name, &target));
                    let remembered = engine.roots.get(pack.id(), &target.id).is_some()
                        || profile.roots.contains_key(&target.id);
                    let deployed = engine
                        .manifest(pack.id(), &target.id)
                        .ok()
                        .flatten()
                        .map(|m| (m.profile, m.files.len(), m.mode));
                    // Checked here rather than per frame: enumerating processes
                    // costs tens of milliseconds and the answer only matters
                    // when the user is about to act.
                    let remote = root
                        .as_ref()
                        .map(|r| modifile_core::paths::is_network_path(r))
                        .unwrap_or(false);
                    let running = root
                        .as_ref()
                        .filter(|_| !remote)
                        .and_then(|r| Engine::running(&target, r))
                        .map(|found| found.to_string());
                    targets.push(TargetRow {
                        target,
                        root,
                        remembered,
                        deployed,
                        running,
                        remote,
                    });
                }
            }
        }
        self.targets = targets;
        self.config_files = configs;

        for entry in &profile.mods {
            let locked = self.lock.get(&entry.id);
            self.rows.push(ModRow {
                id: entry.id.clone(),
                enabled: entry.enabled,
                version: locked
                    .map(|l| l.version.clone())
                    .unwrap_or_else(|| "not synced".into()),
                trust: locked.map(|l| l.trust.level),
                note: locked.map(|l| l.trust.notes.join("; ")).unwrap_or_default(),
                pinned: entry.pin.is_some(),
            });
        }
        self.profile = Some(profile);
    }

    fn refresh(&mut self) {
        // Cheapest way to stay honest: re-read from disk after every mutation.
        if let Some(engine) = self.engine.as_mut() {
            engine.roots =
                modifile_core::roots::GlobalRoots::load(&self.paths.roots_file()).unwrap_or_default();
        }
        if let Some(name) = self.selected.clone() {
            self.select(&name);
        }
    }

    fn log_line(&self, line: impl Into<String>) {
        if let Ok(mut log) = self.log.lock() {
            log.push(line.into());
            if log.len() > 500 {
                let excess = log.len() - 500;
                log.drain(..excess);
            }
        }
    }

    // --- actions ----------------------------------------------------------

    fn do_sync(&mut self, ctx: &egui::Context) {
        let Some(profile) = self.profile.clone() else {
            return;
        };
        let paths = self.paths.clone();
        let log = self.log.clone();
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        self.busy = true;
        self.log_line(format!("Syncing {}…", profile.name));

        let handle = self.runtime.handle().clone();
        std::thread::spawn(move || {
            let push = {
                let log = log.clone();
                let ctx = ctx.clone();
                move |line: String| {
                    if let Ok(mut log) = log.lock() {
                        log.push(line);
                    }
                    ctx.request_repaint();
                }
            };

            let result = handle.block_on(async {
                let engine = Engine::open(paths.clone(), load_token(&paths))?;
                let pack = engine.pack_for(&profile)?;
                let lock_path = paths.lock_file(&profile.name);
                let previous = Lock::load(&lock_path)?;

                let reporter = {
                    let push = push.clone();
                    Arc::new(move |event: Event| match event {
                        Event::Resolved { id, version } => {
                            push(format!("resolved {id} → {version}"))
                        }
                        Event::Downloading { id, asset, size } => {
                            push(format!("downloading {id}: {asset} ({})", format_bytes(size)))
                        }
                        Event::Cached { id, version } => {
                            push(format!("already have {id} {version}"))
                        }
                        Event::Failed { id, error } => push(format!("FAILED {id}: {error}")),
                        _ => {}
                    })
                };

                let (lock, failures) =
                    engine.sync(pack, &profile, &previous, Some(reporter)).await?;
                lock.save(&lock_path)?;
                Ok::<_, modifile_core::Error>((lock.mods.len(), failures.len()))
            });

            match result {
                Ok((ok, failed)) => {
                    if failed > 0 {
                        push(format!("{ok} ready, {failed} failed."));
                    } else {
                        push(format!("{ok} mod(s) ready. Press Deploy to install them."));
                    }
                    let _ = tx.send(Msg::Finished);
                }
                Err(e) => {
                    let _ = tx.send(Msg::Error(e.to_string()));
                }
            }
            ctx.request_repaint();
        });
    }

    fn do_deploy(&mut self, force: bool) {
        let (Some(profile), Some(engine)) = (self.profile.clone(), self.engine()) else {
            return;
        };
        let Ok(pack) = engine.pack_for(&profile) else {
            return;
        };
        let lock = self.lock.clone();
        let mut messages = Vec::new();
        let mut deployed = false;

        for row in &self.targets {
            let Some(root) = &row.root else {
                messages.push(format!(
                    "{}: no folder set — press Choose folder first",
                    row.target.name
                ));
                continue;
            };
            match engine
                .plan(pack, &profile, &lock, &row.target, root)
                .and_then(|plan| {
                    let report = engine.deploy(
                        pack,
                        &row.target,
                        &plan,
                        &profile.name,
                        modifile_core::engine::DeployOptions {
                            force,
                            assume_stopped: self.assume_stopped,
                        },
                    )?;
                    Ok((plan, report))
                }) {
                Ok((plan, report)) => {
                    deployed = true;
                    messages.push(format!(
                        "{}: {} file(s) linked ({}), {} removed",
                        row.target.name,
                        report.linked,
                        format_bytes(report.bytes),
                        report.removed
                    ));
                    if report.captured > 0 || report.restored > 0 || report.seeded > 0 {
                        messages.push(format!(
                            "  configs: {} saved to the old profile, {} restored, {} created",
                            report.captured, report.restored, report.seeded
                        ));
                    }
                    for conflict in &plan.conflicts {
                        messages.push(format!(
                            "  {} — {} wins over {}",
                            display_path(&conflict.rel),
                            conflict.winner,
                            conflict.losers.join(", ")
                        ));
                    }
                    for (path, reason) in &report.skipped {
                        messages.push(format!("  skipped {}: {reason}", display_path(path)));
                    }
                }
                Err(e) => messages.push(format!("{}: {e}", row.target.name)),
            }
        }
        if deployed {
            messages.push("Done. Launch the game normally — nothing needs to stay open.".into());
        }
        for message in messages {
            self.log_line(message);
        }
        self.refresh();
    }

    fn do_verify(&mut self) {
        let (Some(profile), Some(engine)) = (self.profile.clone(), self.engine()) else {
            return;
        };
        let Ok(pack) = engine.pack_for(&profile) else {
            return;
        };
        let mut messages = Vec::new();
        for row in &self.targets {
            let Ok(Some(manifest)) = engine.manifest(pack.id(), &row.target.id) else {
                continue;
            };
            let report = modifile_core::deploy::verify(&manifest);
            messages.push(if report.is_clean() {
                format!("{}: all {} file(s) intact", row.target.name, report.ok)
            } else {
                format!(
                    "{}: {} intact, {} missing, {} changed — a game update probably \
                     overwrote them; press Deploy again",
                    row.target.name,
                    report.ok,
                    report.missing.len(),
                    report.modified.len()
                )
            });
        }
        if messages.is_empty() {
            messages.push("Nothing deployed yet.".into());
        }
        for message in messages {
            self.log_line(message);
        }
    }

    fn do_undeploy(&mut self) {
        let (Some(profile), Some(engine)) = (self.profile.clone(), self.engine()) else {
            return;
        };
        let Ok(pack) = engine.pack_for(&profile) else {
            return;
        };
        let game = pack.id().to_string();
        let ids: Vec<String> = self.targets.iter().map(|r| r.target.id.clone()).collect();
        let mut messages = Vec::new();
        for target in ids {
            match engine.undeploy(
                &game,
                &target,
                modifile_core::engine::DeployOptions {
                    force: false,
                    assume_stopped: self.assume_stopped,
                },
            ) {
                Ok(report) => messages.push(format!("{target}: {} file(s) removed", report.removed)),
                Err(e) => messages.push(format!("{target}: {e}")),
            }
        }
        for message in messages {
            self.log_line(message);
        }
        self.refresh();
    }

    fn do_add(&mut self) {
        let input = self.add_input.trim().to_string();
        if input.is_empty() {
            return;
        }
        let Some(name) = self.selected.clone() else {
            return;
        };
        match input.parse::<ModId>() {
            Ok(id) => {
                let path = self.paths.profile_file(&name);
                if let Ok(mut profile) = Profile::load(&path) {
                    if profile.add(ModEntry::new(id.clone())) {
                        let _ = profile.save(&path);
                        self.log_line(format!("Added {id}. Press Sync to fetch it."));
                    } else {
                        self.log_line(format!("{id} is already in this profile."));
                    }
                }
                self.add_input.clear();
                self.refresh();
            }
            Err(e) => self.log_line(e.to_string()),
        }
    }

    fn do_remove(&mut self, id: &ModId) {
        let Some(name) = self.selected.clone() else {
            return;
        };
        let path = self.paths.profile_file(&name);
        if let Ok(mut profile) = Profile::load(&path) {
            profile.remove(id);
            let _ = profile.save(&path);
            self.log_line(format!("Removed {id}. Press Deploy to take it out of the game."));
        }
        self.refresh();
    }

    fn do_toggle(&mut self, id: &ModId) {
        let Some(name) = self.selected.clone() else {
            return;
        };
        let path = self.paths.profile_file(&name);
        if let Ok(mut profile) = Profile::load(&path) {
            if let Some(entry) = profile.mods.iter_mut().find(|m| &m.id == id) {
                entry.enabled = !entry.enabled;
            }
            let _ = profile.save(&path);
        }
        self.refresh();
    }

    /// Open the folder chooser for a target.
    ///
    /// This is a dialog rather than a straight call to the native picker on
    /// purpose. On Linux `rfd` goes through the XDG desktop portal, and on a
    /// box without `xdg-desktop-portal` installed it returns nothing at all —
    /// which would leave the user with a button that does nothing and no way
    /// to proceed. Typing a path always works, so that is the primary control
    /// and Browse is the convenience.
    fn open_root_dialog(&mut self, game: &str, target: &Target) {
        self.root_input = self
            .engine()
            .and_then(|e| e.roots.get(game, &target.id).cloned())
            .or_else(|| {
                self.targets
                    .iter()
                    .find(|r| r.target.id == target.id)
                    .and_then(|r| r.root.clone())
            })
            .map(|p| display_path(&p))
            .unwrap_or_default();
        self.root_dialog = Some((game.to_string(), target.clone()));
    }

    /// Native picker, if this system has one. `None` also means "the portal is
    /// missing", which is not an error worth shouting about.
    fn browse_for_folder(&self, target: &Target, start: &str) -> Option<PathBuf> {
        let mut dialog = rfd::FileDialog::new().set_title(format!("Where is {}?", target.name));
        if !start.is_empty() && Path::new(start).is_dir() {
            dialog = dialog.set_directory(start);
        }
        dialog.pick_folder()
    }

    fn commit_root(&mut self, game: &str, target: &Target, path: PathBuf) {
        // Say if the folder does not look right, but allow it anyway —
        // unusual installs are exactly why this control exists.
        let looks_right = self
            .engine()
            .and_then(|e| e.pack(game))
            .map(|pack| pack.matches_markers(target, &path))
            .unwrap_or(false);

        if let Some(engine) = self.engine.as_mut() {
            match engine.set_root(game, &target.id, path.clone()) {
                Ok(()) => {
                    let note = if looks_right {
                        ""
                    } else {
                        " (warning: no expected game file found here)"
                    };
                    self.log_line(format!("{} → {}{note}", target.name, display_path(&path)));
                }
                Err(e) => self.log_line(e.to_string()),
            }
        }
        self.root_dialog = None;
        self.refresh();
    }

    fn root_dialog_window(&mut self, ctx: &egui::Context) {
        let Some((game, target)) = self.root_dialog.clone() else {
            return;
        };
        let mut open = true;
        let mut commit: Option<PathBuf> = None;
        let mut cancel = false;

        egui::Window::new(format!("Where is {}?", target.name))
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .open(&mut open)
            .show(ctx, |ui| {
                ui.add_space(4.0);
                ui.label("Folder");
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.root_input)
                            .desired_width(400.0)
                            .hint_text("/home/you/ValheimServer/server"),
                    );
                    if ui.button("Browse…").clicked() {
                        if let Some(picked) = self.browse_for_folder(&target, &self.root_input) {
                            self.root_input = display_path(&picked);
                        } else {
                            self.log_line(
                                "No folder chosen. If the Browse button does nothing, type the \
                                 path here instead — on Linux it needs xdg-desktop-portal.",
                            );
                        }
                    }
                });

                ui.add_space(6.0);
                let typed = Path::new(self.root_input.trim());
                let exists = !self.root_input.trim().is_empty() && typed.is_dir();
                let looks_right = exists
                    && self
                        .engine()
                        .and_then(|e| e.pack(&game))
                        .map(|pack| pack.matches_markers(&target, typed))
                        .unwrap_or(false);

                let (message, color) = if self.root_input.trim().is_empty() {
                    (
                        format!("Look for the folder containing {}", markers_hint(&target)),
                        theme::MUTED,
                    )
                } else if !exists {
                    ("That folder does not exist.".to_string(), theme::BAD)
                } else if looks_right {
                    ("Looks right.".to_string(), theme::GOOD)
                } else {
                    (
                        format!(
                            "No {} here — you can still use it, but check the path.",
                            markers_hint(&target)
                        ),
                        theme::WARN,
                    )
                };
                ui.label(egui::RichText::new(message).small().color(color));

                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(exists, egui::Button::new("Use this folder"))
                        .clicked()
                    {
                        commit = Some(typed.to_path_buf());
                    }
                    if ui.button("Cancel").clicked() {
                        cancel = true;
                    }
                });
                ui.add_space(2.0);
            });

        if let Some(path) = commit {
            self.commit_root(&game, &target, path);
        } else if cancel || !open {
            self.root_dialog = None;
        }
    }

    fn forget_root(&mut self, game: &str, target_id: &str) {
        if let Some(engine) = self.engine.as_mut() {
            let _ = engine.clear_root(game, target_id);
        }
        self.log_line("Folder forgotten; back to autodetection.");
        self.refresh();
    }

    fn save_token(&mut self) {
        let token = self.token_input.clone();
        if let Some(engine) = self.engine.as_mut() {
            match engine.set_token(&token) {
                Ok(()) if token.trim().is_empty() => {
                    self.log_line("Token cleared.");
                }
                Ok(()) => self.log_line("Token saved. 5000 requests/hour."),
                Err(e) => self.log_line(e.to_string()),
            }
        }
    }

    fn create_profile(&mut self) {
        let name = self.new_profile_name.trim().to_string();
        let game = self.new_profile_game.clone();
        if name.is_empty() || game.is_empty() {
            self.log_line("A profile needs a name and a game.");
            return;
        }
        let path = self.paths.profile_file(&name);
        if path.exists() {
            self.log_line(format!("Profile `{name}` already exists."));
            return;
        }
        if let Err(e) = Profile::new(&name, &game).save(&path) {
            self.log_line(e.to_string());
            return;
        }
        self.show_new_profile = false;
        self.new_profile_name.clear();
        self.reload_profiles();
        self.select(&name);
        self.log_line(format!("Created `{name}`. Add mods below, then Sync."));
    }

    fn open_new_profile(&mut self) {
        if self.new_profile_game.is_empty() {
            self.new_profile_game = self
                .engine()
                .and_then(|e| e.packs.first())
                .map(|p| p.id().to_string())
                .unwrap_or_default();
        }
        self.show_new_profile = true;
    }

    fn run_gc(&mut self) {
        if let Some(engine) = self.engine() {
            match engine.gc() {
                Ok((entries, bytes)) => self.log_line(format!(
                    "Removed {entries} unused download(s), freeing {}.",
                    format_bytes(bytes)
                )),
                Err(e) => self.log_line(e.to_string()),
            }
        }
    }
}

fn load_token(paths: &Paths) -> Option<String> {
    for var in ["MODIFILE_GITHUB_TOKEN", "GITHUB_TOKEN", "GH_TOKEN"] {
        if let Ok(token) = std::env::var(var) {
            if !token.trim().is_empty() {
                return Some(token.trim().to_string());
            }
        }
    }
    std::fs::read_to_string(paths.token_file())
        .ok()
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
}

fn display_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// The file that identifies a target, for telling the user what to look for.
fn markers_hint(target: &Target) -> String {
    match target.markers.first() {
        Some(first) => first.clone(),
        None => "the game files".to_string(),
    }
}

/// Show a folder in the system file manager.
fn reveal(path: &Path) {
    let _ = if cfg!(windows) {
        std::process::Command::new("explorer").arg(path).spawn()
    } else if cfg!(target_os = "macos") {
        std::process::Command::new("open").arg(path).spawn()
    } else {
        std::process::Command::new("xdg-open").arg(path).spawn()
    };
}

// ---------------------------------------------------------------------------
// Painting
// ---------------------------------------------------------------------------

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                Msg::Error(e) => {
                    self.busy = false;
                    self.log_line(format!("error: {e}"));
                }
                Msg::Finished => {
                    self.busy = false;
                    self.refresh();
                }
            }
        }

        self.top_bar(ui);
        self.status_bar(ui);
        self.sidebar(ui);
        self.log_panel(ui);

        egui::CentralPanel::default()
            .frame(theme::panel_frame())
            .show(ui, |ui| match self.view {
                View::Profile => self.profile_view(ui),
                View::Games => self.games_view(ui),
                View::Settings => self.settings_view(ui),
            });

        if self.show_new_profile {
            let ctx = ui.ctx().clone();
            self.new_profile_window(&ctx);
        }
        if self.root_dialog.is_some() {
            let ctx = ui.ctx().clone();
            self.root_dialog_window(&ctx);
        }
    }
}

impl App {
    fn top_bar(&mut self, ui: &mut egui::Ui) {
        egui::Panel::top("top")
            .frame(theme::bar_frame())
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.heading("Modifile");
                    ui.add_space(10.0);
                    ui.label(
                        egui::RichText::new("universal mod manager").color(theme::MUTED),
                    );

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let ready = self.profile.is_some() && !self.busy;
                        let has_mods = !self.rows.is_empty();
                        let has_lock = !self.lock.mods.is_empty();
                        let running = self
                            .targets
                            .iter()
                            .find_map(|t| t.running.clone());

                        if ui
                            .add_enabled(
                                ready && has_lock && running.is_none(),
                                egui::Button::new("2 · Deploy").fill(theme::ACCENT_DIM),
                            )
                            .on_hover_text("Link the downloaded mods into your game folder")
                            .on_disabled_hover_text(match &running {
                                Some(found) => format!(
                                    "The game is running ({found}).\nClose it before changing mods."
                                ),
                                None => "Press Sync first".to_string(),
                            })
                            .clicked()
                        {
                            self.do_deploy(false);
                        }
                        if ui
                            .add_enabled(
                                ready && has_mods,
                                egui::Button::new("1 · Sync").fill(theme::ACCENT_DIM),
                            )
                            .on_hover_text("Check GitHub for the latest versions and download them")
                            .on_disabled_hover_text("Add a mod first")
                            .clicked()
                        {
                            let ctx = ui.ctx().clone();
                            self.do_sync(&ctx);
                        }

                        ui.add_space(6.0);
                        ui.menu_button("More", |ui| {
                            if ui.button("Check installed files").clicked() {
                                self.do_verify();
                                ui.close();
                            }
                            if ui.button("Remove all mods from game").clicked() {
                                self.do_undeploy();
                                ui.close();
                            }
                            if ui.button("Deploy, overwriting other files").clicked() {
                                self.do_deploy(true);
                                ui.close();
                            }
                        });
                        if self.busy {
                            ui.add(egui::Spinner::new());
                        }
                    });
                });
            });
    }

    fn status_bar(&mut self, ui: &mut egui::Ui) {
        egui::Panel::bottom("status")
            .frame(theme::bar_frame())
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    let store = self
                        .engine()
                        .map(|e| e.store.size_bytes())
                        .unwrap_or_default();
                    ui.label(
                        egui::RichText::new(format!("downloads {}", format_bytes(store)))
                            .color(theme::MUTED),
                    );
                    ui.separator();

                    let authed = self
                        .engine()
                        .map(|e| e.github.http().has_token())
                        .unwrap_or(false);
                    if authed {
                        ui.label(egui::RichText::new("GitHub token set").color(theme::MUTED));
                    } else if ui
                        .link(egui::RichText::new("no GitHub token — limited to 60 checks/hour").color(theme::WARN))
                        .clicked()
                    {
                        self.view = View::Settings;
                    }
                });
            });
    }

    fn sidebar(&mut self, ui: &mut egui::Ui) {
        egui::Panel::left("nav")
            .exact_size(216.0)
            .frame(theme::panel_frame())
            .show(ui, |ui| {
                ui.add_space(4.0);
                if ui
                    .add_sized(
                        [ui.available_width(), 30.0],
                        egui::Button::new("＋  New profile").fill(theme::ACCENT_DIM),
                    )
                    .clicked()
                {
                    self.open_new_profile();
                }
                ui.add_space(10.0);

                ui.label(egui::RichText::new("PROFILES").small().color(theme::MUTED));
                ui.add_space(4.0);
                if self.profiles.is_empty() {
                    ui.label(egui::RichText::new("none yet").color(theme::MUTED));
                }
                let profiles = self.profiles.clone();
                for name in profiles {
                    let selected =
                        self.view == View::Profile && self.selected.as_deref() == Some(name.as_str());
                    if ui.selectable_label(selected, &name).clicked() {
                        self.select(&name);
                    }
                }

                ui.add_space(14.0);
                ui.separator();
                ui.add_space(6.0);

                if ui
                    .selectable_label(self.view == View::Games, "Games & folders")
                    .clicked()
                {
                    self.view = View::Games;
                }
                if ui
                    .selectable_label(self.view == View::Settings, "Settings")
                    .clicked()
                {
                    self.view = View::Settings;
                }
            });
    }

    fn log_panel(&mut self, ui: &mut egui::Ui) {
        egui::Panel::bottom("log")
            .resizable(true)
            .default_size(128.0)
            .frame(theme::panel_frame())
            .show(ui, |ui| {
                ui.add_space(2.0);
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("ACTIVITY").small().color(theme::MUTED));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.small_button("clear").clicked() {
                            if let Ok(mut log) = self.log.lock() {
                                log.clear();
                            }
                        }
                    });
                });
                ui.add_space(2.0);
                egui::ScrollArea::vertical()
                    .stick_to_bottom(true)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        if let Ok(log) = self.log.lock() {
                            if log.is_empty() {
                                ui.label(
                                    egui::RichText::new("Nothing yet.").color(theme::MUTED),
                                );
                            }
                            for line in log.iter() {
                                let color = if line.contains("FAILED") || line.starts_with("error")
                                {
                                    theme::BAD
                                } else if line.starts_with("  ") {
                                    theme::MUTED
                                } else {
                                    theme::TEXT
                                };
                                ui.label(egui::RichText::new(line).monospace().color(color));
                            }
                        }
                    });
            });
    }

    // --- views ------------------------------------------------------------

    fn profile_view(&mut self, ui: &mut egui::Ui) {
        if self.profile.is_none() {
            self.welcome(ui);
            return;
        }
        let profile = self.profile.clone().expect("checked above");
        let game_name = self
            .engine()
            .and_then(|e| e.pack(&profile.game))
            .map(|p| p.pack.game.name.clone())
            .unwrap_or_else(|| profile.game.clone());

        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.heading(&profile.name);
            ui.label(egui::RichText::new(game_name).color(theme::MUTED));
        });

        ui.add_space(10.0);
        self.targets_section(ui, &profile.game);
        ui.add_space(12.0);
        self.configs_section(ui);
        ui.add_space(12.0);
        self.mods_section(ui);
    }

    /// Config files belong to the profile, so this is where you reset them to
    /// the mods' defaults or pull them in from somewhere else.
    fn configs_section(&mut self, ui: &mut egui::Ui) {
        let saved = self.config_files.len();
        let mut reset = false;
        let mut import: Option<modifile_core::engine::ConfigSource> = None;
        let others: Vec<String> = self
            .profiles
            .iter()
            .filter(|p| Some(p.as_str()) != self.selected.as_deref())
            .cloned()
            .collect();

        ui.label(egui::RichText::new("CONFIGS").small().color(theme::MUTED));
        ui.add_space(4.0);
        egui::Frame::NONE
            .fill(theme::CARD)
            .corner_radius(6.0)
            .inner_margin(egui::Margin::symmetric(10, 8))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new(if saved == 0 {
                            "No saved settings yet — they are kept the moment you switch away \
                             from this profile."
                                .to_string()
                        } else {
                            format!("{saved} file(s) kept for this profile only")
                        })
                        .color(theme::MUTED),
                    );

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.menu_button("Import from…", |ui| {
                            if ui
                                .button("This game's current folder")
                                .on_hover_text(
                                    "Adopt settings already in the game folder, e.g. from \
                                     modding it by hand",
                                )
                                .clicked()
                            {
                                import = Some(modifile_core::engine::ConfigSource::Game);
                                ui.close();
                            }
                            if others.is_empty() {
                                ui.label(
                                    egui::RichText::new("no other profiles").color(theme::MUTED),
                                );
                            }
                            for other in &others {
                                if ui.button(other).clicked() {
                                    import = Some(
                                        modifile_core::engine::ConfigSource::Profile(
                                            other.clone(),
                                        ),
                                    );
                                    ui.close();
                                }
                            }
                        });
                        if ui
                            .add_enabled(saved > 0, egui::Button::new("Reset to defaults"))
                            .on_hover_text(
                                "Discard this profile's settings. The mods' originals are kept \
                                 in the download store, so nothing is lost permanently.",
                            )
                            .clicked()
                        {
                            reset = true;
                        }
                    });
                });
            });

        if reset {
            self.do_reset_configs();
        }
        if let Some(source) = import {
            self.do_import_configs(source);
        }
    }

    fn do_reset_configs(&mut self) {
        let (Some(profile), Some(engine)) = (self.profile.clone(), self.engine()) else {
            return;
        };
        let Ok(pack) = engine.pack_for(&profile) else {
            return;
        };
        let mut messages = Vec::new();
        for row in &self.targets {
            match engine.reset_configs(pack, &profile.name, &row.target, row.root.as_deref()) {
                Ok(count) if count > 0 => messages.push(format!(
                    "{}: discarded {count} config file(s) — press Deploy to restore defaults",
                    row.target.name
                )),
                Ok(_) => {}
                Err(e) => messages.push(format!("{}: {e}", row.target.name)),
            }
        }
        for message in messages {
            self.log_line(message);
        }
        self.refresh();
    }

    fn do_import_configs(&mut self, source: modifile_core::engine::ConfigSource) {
        let (Some(profile), Some(engine)) = (self.profile.clone(), self.engine()) else {
            return;
        };
        let Ok(pack) = engine.pack_for(&profile) else {
            return;
        };
        let mut messages = Vec::new();
        for row in &self.targets {
            match engine.import_configs(
                pack,
                &profile.name,
                &row.target,
                &source,
                row.root.as_deref(),
            ) {
                Ok(count) if count > 0 => {
                    messages.push(format!("{}: imported {count} config file(s)", row.target.name))
                }
                Ok(_) => {}
                Err(e) => messages.push(format!("{}: {e}", row.target.name)),
            }
        }
        if messages.is_empty() {
            messages.push("Nothing to import.".to_string());
        }
        for message in messages {
            self.log_line(message);
        }
        self.refresh();
    }

    /// What a first-time user sees. It has to answer "what do I do now".
    fn welcome(&mut self, ui: &mut egui::Ui) {
        ui.add_space(30.0);
        ui.vertical_centered(|ui| {
            ui.heading("Welcome to Modifile");
            ui.add_space(6.0);
            ui.label(
                egui::RichText::new(
                    "Mods from GitHub, linked into your game. Nothing stays running while you play.",
                )
                .color(theme::MUTED),
            );
            ui.add_space(22.0);
        });

        egui::Frame::NONE
            .fill(theme::CARD)
            .corner_radius(8.0)
            .inner_margin(egui::Margin::symmetric(18, 16))
            .show(ui, |ui| {
                for (step, text) in [
                    ("1", "Make a profile and pick which game it is for."),
                    ("2", "Add mods by pasting a GitHub repo, like WeakAuras/WeakAuras2."),
                    ("3", "Press Sync to download them, then Deploy to install."),
                    ("4", "Launch the game however you normally do."),
                ] {
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new(step).strong().color(theme::ACCENT));
                        ui.label(text);
                    });
                    ui.add_space(6.0);
                }
            });

        ui.add_space(18.0);
        ui.vertical_centered(|ui| {
            if ui
                .add_sized([230.0, 34.0], egui::Button::new("Create your first profile").fill(theme::ACCENT_DIM))
                .clicked()
            {
                self.open_new_profile();
            }
            ui.add_space(8.0);
            if ui.link("See which games were found on this PC").clicked() {
                self.view = View::Games;
            }
        });
    }

    fn targets_section(&mut self, ui: &mut egui::Ui, game: &str) {
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("GAME FOLDERS").small().color(theme::MUTED));
            ui.label(
                egui::RichText::new("where mods get installed")
                    .small()
                    .color(theme::MUTED),
            );
        });
        ui.add_space(4.0);

        let mut choose: Option<Target> = None;
        let mut forget: Option<String> = None;

        for row in &self.targets {
            egui::Frame::NONE
                .fill(theme::CARD)
                .corner_radius(6.0)
                .inner_margin(egui::Margin::symmetric(10, 8))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        let (badge, color) = match row.target.kind {
                            modifile_core::TargetKind::Server => ("SERVER", theme::ACCENT),
                            modifile_core::TargetKind::Client => ("CLIENT", theme::MUTED),
                        };
                        ui.label(egui::RichText::new(badge).small().color(color));
                        ui.label(egui::RichText::new(&row.target.name).strong());

                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if row.root.is_some() {
                                if ui.small_button("Change…").clicked() {
                                    choose = Some(row.target.clone());
                                }
                                if row.remembered && ui.small_button("Forget").clicked() {
                                    forget = Some(row.target.id.clone());
                                }
                            } else if ui.button("Choose folder…").clicked() {
                                choose = Some(row.target.clone());
                            }

                            match &row.deployed {
                                Some((name, count, mode)) => {
                                    ui.label(
                                        egui::RichText::new(format!(
                                            "{count} file(s) from `{name}` ({})",
                                            mode.map(LinkMode::label).unwrap_or("?")
                                        ))
                                        .color(theme::GOOD),
                                    );
                                }
                                None if row.root.is_some() => {
                                    ui.label(
                                        egui::RichText::new("nothing installed")
                                            .color(theme::MUTED),
                                    );
                                }
                                None => {
                                    ui.label(
                                        egui::RichText::new("folder not found")
                                            .color(theme::WARN),
                                    );
                                }
                            }
                        });
                    });
                    if let Some(root) = &row.root {
                        ui.label(
                            egui::RichText::new(display_path(root))
                                .small()
                                .color(theme::MUTED),
                        );
                    }
                    if let Some(found) = &row.running {
                        ui.label(
                            egui::RichText::new(format!(
                                "⚠ running now ({found}) — close the game to change mods"
                            ))
                            .small()
                            .color(theme::WARN),
                        );
                    }
                    if row.remote {
                        ui.horizontal(|ui| {
                            ui.checkbox(&mut self.assume_stopped, "");
                            ui.label(
                                egui::RichText::new(
                                    "on another machine — tick to confirm the server is stopped",
                                )
                                .small()
                                .color(theme::WARN),
                            );
                        });
                    }
                });
            ui.add_space(4.0);
        }

        if let Some(target) = choose {
            self.open_root_dialog(game, &target);
        }
        if let Some(target_id) = forget {
            self.forget_root(game, &target_id);
        }
    }

    fn mods_section(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("MODS").small().color(theme::MUTED));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let add = ui.button("Add");
                let field = ui.add(
                    egui::TextEdit::singleline(&mut self.add_input)
                        .desired_width(320.0)
                        .hint_text("paste a GitHub repo, e.g. WeakAuras/WeakAuras2"),
                );
                if add.clicked()
                    || (field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)))
                {
                    self.do_add();
                }
            });
        });
        ui.add_space(6.0);

        if self.rows.is_empty() {
            egui::Frame::NONE
                .fill(theme::CARD)
                .corner_radius(6.0)
                .inner_margin(egui::Margin::symmetric(14, 12))
                .show(ui, |ui| {
                    ui.label("No mods yet.");
                    ui.add_space(4.0);
                    ui.label(
                        egui::RichText::new(
                            "Paste a GitHub repository above. Any mod that publishes its \
                             releases on GitHub works — owner/repo, or the full URL.",
                        )
                        .color(theme::MUTED),
                    );
                });
            return;
        }

        let mut toggle: Option<ModId> = None;
        let mut remove: Option<ModId> = None;

        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for row in &self.rows {
                    egui::Frame::NONE
                        .fill(theme::CARD)
                        .corner_radius(6.0)
                        .inner_margin(egui::Margin::symmetric(10, 7))
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                let mut enabled = row.enabled;
                                if ui
                                    .checkbox(&mut enabled, "")
                                    .on_hover_text("Include this mod when deploying")
                                    .changed()
                                {
                                    toggle = Some(row.id.clone());
                                }

                                let label = format!("{}/{}", row.id.owner, row.id.repo);
                                let text = if row.enabled {
                                    egui::RichText::new(label).color(theme::TEXT)
                                } else {
                                    egui::RichText::new(label)
                                        .color(theme::MUTED)
                                        .strikethrough()
                                };
                                ui.label(text).on_hover_text(row.id.web_url());

                                if row.pinned {
                                    ui.label(
                                        egui::RichText::new("pinned").small().color(theme::MUTED),
                                    );
                                }

                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        if ui.small_button("✕").on_hover_text("Remove").clicked() {
                                            remove = Some(row.id.clone());
                                        }
                                        if let Some(level) = row.trust {
                                            let mut tip = level.explain().to_string();
                                            if !row.note.is_empty() {
                                                tip.push_str("\n\n");
                                                tip.push_str(&row.note);
                                            }
                                            ui.label(
                                                egui::RichText::new(level.label())
                                                    .small()
                                                    .color(theme::trust_color(level)),
                                            )
                                            .on_hover_text(tip);
                                        }
                                        ui.label(
                                            egui::RichText::new(&row.version)
                                                .monospace()
                                                .color(theme::MUTED),
                                        );
                                    },
                                );
                            });
                        });
                    ui.add_space(3.0);
                }
            });

        if let Some(id) = toggle {
            self.do_toggle(&id);
        }
        if let Some(id) = remove {
            self.do_remove(&id);
        }
    }

    fn games_view(&mut self, ui: &mut egui::Ui) {
        ui.add_space(6.0);
        ui.heading("Games & folders");
        ui.label(
            egui::RichText::new(
                "Modifile looks for these on startup. Point it at anything it missed.",
            )
            .color(theme::MUTED),
        );
        ui.add_space(12.0);

        struct Entry {
            game: String,
            name: String,
            description: String,
            targets: Vec<(Target, Option<PathBuf>, bool)>,
        }

        let entries: Vec<Entry> = self
            .engine()
            .map(|engine| {
                engine
                    .packs
                    .iter()
                    .map(|pack| Entry {
                        game: pack.id().to_string(),
                        name: pack.pack.game.name.clone(),
                        description: pack.pack.game.description.clone(),
                        targets: pack
                            .pack
                            .targets
                            .iter()
                            .map(|t| {
                                let remembered = engine.roots.get(pack.id(), &t.id).cloned();
                                let found = remembered
                                    .clone()
                                    .filter(|p| p.is_dir())
                                    .or_else(|| pack.detect(t).into_iter().next());
                                (t.clone(), found, remembered.is_some())
                            })
                            .collect(),
                    })
                    .collect()
            })
            .unwrap_or_default();

        let mut choose: Option<(String, Target)> = None;
        let mut forget: Option<(String, String)> = None;

        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for entry in &entries {
                    egui::Frame::NONE
                        .fill(theme::CARD)
                        .corner_radius(8.0)
                        .inner_margin(egui::Margin::symmetric(12, 10))
                        .show(ui, |ui| {
                            ui.label(egui::RichText::new(&entry.name).strong());
                            if !entry.description.is_empty() {
                                let first = entry
                                    .description
                                    .lines()
                                    .next()
                                    .unwrap_or_default()
                                    .to_string();
                                ui.label(
                                    egui::RichText::new(first).small().color(theme::MUTED),
                                );
                            }
                            ui.add_space(6.0);

                            for (target, found, remembered) in &entry.targets {
                                ui.horizontal(|ui| {
                                    let badge = match target.kind {
                                        modifile_core::TargetKind::Server => "SERVER",
                                        modifile_core::TargetKind::Client => "CLIENT",
                                    };
                                    ui.label(
                                        egui::RichText::new(badge).small().color(theme::MUTED),
                                    );
                                    ui.label(&target.name);

                                    ui.with_layout(
                                        egui::Layout::right_to_left(egui::Align::Center),
                                        |ui| {
                                            if ui.small_button(if found.is_some() {
                                                "Change…"
                                            } else {
                                                "Choose folder…"
                                            })
                                            .clicked()
                                            {
                                                choose =
                                                    Some((entry.game.clone(), target.clone()));
                                            }
                                            if *remembered && ui.small_button("Forget").clicked() {
                                                forget = Some((
                                                    entry.game.clone(),
                                                    target.id.clone(),
                                                ));
                                            }
                                            match found {
                                                Some(path) => ui.label(
                                                    egui::RichText::new(display_path(path))
                                                        .small()
                                                        .color(theme::GOOD),
                                                ),
                                                None => ui.label(
                                                    egui::RichText::new("not found")
                                                        .small()
                                                        .color(theme::MUTED),
                                                ),
                                            };
                                        },
                                    );
                                });
                            }
                        });
                    ui.add_space(6.0);
                }

                ui.add_space(8.0);
                egui::Frame::NONE
                    .fill(theme::CARD)
                    .corner_radius(8.0)
                    .inner_margin(egui::Margin::symmetric(12, 10))
                    .show(ui, |ui| {
                        ui.label(egui::RichText::new("Add another game").strong());
                        ui.add_space(4.0);
                        ui.label(
                            egui::RichText::new(
                                "Support for a game is a single .toml file — no new version of \
                                 Modifile needed. Drop one in the packs folder and restart.",
                            )
                            .color(theme::MUTED),
                        );
                        ui.add_space(8.0);
                        if ui.button("Open packs folder").clicked() {
                            reveal(&self.paths.packs);
                        }
                    });

                if let Some(engine) = self.engine() {
                    for (path, error) in &engine.pack_errors {
                        ui.add_space(6.0);
                        ui.label(
                            egui::RichText::new(format!(
                                "ignored {}: {error}",
                                display_path(path)
                            ))
                            .color(theme::BAD),
                        );
                    }
                }
            });

        if let Some((game, target)) = choose {
            self.open_root_dialog(&game, &target);
        }
        if let Some((game, target_id)) = forget {
            self.forget_root(&game, &target_id);
        }
    }

    fn settings_view(&mut self, ui: &mut egui::Ui) {
        ui.add_space(6.0);
        ui.heading("Settings");
        ui.add_space(12.0);

        egui::Frame::NONE
            .fill(theme::CARD)
            .corner_radius(8.0)
            .inner_margin(egui::Margin::symmetric(12, 10))
            .show(ui, |ui| {
                ui.label(egui::RichText::new("GitHub token").strong());
                ui.add_space(4.0);
                ui.label(
                    egui::RichText::new(
                        "Optional, and free. Without one GitHub allows 60 update checks per \
                         hour for your whole machine; with one, 5000. It is only used for \
                         read-only requests.",
                    )
                    .color(theme::MUTED),
                );
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.token_input)
                            .desired_width(340.0)
                            .password(true)
                            .hint_text("ghp_…"),
                    );
                    if ui.button("Save").clicked() {
                        self.save_token();
                    }
                });
                ui.add_space(6.0);
                ui.hyperlink_to(
                    "Create one on GitHub (no scopes needed)",
                    "https://github.com/settings/tokens/new",
                );
            });

        ui.add_space(10.0);
        egui::Frame::NONE
            .fill(theme::CARD)
            .corner_radius(8.0)
            .inner_margin(egui::Margin::symmetric(12, 10))
            .show(ui, |ui| {
                ui.label(egui::RichText::new("Storage").strong());
                ui.add_space(4.0);
                let store = self
                    .engine()
                    .map(|e| e.store.size_bytes())
                    .unwrap_or_default();
                ui.label(
                    egui::RichText::new(format!(
                        "Downloads take {}. Profiles share them, so installing the same mod \
                         in five profiles still stores it once.",
                        format_bytes(store)
                    ))
                    .color(theme::MUTED),
                );
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui
                        .button("Clean up unused downloads")
                        .on_hover_text("Deletes downloads no profile refers to")
                        .clicked()
                    {
                        self.run_gc();
                    }
                    if ui.button("Open data folder").clicked() {
                        reveal(&self.paths.home);
                    }
                });
                ui.add_space(6.0);
                ui.label(
                    egui::RichText::new(display_path(&self.paths.home))
                        .small()
                        .color(theme::MUTED),
                );
            });
    }

    fn new_profile_window(&mut self, ctx: &egui::Context) {
        let mut open = self.show_new_profile;
        egui::Window::new("New profile")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .open(&mut open)
            .show(ctx, |ui| {
                ui.add_space(4.0);
                ui.label("Which game?");
                let packs: Vec<(String, String)> = self
                    .engine()
                    .map(|e| {
                        e.packs
                            .iter()
                            .map(|p| (p.id().to_string(), p.pack.game.name.clone()))
                            .collect()
                    })
                    .unwrap_or_default();
                let current = packs
                    .iter()
                    .find(|(id, _)| id == &self.new_profile_game)
                    .map(|(_, name)| name.clone())
                    .unwrap_or_else(|| "select a game".to_string());

                egui::ComboBox::from_id_salt("game")
                    .selected_text(current)
                    .width(280.0)
                    .show_ui(ui, |ui| {
                        for (id, name) in &packs {
                            ui.selectable_value(&mut self.new_profile_game, id.clone(), name);
                        }
                    });

                ui.add_space(10.0);
                ui.label("Name it");
                let field = ui.add(
                    egui::TextEdit::singleline(&mut self.new_profile_name)
                        .desired_width(280.0)
                        .hint_text("raiding, modded-server, vanilla-plus…"),
                );
                ui.add_space(4.0);
                ui.label(
                    egui::RichText::new(
                        "Profiles swap in seconds and share downloads, so make as many as you like.",
                    )
                    .small()
                    .color(theme::MUTED),
                );

                ui.add_space(14.0);
                ui.horizontal(|ui| {
                    if ui.button("Create").clicked()
                        || (field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)))
                    {
                        self.create_profile();
                    }
                    if ui.button("Cancel").clicked() {
                        self.show_new_profile = false;
                    }
                });
                ui.add_space(2.0);
            });

        if !open {
            self.show_new_profile = false;
        }
    }
}
